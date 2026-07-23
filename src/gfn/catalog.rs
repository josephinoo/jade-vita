//! Game catalog fetch - needed before a streaming session can exist at all, since CloudMatch's
//! `POST /v2/session` requires a numeric `appId` (see docs/protocol-notes.md §2). Fase 3 (actual
//! session creation/streaming) builds on top of `GameSummary::app_id` from here.
//!
//! Uses the same plain (non-persisted-query) GraphQL endpoint the desktop GFN client calls for
//! its own catalog browsing (`games.geforce.com/graphql`), not the LCARS CDN's persisted-query
//! endpoint used for marketing/marquee panels - see
//! `opennow-stable/src/main/gfn/games.ts::fetchPaginatedLibraryApps`/`browseCatalogUncached`.
//!
//! First version of this module filtered `apps()` down to the account's own "added to my GFN
//! library" list (`variants.gfn.library.status.notEquals: "NOT_OWNED"`), matching the reference
//! client's own `fetchLibraryGames`. On a real account that turned out to return zero results:
//! "added to library" is a separate, opt-in concept from "launchable on GFN" - most people never
//! explicitly add anything and just search/launch directly. `browseCatalogUncached` in the
//! reference client passes an **empty** `filters: {}` when the user hasn't picked any browse
//! filter, which browses the whole live catalog instead - that's what this fetches now.
//!
//! This is a deliberate simplification of that reference: no pagination, no per-title artwork,
//! no search box, no genre/filter UI - just enough to list launchable titles and their `appId`.

use super::headers::{self, error_for_status_with_body};
use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;

const GRAPHQL_ENDPOINT: &str = "https://games.geforce.com/graphql";
const CLOUDMATCH_BASE_URL: &str = "https://prod.cloudmatchbeta.nvidiagrid.net/";
const LOCALE: &str = "en_US";
/// Same default sort `browseCatalogUncached` falls back to when nothing more specific applies.
const CATALOG_SORT: &str = "itemMetadata.relevance:DESC,sortName:ASC";
/// Simplification vs. the official client: fetches a single page instead of doing real
/// pagination (`pageInfo.hasNextPage`/`endCursor`). 200 matches the reference client's own
/// `LIBRARY_FETCH_COUNT`. A larger value (2000 was tried) gets the request rejected outright
/// with an HTTP 400 - the server validates `first` against some undocumented maximum, so this is
/// not just a slower/heavier request, it's a hard ceiling. Consequence: the in-app search box
/// can only find titles within this first alphabetical page (up to roughly the "H" section) -
/// reaching the full catalog needs either real `endCursor` pagination or switching to the
/// server-side `searchQuery` argument instead of client-side filtering. Neither is implemented
/// yet; flagged here so it isn't mistaken for an oversight later.
const FETCH_COUNT: u32 = 200;

#[derive(Debug, Clone)]
pub struct GameSummary {
    pub app_id: String,
    pub title: String,
    /// Best-effort poster-style cover URL (portrait box art). `None` if the catalog response
    /// carried no image fields at all - the grid just draws a placeholder tile then.
    pub cover_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ServerInfoResponse {
    #[serde(rename = "requestStatus")]
    request_status: ServerInfoRequestStatus,
}

#[derive(Debug, Deserialize)]
struct ServerInfoRequestStatus {
    #[serde(rename = "serverId")]
    server_id: Option<String>,
}

/// The "VPC id" CloudMatch expects on catalog/session calls - not documented anywhere beyond
/// `requestStatus.serverId` showing up in `serverInfo` responses (see protocol notes §2).
pub async fn fetch_vpc_id(client: &Client, token: &str) -> Result<String> {
    let response = headers::apply_lcars_headers(
        client.get(format!("{CLOUDMATCH_BASE_URL}v2/serverInfo")),
        token,
        "WEBRTC",
    )
    .send()
    .await
    .context("serverInfo request failed")?;
    let response = error_for_status_with_body(response).await?;

    let payload: ServerInfoResponse = response
        .json()
        .await
        .context("failed to decode serverInfo response")?;
    payload
        .request_status
        .server_id
        .context("serverInfo response did not include a VPC id")
}

#[derive(Debug, Deserialize)]
struct GraphQlEnvelope<T> {
    data: Option<T>,
    #[serde(default)]
    errors: Option<Vec<GraphQlError>>,
}

#[derive(Debug, Deserialize)]
struct GraphQlError {
    message: String,
}

#[derive(Debug, Deserialize)]
struct CatalogData {
    apps: CatalogApps,
}

#[derive(Debug, Deserialize)]
struct CatalogApps {
    items: Vec<CatalogAppItem>,
}

#[derive(Debug, Deserialize)]
struct CatalogAppItem {
    id: String,
    title: String,
    #[serde(default)]
    variants: Vec<CatalogAppVariant>,
    /// Mirrors the field shape the official client requests (`games.ts` line 1014:
    /// `images { ... KEY_ART KEY_IMAGE GAME_BOX_ART ... }`). Each value is either a single
    /// URL string or an array of URL strings (depending on the image kind); we capture both
    /// shapes and pick the first non-empty entry. Missing entirely if the catalog entry has
    /// no artwork published.
    #[serde(default)]
    images: Option<CatalogAppImages>,
}

#[derive(Debug, Deserialize)]
struct CatalogAppVariant {
    id: String,
}

#[derive(Debug, Deserialize, Default)]
struct CatalogAppImages {
    /// Box art (portrait poster) - preferred for grid covers.
    #[serde(default, rename = "GAME_BOX_ART")]
    game_box_art: ImageField,
    /// Square key image - second preference (some titles ship only this).
    #[serde(default, rename = "KEY_IMAGE")]
    key_image: ImageField,
    /// Wide key art - third preference (latest fallback).
    #[serde(default, rename = "KEY_ART")]
    key_art: ImageField,
}

/// Catalog image values arrive as either a single URL (`"..."`) or an array (`["...", ...]`);
/// `ImageField` accepts both generously and exposes a `first()` accessor.
#[derive(Debug, Default, Deserialize)]
#[serde(untagged)]
enum ImageField {
    #[default]
    Empty,
    Single(String),
    Many(Vec<String>),
}

impl ImageField {
    fn first(&self) -> Option<&str> {
        match self {
            ImageField::Empty => None,
            ImageField::Single(s) => Some(s.as_str()),
            ImageField::Many(list) => list.first().map(|s| s.as_str()),
        }
    }
}

impl CatalogAppImages {
    /// Same preference order as OpenNOW's `POSTER_IMAGE_KEYS` (`games.ts` line 382):
    /// GAME_BOX_ART > KEY_IMAGE > KEY_ART.
    fn poster_url(&self) -> Option<String> {
        self.game_box_art
            .first()
            .or_else(|| self.key_image.first())
            .or_else(|| self.key_art.first())
            .map(|url| optimize_image(url))
    }
}

/// NVIDIA's `img.nvidiagrid.net` CDN accepts URL-fragment suffixes like `;f=jpeg;w=300` to
/// transcode/resize on the fly (see `optimizeImage` in `games.ts` line 384). The official
/// client asks for `f=webp`, which our decoder doesn't support - we ask for `jpeg` instead,
/// which the same CDN happily serves. Non-nvidiagrid URLs (rare for catalog covers) are
/// returned as-is.
fn optimize_image(url: &str) -> String {
    if url.contains("img.nvidiagrid.net") {
        format!("{url};f=jpeg;w=256")
    } else {
        url.to_owned()
    }
}

const CATALOG_QUERY: &str = r#"
query GetCatalogApps(
  $vpcId: String!,
  $locale: String!,
  $sortString: String!,
  $fetchCount: Int!,
  $filters: AppFilterFields!
) {
  apps(vpcId: $vpcId, language: $locale, orderBy: $sortString, first: $fetchCount, filters: $filters) {
    items {
      id
      title
      variants { id }
      images { GAME_BOX_ART KEY_IMAGE KEY_ART }
    }
  }
}
"#;

/// Same shape as `CATALOG_QUERY` plus the `searchQuery` argument - matches the reference
/// client's `GetSearchFilterResults` (`games.ts`). Passing the search term to the server instead
/// of filtering `CATALOG_QUERY`'s results locally is what lets search reach the *entire* live
/// catalog rather than only whatever fits in one `FETCH_COUNT`-sized page (see module docs and
/// the `FETCH_COUNT` comment on why a bigger local page isn't a viable alternative).
const CATALOG_SEARCH_QUERY: &str = r#"
query GetCatalogSearchApps(
  $vpcId: String!,
  $locale: String!,
  $sortString: String!,
  $fetchCount: Int!,
  $searchString: String!,
  $filters: AppFilterFields!
) {
  apps(vpcId: $vpcId, language: $locale, orderBy: $sortString, first: $fetchCount, searchQuery: $searchString, filters: $filters) {
    items {
      id
      title
      variants { id }
      images { GAME_BOX_ART KEY_IMAGE KEY_ART }
    }
  }
}
"#;

pub async fn fetch_catalog(client: &Client, token: &str, vpc_id: &str) -> Result<Vec<GameSummary>> {
    let body = json!({
        "query": CATALOG_QUERY,
        "variables": {
            "vpcId": vpc_id,
            "locale": LOCALE,
            "sortString": CATALOG_SORT,
            "fetchCount": FETCH_COUNT,
            // Empty on purpose - see module docs. A non-empty filter here narrows to a specific
            // genre/store/etc, which is what the reference client's filter UI builds up; we have
            // no such UI yet.
            "filters": {},
        },
    });
    run_catalog_query(client, token, body, "catalog").await
}

/// Server-side search across the whole GFN catalog for `query`. Empty `query` behaves like
/// `fetch_catalog` on the server's end, but callers should just call `fetch_catalog` directly in
/// that case - this function always takes the (slightly heavier) search code path.
pub async fn search_catalog(
    client: &Client,
    token: &str,
    vpc_id: &str,
    query: &str,
) -> Result<Vec<GameSummary>> {
    let body = json!({
        "query": CATALOG_SEARCH_QUERY,
        "variables": {
            "vpcId": vpc_id,
            "locale": LOCALE,
            "sortString": CATALOG_SORT,
            "fetchCount": FETCH_COUNT,
            "searchString": query,
            "filters": {},
        },
    });
    run_catalog_query(client, token, body, "catalog search").await
}

async fn run_catalog_query(
    client: &Client,
    token: &str,
    body: serde_json::Value,
    context_label: &str,
) -> Result<Vec<GameSummary>> {
    let response = headers::apply_graphql_headers(client.post(GRAPHQL_ENDPOINT), token)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("{context_label} GraphQL request failed"))?;
    let response = error_for_status_with_body(response).await?;

    let envelope: GraphQlEnvelope<CatalogData> = response
        .json()
        .await
        .with_context(|| format!("failed to decode {context_label} GraphQL response"))?;

    if let Some(errors) = envelope.errors.filter(|errors| !errors.is_empty()) {
        bail!(
            "{context_label} GraphQL errors: {}",
            errors
                .into_iter()
                .map(|error| error.message)
                .collect::<Vec<_>>()
                .join("; ")
        );
    }

    let data = envelope
        .data
        .with_context(|| format!("{context_label} GraphQL response had no data"))?;
    Ok(data
        .apps
        .items
        .into_iter()
        .map(|item| {
            let numeric_app_id = item
                .variants
                .iter()
                .find(|v| v.id.chars().all(|c| c.is_ascii_digit()))
                .map(|v| v.id.clone())
                .or_else(|| {
                    if item.id.chars().all(|c| c.is_ascii_digit()) {
                        Some(item.id.clone())
                    } else {
                        item.variants.first().map(|v| v.id.clone())
                    }
                })
                .unwrap_or(item.id);

            GameSummary {
                cover_url: item.images.as_ref().and_then(|images| images.poster_url()),
                app_id: numeric_app_id,
                title: item.title,
            }
        })
        .collect())
}

/// Fetches the VPC id and then the catalog in one call - the two requests every caller needs
/// together (there is no reason to ever want one without the other).
pub async fn fetch_catalog_for_account(client: &Client, token: &str) -> Result<Vec<GameSummary>> {
    let vpc_id = fetch_vpc_id(client, token).await?;
    fetch_catalog(client, token, &vpc_id).await
}

/// Fetches the VPC id and then runs a server-side search in one call, mirroring
/// `fetch_catalog_for_account`.
pub async fn search_catalog_for_account(
    client: &Client,
    token: &str,
    query: &str,
) -> Result<Vec<GameSummary>> {
    let vpc_id = fetch_vpc_id(client, token).await?;
    search_catalog(client, token, &vpc_id, query).await
}
