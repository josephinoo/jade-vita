pub mod ui;

use crate::gfn::auth::{self, AuthTokens, DeviceCodeChallenge, DevicePollOutcome, GfnUser};
use crate::gfn::catalog::{self, GameSummary};
use crate::gfn::cloudmatch::{self, SessionInfo};
use crate::gfn::covers::CoverStore;
use crate::gfn::signaling::{self, SignalingEvent, SignalingHandle};
use crate::input::{AppCommand, InputCommand};
use crate::jobs::{PollJob, poll_job};
use anyhow::Result;
use reqwest::Client;
use std::sync::Arc;
use std::time::Instant;
use tokio::task::JoinHandle;

/// What Confirm should retry from the `Error` screen.
pub enum ErrorRetry {
    RestartLogin,
    ReloadCatalog(GfnUser),
    BackToGameDetail {
        user: GfnUser,
        games: Vec<GameSummary>,
        selected: usize,
        filtered_indices: Vec<usize>,
        search_query: String,
        search_requested: bool,
        covers: CoverStore,
    },
}

/// How many game tiles fit side-by-side on the Vita's 960x544 display at our UI scale. Picked
/// empirically to leave room for legible titles under each cover without forcing horizontal
/// scrolling; exposed as a sibling module-level const so `move_in_grid` and the renderer in
/// `app::ui::catalog_screen` agree on the geometry.
pub(crate) const GRID_COLUMNS: usize = 4;

#[derive(Clone, Copy)]
enum GridStep {
    Up,
    Down,
    Left,
    Right,
}

/// Moves `selected` through the grid by one cell in `step`'s direction, clamping at the
/// edges/matrix bounds. Rows and columns match what the renderer draws (`GRID_COLUMNS` per
/// row); `Left`/`Right` wrap across the last row's ragged end without going out of range.
fn move_in_grid(len: usize, columns: usize, selected: usize, step: GridStep) -> usize {
    if len == 0 {
        return selected;
    }
    let max = len - 1;
    let next = match step {
        GridStep::Up => selected.saturating_sub(columns),
        GridStep::Down => (selected + columns).min(max),
        GridStep::Left => selected.saturating_sub(1),
        GridStep::Right => (selected + 1).min(max),
    };
    next
}

/// Returns the indices of `games` whose title contains `query` (case-insensitive). An empty
/// query returns all indices.
fn filter_indices(games: &[GameSummary], query: &str) -> Vec<usize> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return (0..games.len()).collect();
    }
    games
        .iter()
        .enumerate()
        .filter(|(_, game)| game.title.to_lowercase().contains(&query))
        .map(|(index, _)| index)
        .collect()
}

/// Top-level screen the shell is currently rendering.
pub enum AppState {
    /// Press Confirm to start the device-code flow.
    Login,
    /// `POST /device/authorize` in flight.
    StartingDeviceLogin(PollJob<DeviceCodeChallenge>),
    /// Waiting for the user to complete login on another device; polling `/token` on
    /// `challenge.interval`.
    WaitingForDeviceAuthorization {
        challenge: DeviceCodeChallenge,
        poll_job: Option<PollJob<DevicePollOutcome>>,
        next_poll_at: Instant,
    },
    /// Fetching the VPC id + browsable game catalog after a successful (or restored) login.
    LoadingCatalog {
        user: GfnUser,
        job: PollJob<Vec<GameSummary>>,
    },
    /// Fase 2 stops here - Fase 3 adds actually creating a streaming session for the selected
    /// game (`games[selected].app_id`) instead of just showing a placeholder note.
    Catalog {
        user: GfnUser,
        games: Vec<GameSummary>,
        /// `selected` indexes into `filtered_indices`, not directly into `games`.
        selected: usize,
        /// Indices into `games` that match the current `search_query`. Empty query = all games.
        filtered_indices: Vec<usize>,
        search_query: String,
        /// Set to `true` when the user presses the search button; the shell uses SDL's text input
        /// API to open the system/on-screen keyboard and feed the resulting text back via
        /// `AppCommand::SetSearchQuery`. Mirrors green-vita's `title_search_requested`.
        search_requested: bool,
        /// Shared cover-art cache: lazily filled by async download tasks spawned from the UI
        /// loop as tiles become visible (see `app::ui::catalog_screen`). The `Arc` lets the
        /// store outlive the originating `AppState::Catalog` - inflight downloads keep their
        /// last reference and complete into an orphaned map that gets GC'd when the task ends,
        /// with no risk of holding stale references in `App::state` after it transitions away
        /// from this screen.
        covers: CoverStore,
    },
    /// Detail view for a single game, opened from the catalog grid with Confirm (×).
    /// Keeps the same `games`/`selected`/`filtered_indices`/`search_query`/`search_requested`/
    /// `covers` so Back returns to the catalog without refetching anything and preserving scroll
    /// position / cover cache / search state.
    GameDetail {
        user: GfnUser,
        games: Vec<GameSummary>,
        selected: usize,
        filtered_indices: Vec<usize>,
        search_query: String,
        search_requested: bool,
        covers: CoverStore,
    },
    /// CloudMatch session creation + polling in progress. Spawned from `GameDetail` when the
    /// user presses Confirm to "launch" the selected game.
    CreatingSession {
        user: GfnUser,
        games: Vec<GameSummary>,
        selected: usize,
        filtered_indices: Vec<usize>,
        search_query: String,
        search_requested: bool,
        covers: CoverStore,
        job: PollJob<SessionInfo>,
        queue_tracker: cloudmatch::QueueProgressTracker,
    },
    /// CloudMatch session is ready. This is a debug/transition screen: it shows the resolved
    /// server IP, signaling server, and codec profile, and is the launchpad for the WebRTC
    /// signaling step.
    SessionReady {
        user: GfnUser,
        games: Vec<GameSummary>,
        selected: usize,
        filtered_indices: Vec<usize>,
        search_query: String,
        search_requested: bool,
        covers: CoverStore,
        session: SessionInfo,
    },
    /// Connected to the session's NVST signaling WebSocket. `offer_sdp` fills in once the server
    /// sends its offer; this is as far as Fase 3's signaling step goes today - actually building
    /// a `rtc` peer connection from that offer is the next commit.
    Signaling {
        user: GfnUser,
        games: Vec<GameSummary>,
        selected: usize,
        filtered_indices: Vec<usize>,
        search_query: String,
        search_requested: bool,
        covers: CoverStore,
        session: SessionInfo,
        handle: SignalingHandle,
        offer_sdp: Option<String>,
    },
    /// Active WebRTC video/audio streaming session state.
    Streaming {
        user: GfnUser,
        games: Vec<GameSummary>,
        selected: usize,
        filtered_indices: Vec<usize>,
        search_query: String,
        search_requested: bool,
        covers: CoverStore,
        session: SessionInfo,
        handle: SignalingHandle,
        peer: crate::gfn::peer::PeerEngine,
    },
    Error {
        message: String,
        retry: ErrorRetry,
    },
}

pub struct App {
    pub(crate) state: AppState,
    /// Used both for GFN REST/GraphQL calls (from the async `AppState` tasks below) and - via
    /// `app::ui::build_ui`, which also borrows `&App` - for the per-frame lazy cover-art
    /// download requests kicked off from the catalog grid renderer.
    pub(crate) http_client: Client,
    /// Set on every successful (or restored) login, cleared on sign-out. Fase 3's session
    /// creation will need this too, so it lives on `App` rather than threaded through every
    /// `AppState` variant that happens to run after login.
    tokens: Option<AuthTokens>,
    /// Debug readout of the last navigation command received, shown on the placeholder screen
    /// so input mapping can be sanity-checked on real hardware before there is anything else to
    /// look at.
    pub(crate) last_input: Option<InputCommand>,
    /// Transient one-line status message (e.g. "press Confirm on a game does X once Fase 3
    /// lands"), shown under the game list until the next input event replaces or clears it.
    pub(crate) status_note: Option<String>,
    /// Debounce/dispatch state for server-side catalog search. Deliberately kept on `App`
    /// instead of inside `AppState::Catalog` - the query text and its instant local pre-filter
    /// already live there (see `apply_search_query`), but threading this through would mean
    /// touching every one of that variant's many match arms just to move fields they don't care
    /// about. Cleared (and any in-flight job left to finish orphaned) whenever the current state
    /// isn't `AppState::Catalog` - see `advance_catalog_search`.
    search_job: Option<PollJob<Vec<GameSummary>>>,
    /// Set when the query changed and a debounced server search hasn't fired for it yet.
    search_pending_since: Option<Instant>,
    /// The last query a server search was actually dispatched for - avoids re-firing once the
    /// debounce elapses if the user hasn't typed anything new since.
    last_dispatched_search_query: Option<String>,
    pub(crate) confirm_exit: bool,
}

impl App {
    /// Returns the current Bearer token if the user is logged in.
    pub fn bearer_token(&self) -> Option<&str> {
        self.tokens.as_ref().map(|tokens| tokens.bearer())
    }

    pub fn new() -> Result<Self> {
        let http_client = auth::client();
        let tokens = auth::load_tokens();
        let state = match &tokens {
            Some(tokens) => match auth::user_from_tokens(tokens) {
                Ok(user) => Self::start_catalog_fetch(&http_client, tokens, user),
                Err(error) => {
                    eprintln!("Saved GFN login could not be decoded, clearing it: {error:#}");
                    auth::clear_tokens();
                    AppState::Login
                }
            },
            None => AppState::Login,
        };

        Ok(Self {
            state,
            http_client,
            tokens,
            last_input: None,
            status_note: None,
            search_job: None,
            search_pending_since: None,
            last_dispatched_search_query: None,
            confirm_exit: false,
        })
    }

    pub async fn handle_command(&mut self, command: AppCommand) -> Result<()> {
        // Snapshot these up front so the match arms can move `self` references freely without
        // holding a borrow across the state reassignment.
        let bearer_token = self.bearer_token().map(|s| s.to_owned());
        let http_client = self.http_client.clone();

        // Takes ownership of the current state up front rather than matching on `&mut
        // self.state` directly - some arms below need to both read out of the matched state
        // (e.g. `ReloadCatalog(user)`) and reassign `self.state`, which the borrow checker
        // won't allow through a live reference into the same field.
        let current_state = std::mem::replace(&mut self.state, AppState::Login);
        self.state = match command {
            AppCommand::SetSearchQuery(query) => {
                return self.apply_search_query(current_state, query);
            }
            AppCommand::RequestSearch => {
                return self.request_search(current_state);
            }
            AppCommand::CloseSearch => {
                return self.close_search(current_state);
            }
            AppCommand::ToggleConfirmExit => {
                self.confirm_exit = !self.confirm_exit;
                current_state
            }
            AppCommand::CancelConfirmExit => {
                self.confirm_exit = false;
                current_state
            }
            AppCommand::ConfirmExitSession => {
                self.confirm_exit = false;
                self.exit_session(current_state)?
            }
            AppCommand::Input(input) => {
                self.last_input = Some(input);
                self.handle_input_command(current_state, input, bearer_token, http_client)
                    .await?
            }
        };
        Ok(())
    }

    fn apply_search_query(&mut self, state: AppState, query: String) -> Result<()> {
        self.state = match state {
            AppState::Catalog {
                user,
                games,
                selected: _,
                filtered_indices: _,
                search_query: _,
                search_requested,
                covers,
            } => {
                let filtered_indices = filter_indices(&games, &query);
                // Reset selection to the first matching result whenever the query changes.
                let selected = 0;
                // Arms the debounce timer for a server-side search - see `advance_catalog_search`.
                // Cleared once that search actually dispatches, not here, so rapid keystrokes
                // keep pushing the timer back instead of firing one request per character.
                self.search_pending_since = Some(Instant::now());
                AppState::Catalog {
                    user,
                    games,
                    selected,
                    filtered_indices,
                    search_query: query,
                    search_requested,
                    covers,
                }
            }
            other => other,
        };
        Ok(())
    }

    /// Flip the `search_requested` flag to true so the shell can start the platform text-input
    /// method (SDL IME / on-screen keyboard). Reset to false once the query actually arrives via
    /// `SetSearchQuery`.
    fn request_search(&mut self, state: AppState) -> Result<()> {
        self.state = match state {
            AppState::Catalog {
                user,
                games,
                selected,
                filtered_indices,
                search_query,
                search_requested: _,
                covers,
            } => AppState::Catalog {
                user,
                games,
                selected,
                filtered_indices,
                search_query,
                search_requested: true,
                covers,
            },
            other => other,
        };
        Ok(())
    }

    fn close_search(&mut self, state: AppState) -> Result<()> {
        self.state = match state {
            AppState::Catalog {
                user,
                games,
                selected,
                filtered_indices,
                search_query,
                search_requested: _,
                covers,
            } => AppState::Catalog {
                user,
                games,
                selected,
                filtered_indices,
                search_query,
                search_requested: false,
                covers,
            },
            other => other,
        };
        Ok(())
    }

    fn exit_session(&mut self, state: AppState) -> Result<AppState> {
        let new_state = match state {
            AppState::CreatingSession {
                user,
                games,
                selected,
                filtered_indices,
                search_query,
                search_requested,
                covers,
                ..
            }
            | AppState::SessionReady {
                user,
                games,
                selected,
                filtered_indices,
                search_query,
                search_requested,
                covers,
                ..
            } => AppState::GameDetail {
                user,
                games,
                selected,
                filtered_indices,
                search_query,
                search_requested,
                covers,
            },
            AppState::Signaling {
                user,
                games,
                selected,
                filtered_indices,
                search_query,
                search_requested,
                covers,
                handle,
                ..
            }
            | AppState::Streaming {
                user,
                games,
                selected,
                filtered_indices,
                search_query,
                search_requested,
                covers,
                handle,
                ..
            } => {
                handle.close();
                AppState::GameDetail {
                    user,
                    games,
                    selected,
                    filtered_indices,
                    search_query,
                    search_requested,
                    covers,
                }
            }
            other => other,
        };
        Ok(new_state)
    }

    async fn handle_input_command(
        &mut self,
        current_state: AppState,
        input: InputCommand,
        bearer_token: Option<String>,
        http_client: Client,
    ) -> Result<AppState> {
        Ok(match (current_state, input) {
            (AppState::Login, InputCommand::Confirm) => self.start_login_state(),
            (AppState::WaitingForDeviceAuthorization { .. }, InputCommand::Back) => AppState::Login,
            (
                AppState::Catalog {
                    user,
                    games,
                    selected,
                    filtered_indices,
                    search_query,
                    search_requested,
                    covers,
                },
                InputCommand::MoveUp,
            ) => {
                if search_requested {
                    // While the system keyboard is open let the platform handle d-pad/stick input.
                    AppState::Catalog {
                        user,
                        games,
                        selected,
                        filtered_indices,
                        search_query,
                        search_requested,
                        covers,
                    }
                } else {
                    let selected =
                        move_in_grid(filtered_indices.len(), GRID_COLUMNS, selected, GridStep::Up);
                    AppState::Catalog {
                        user,
                        games,
                        selected,
                        filtered_indices,
                        search_query,
                        search_requested,
                        covers,
                    }
                }
            }
            (
                AppState::Catalog {
                    user,
                    games,
                    selected,
                    filtered_indices,
                    search_query,
                    search_requested,
                    covers,
                },
                InputCommand::MoveDown,
            ) => {
                if search_requested {
                    AppState::Catalog {
                        user,
                        games,
                        selected,
                        filtered_indices,
                        search_query,
                        search_requested,
                        covers,
                    }
                } else {
                    let selected = move_in_grid(
                        filtered_indices.len(),
                        GRID_COLUMNS,
                        selected,
                        GridStep::Down,
                    );
                    AppState::Catalog {
                        user,
                        games,
                        selected,
                        filtered_indices,
                        search_query,
                        search_requested,
                        covers,
                    }
                }
            }
            (
                AppState::Catalog {
                    user,
                    games,
                    selected,
                    filtered_indices,
                    search_query,
                    search_requested,
                    covers,
                },
                InputCommand::MoveLeft,
            ) => {
                if search_requested {
                    AppState::Catalog {
                        user,
                        games,
                        selected,
                        filtered_indices,
                        search_query,
                        search_requested,
                        covers,
                    }
                } else {
                    let selected = move_in_grid(
                        filtered_indices.len(),
                        GRID_COLUMNS,
                        selected,
                        GridStep::Left,
                    );
                    AppState::Catalog {
                        user,
                        games,
                        selected,
                        filtered_indices,
                        search_query,
                        search_requested,
                        covers,
                    }
                }
            }
            (
                AppState::Catalog {
                    user,
                    games,
                    selected,
                    filtered_indices,
                    search_query,
                    search_requested,
                    covers,
                },
                InputCommand::MoveRight,
            ) => {
                if search_requested {
                    AppState::Catalog {
                        user,
                        games,
                        selected,
                        filtered_indices,
                        search_query,
                        search_requested,
                        covers,
                    }
                } else {
                    let selected = move_in_grid(
                        filtered_indices.len(),
                        GRID_COLUMNS,
                        selected,
                        GridStep::Right,
                    );
                    AppState::Catalog {
                        user,
                        games,
                        selected,
                        filtered_indices,
                        search_query,
                        search_requested,
                        covers,
                    }
                }
            }
            (
                AppState::Catalog {
                    user,
                    games,
                    selected,
                    filtered_indices,
                    search_query,
                    search_requested,
                    covers,
                },
                InputCommand::Confirm,
            ) => {
                if search_requested {
                    // Close the system keyboard and return to grid navigation.
                    AppState::Catalog {
                        user,
                        games,
                        selected,
                        filtered_indices,
                        search_query,
                        search_requested: false,
                        covers,
                    }
                } else {
                    // Open the detail/info screen for the selected game.
                    AppState::GameDetail {
                        user,
                        games,
                        selected,
                        filtered_indices,
                        search_query,
                        search_requested,
                        covers,
                    }
                }
            }
            (
                AppState::Catalog {
                    user,
                    games,
                    selected,
                    filtered_indices,
                    search_query,
                    search_requested,
                    covers,
                },
                InputCommand::Back,
            ) => {
                if search_requested {
                    // Close the system keyboard without leaving the catalog.
                    AppState::Catalog {
                        user,
                        games,
                        selected,
                        filtered_indices,
                        search_query,
                        search_requested: false,
                        covers,
                    }
                } else {
                    // Back in the catalog no longer signs out immediately - too easy to hit by
                    // accident. Sign-out will live in a dedicated menu later.
                    AppState::Catalog {
                        user,
                        games,
                        selected,
                        filtered_indices,
                        search_query,
                        search_requested,
                        covers,
                    }
                }
            }
            (
                AppState::GameDetail {
                    user,
                    games,
                    selected,
                    filtered_indices,
                    search_query,
                    search_requested,
                    covers,
                },
                InputCommand::Back,
            ) => AppState::Catalog {
                user,
                games,
                selected,
                filtered_indices,
                search_query,
                search_requested,
                covers,
            },
            (
                state @ (AppState::CreatingSession { .. }
                | AppState::SessionReady { .. }
                | AppState::Signaling { .. }),
                InputCommand::Back,
            ) => {
                self.confirm_exit = !self.confirm_exit;
                state
            }
            (
                AppState::SessionReady {
                    user,
                    games,
                    selected,
                    filtered_indices,
                    search_query,
                    search_requested,
                    covers,
                    session,
                },
                InputCommand::Confirm,
            ) => match signaling::connect(&session.signaling_url, &session.session_id) {
                Ok(handle) => {
                    self.status_note =
                        Some("Conectando a la señalización de NVIDIA...".to_owned());
                    AppState::Signaling {
                        user,
                        games,
                        selected,
                        filtered_indices,
                        search_query,
                        search_requested,
                        covers,
                        session,
                        handle,
                        offer_sdp: None,
                    }
                }
                Err(error) => {
                    self.status_note =
                        Some(format!("No se pudo conectar la señalización: {error:#}"));
                    AppState::SessionReady {
                        user,
                        games,
                        selected,
                        filtered_indices,
                        search_query,
                        search_requested,
                        covers,
                        session,
                    }
                }
            }
            (
                AppState::GameDetail {
                    user,
                    games,
                    selected,
                    filtered_indices,
                    search_query,
                    search_requested,
                    covers,
                },
                InputCommand::Confirm,
            ) => {
                // Start the CloudMatch session creation flow for the selected game. This is the
                // first real Fase 3 step the user can exercise: it will hit NVIDIA's REST API and
                // poll until the session reports ready.
                let game_index = filtered_indices.get(selected).copied();
                match (
                    game_index.and_then(|index| games.get(index)),
                    bearer_token.clone(),
                ) {
                    (Some(game), Some(token)) => {
                        let app_id = game.app_id.clone();
                        let queue_tracker =
                            Arc::new(std::sync::Mutex::new(cloudmatch::QueueStatus::default()));
                        let tracker_clone = queue_tracker.clone();
                        let handle: JoinHandle<Result<SessionInfo>> = tokio::spawn(async move {
                            let settings = cloudmatch::StreamSettings::for_vita();
                            let session = cloudmatch::create_session(
                                &http_client,
                                cloudmatch::CreateSessionRequest {
                                    token: token.as_str(),
                                    app_id: &app_id,
                                    vpc_id: "", // VPC id is not required by the v2/session endpoint; serverInfo is optional for MVP.
                                    settings: &settings,
                                },
                            )
                            .await?;
                            cloudmatch::poll_session(
                                &http_client,
                                cloudmatch::PollSessionRequest {
                                    token: token.as_str(),
                                    session_id: &session.session_id,
                                    session: &session,
                                },
                                Some(tracker_clone),
                            )
                            .await
                        });
                        AppState::CreatingSession {
                            user,
                            games,
                            selected,
                            filtered_indices,
                            search_query,
                            search_requested,
                            covers,
                            job: PollJob::Pending(handle),
                            queue_tracker,
                        }
                    }
                    _ => {
                        self.status_note =
                            Some("No se pudo iniciar sesion: falta login o juego.".to_owned());
                        AppState::GameDetail {
                            user,
                            games,
                            selected,
                            filtered_indices,
                            search_query,
                            search_requested,
                            covers,
                        }
                    }
                }
            }
            (
                AppState::Error {
                    retry: ErrorRetry::RestartLogin,
                    ..
                },
                InputCommand::Confirm,
            ) => self.start_login_state(),
            (
                AppState::Error {
                    retry: ErrorRetry::ReloadCatalog(user),
                    ..
                },
                InputCommand::Confirm,
            ) => Self::start_catalog_fetch(
                &self.http_client,
                self.tokens.as_ref().expect("retry requires a saved login"),
                user,
            ),
            (
                AppState::Error {
                    retry:
                        ErrorRetry::BackToGameDetail {
                            user,
                            games,
                            selected,
                            filtered_indices,
                            search_query,
                            search_requested,
                            covers,
                        },
                    ..
                },
                InputCommand::Confirm,
            ) => AppState::GameDetail {
                user,
                games,
                selected,
                filtered_indices,
                search_query,
                search_requested,
                covers,
            },
            (AppState::Error { .. }, InputCommand::Back) => AppState::Login,
            (other, _) => other,
        })
    }

    fn start_login_state(&self) -> AppState {
        let client = self.http_client.clone();
        let handle: JoinHandle<Result<DeviceCodeChallenge>> =
            tokio::spawn(async move { auth::start_device_login(&client).await });
        AppState::StartingDeviceLogin(PollJob::Pending(handle))
    }

    fn start_catalog_fetch(client: &Client, tokens: &AuthTokens, user: GfnUser) -> AppState {
        let client = client.clone();
        let bearer = tokens.bearer().to_owned();
        let handle: JoinHandle<Result<Vec<GameSummary>>> =
            tokio::spawn(async move { catalog::fetch_catalog_for_account(&client, &bearer).await });
        AppState::LoadingCatalog {
            user,
            job: PollJob::Pending(handle),
        }
    }

    /// How long to wait after the last keystroke before actually hitting the network - matches
    /// a typical "search-as-you-type" debounce. Long enough that a fast typist doesn't fire one
    /// request per character, short enough that the full-catalog results still feel responsive.
    const SEARCH_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(400);

    /// Drives server-side catalog search: polls any in-flight search job to completion, then -
    /// once the debounce timer has elapsed for the current query and that query hasn't already
    /// been dispatched - fires a new one. A no-op whenever the current screen isn't
    /// `AppState::Catalog`.
    ///
    /// Complements (does not replace) the instant local `filter_indices` pre-filter in
    /// `apply_search_query`/`search_backspace`: those give immediate feedback within whatever
    /// page is already loaded, while this widens `games`/`filtered_indices` in place to the
    /// server's full-catalog match once it comes back - see `docs/protocol-notes.md`-adjacent
    /// comments on `catalog::search_catalog` for why a bigger local page isn't a substitute.
    async fn advance_catalog_search(&mut self) {
        let AppState::Catalog { search_query, .. } = &self.state else {
            self.search_job = None;
            self.search_pending_since = None;
            return;
        };
        let query = search_query.clone();

        if let Some(PollJob::Pending(handle)) = self.search_job.take() {
            match poll_job(handle).await {
                PollJob::Pending(handle) => {
                    self.search_job = Some(PollJob::Pending(handle));
                }
                PollJob::Done(Ok(games)) => {
                    let result_count = games.len();
                    if let AppState::Catalog {
                        games: state_games,
                        filtered_indices,
                        selected,
                        ..
                    } = &mut self.state
                    {
                        *filtered_indices = (0..games.len()).collect();
                        *state_games = games;
                        *selected = 0;
                    }
                    self.status_note = Some(format!("{result_count} resultado(s) para \"{query}\""));
                }
                PollJob::Done(Err(error)) => {
                    self.status_note = Some(format!("Búsqueda falló: {error:#}"));
                }
            }
            // Only ever one search in flight at a time - don't also consider dispatching a new
            // one in the same tick we just resolved this one.
            return;
        }

        let Some(pending_since) = self.search_pending_since else {
            return;
        };
        if pending_since.elapsed() < Self::SEARCH_DEBOUNCE {
            return;
        }
        if self.last_dispatched_search_query.as_deref() == Some(query.as_str()) {
            self.search_pending_since = None;
            return;
        }
        let Some(token) = self.bearer_token().map(str::to_owned) else {
            return;
        };

        self.search_pending_since = None;
        self.last_dispatched_search_query = Some(query.clone());
        let client = self.http_client.clone();
        let handle: JoinHandle<Result<Vec<GameSummary>>> = tokio::spawn(async move {
            if query.trim().is_empty() {
                catalog::fetch_catalog_for_account(&client, &token).await
            } else {
                catalog::search_catalog_for_account(&client, &token, &query).await
            }
        });
        self.search_job = Some(PollJob::Pending(handle));
    }

    /// Per-frame housekeeping: advances whatever async step is in flight. Kept out of the render
    /// closure (mirrors `green-vita`'s `App::tick`) so `build_ui` stays a pure function of the
    /// current state.
    pub async fn tick(&mut self) -> Result<()> {
        self.advance_catalog_search().await;
        match std::mem::replace(&mut self.state, AppState::Login) {
            AppState::StartingDeviceLogin(job) => self.state = self.advance_login_start(job).await,
            AppState::WaitingForDeviceAuthorization {
                challenge,
                poll_job: pending_poll,
                next_poll_at,
            } => {
                self.state = self
                    .advance_login_poll(challenge, pending_poll, next_poll_at)
                    .await
            }
            AppState::LoadingCatalog { user, job } => {
                self.state = Self::advance_catalog_load(user, job).await
            }
            AppState::CreatingSession {
                user,
                games,
                selected,
                filtered_indices,
                search_query,
                search_requested,
                covers,
                job,
                queue_tracker,
            } => {
                self.state = Self::advance_session_creation(
                    user,
                    games,
                    selected,
                    filtered_indices,
                    search_query,
                    search_requested,
                    covers,
                    job,
                    queue_tracker,
                )
                .await
            }
            AppState::Signaling {
                user,
                games,
                selected,
                filtered_indices,
                search_query,
                search_requested,
                covers,
                session,
                handle,
                offer_sdp,
            } => {
                self.state = self.advance_signaling(
                    user,
                    games,
                    selected,
                    filtered_indices,
                    search_query,
                    search_requested,
                    covers,
                    session,
                    handle,
                    offer_sdp,
                )
            }
            AppState::Streaming {
                user,
                games,
                selected,
                filtered_indices,
                search_query,
                search_requested,
                covers,
                session,
                mut handle,
                mut peer,
            } => {
                // Signaling stays alive during streaming: it still trickles NVIDIA's ICE
                // candidates (forwarded into the peer) and carries our answer out.
                while let Some(event) = handle.try_recv() {
                    match event {
                        SignalingEvent::RemoteIce(candidate) => {
                            peer.add_remote_ice(candidate);
                        }
                        SignalingEvent::Disconnected(reason) => {
                            eprintln!("signaling closed during streaming: {reason}");
                        }
                        _ => {}
                    }
                }
                while let Some(event) = peer.try_recv() {
                    match event {
                        crate::gfn::peer::PeerEvent::LocalAnswer { answer_sdp, nvst_sdp } => {
                            self.status_note =
                                Some("Answer SDP generado, enviado a NVIDIA...".to_owned());
                            handle.send_answer(answer_sdp, nvst_sdp);
                        }
                        crate::gfn::peer::PeerEvent::LocalIce(candidate) => {
                            handle.send_local_ice(candidate);
                        }
                        crate::gfn::peer::PeerEvent::Status(status) => {
                            self.status_note = Some(status);
                        }
                        crate::gfn::peer::PeerEvent::Connected => {
                            self.status_note = Some("Transmisión de vídeo en directo activa".to_owned());
                        }
                        crate::gfn::peer::PeerEvent::Error(err) => {
                            eprintln!("Streaming peer error: {err}");
                            self.status_note = Some(format!("Peer: {err}"));
                        }
                        crate::gfn::peer::PeerEvent::Disconnected(reason) => {
                            eprintln!("Streaming peer disconnected: {reason}");
                            self.status_note = Some(format!("Peer desconectado: {reason}"));
                        }
                    }
                }
                self.state = AppState::Streaming {
                    user,
                    games,
                    selected,
                    filtered_indices,
                    search_query,
                    search_requested,
                    covers,
                    session,
                    handle,
                    peer,
                };
            }
            other => self.state = other,
        }
        Ok(())
    }

    /// Drains a bounded number of signaling events per tick (rather than all of them) so a burst
    /// of trickled ICE candidates can't stall a single frame indefinitely.
    #[allow(clippy::too_many_arguments)]
    fn advance_signaling(
        &mut self,
        user: GfnUser,
        games: Vec<GameSummary>,
        selected: usize,
        filtered_indices: Vec<usize>,
        search_query: String,
        search_requested: bool,
        covers: CoverStore,
        session: SessionInfo,
        mut handle: SignalingHandle,
        mut offer_sdp: Option<String>,
    ) -> AppState {
        const MAX_EVENTS_PER_TICK: usize = 8;
        let mut disconnected_reason: Option<String> = None;

        for _ in 0..MAX_EVENTS_PER_TICK {
            match handle.try_recv() {
                Some(SignalingEvent::Connected) => {
                    self.status_note =
                        Some("Señalización conectada, esperando offer SDP...".to_owned());
                }
                Some(SignalingEvent::Offer(sdp)) => {
                    self.status_note = Some(format!(
                        "Offer SDP recibido ({} bytes). Negociando WebRTC...",
                        sdp.len()
                    ));
                    // The peer thread generates the real answer (and its NVST blob) and emits
                    // it as `PeerEvent::LocalAnswer`; `advance_streaming` forwards it through
                    // this same signaling handle. Any ICE candidates still queued behind the
                    // offer are drained next tick by the Streaming arm.
                    match crate::gfn::peer::PeerEngine::new(&sdp, &session) {
                        Ok(peer) => {
                            return AppState::Streaming {
                                user,
                                games,
                                selected,
                                filtered_indices,
                                search_query,
                                search_requested,
                                covers,
                                session,
                                handle,
                                peer,
                            };
                        }
                        Err(error) => {
                            eprintln!("failed to start peer engine: {error:#}");
                            offer_sdp = Some(sdp);
                        }
                    }
                }
                Some(SignalingEvent::RemoteIce(candidate)) => {
                    self.status_note = Some(format!(
                        "Candidato ICE remoto recibido de NVIDIA: {}",
                        candidate.candidate
                    ));
                }
                Some(SignalingEvent::Error(message)) => {
                    eprintln!("Signaling: {message}");
                }
                Some(SignalingEvent::Disconnected(reason)) => {
                    disconnected_reason = Some(reason);
                    break;
                }
                None => break,
            }
        }

        if let Some(reason) = disconnected_reason {
            return AppState::Error {
                message: format!("Señalización desconectada: {reason}"),
                retry: ErrorRetry::BackToGameDetail {
                    user,
                    games,
                    selected,
                    filtered_indices,
                    search_query,
                    search_requested,
                    covers,
                },
            };
        }

        AppState::Signaling {
            user,
            games,
            selected,
            filtered_indices,
            search_query,
            search_requested,
            covers,
            session,
            handle,
            offer_sdp,
        }
    }

    async fn advance_login_start(&self, job: PollJob<DeviceCodeChallenge>) -> AppState {
        let PollJob::Pending(handle) = job else {
            return AppState::Login;
        };
        match poll_job(handle).await {
            PollJob::Pending(handle) => AppState::StartingDeviceLogin(PollJob::Pending(handle)),
            PollJob::Done(Ok(challenge)) => AppState::WaitingForDeviceAuthorization {
                next_poll_at: Instant::now() + challenge.interval,
                challenge,
                poll_job: None,
            },
            PollJob::Done(Err(error)) => AppState::Error {
                message: format!("No se pudo iniciar sesión: {error:#}"),
                retry: ErrorRetry::RestartLogin,
            },
        }
    }

    async fn advance_login_poll(
        &mut self,
        challenge: DeviceCodeChallenge,
        pending_poll: Option<PollJob<DevicePollOutcome>>,
        next_poll_at: Instant,
    ) -> AppState {
        if challenge.is_expired() {
            return AppState::Error {
                message: "El código expiró antes de completar el login. Inténtalo de nuevo."
                    .to_owned(),
                retry: ErrorRetry::RestartLogin,
            };
        }

        let pending_poll = match pending_poll {
            Some(job) => Some(job),
            None if Instant::now() >= next_poll_at => {
                let client = self.http_client.clone();
                let challenge_for_task = challenge.clone();
                let handle: JoinHandle<Result<DevicePollOutcome>> = tokio::spawn(async move {
                    auth::poll_device_login(&client, &challenge_for_task).await
                });
                Some(PollJob::Pending(handle))
            }
            None => None,
        };

        let Some(job) = pending_poll else {
            return AppState::WaitingForDeviceAuthorization {
                challenge,
                poll_job: None,
                next_poll_at,
            };
        };

        let PollJob::Pending(handle) = job else {
            return AppState::WaitingForDeviceAuthorization {
                challenge,
                poll_job: None,
                next_poll_at,
            };
        };

        match poll_job(handle).await {
            PollJob::Pending(handle) => AppState::WaitingForDeviceAuthorization {
                challenge,
                poll_job: Some(PollJob::Pending(handle)),
                next_poll_at,
            },
            PollJob::Done(Ok(DevicePollOutcome::Pending)) => {
                AppState::WaitingForDeviceAuthorization {
                    next_poll_at: Instant::now() + challenge.interval,
                    challenge,
                    poll_job: None,
                }
            }
            PollJob::Done(Ok(DevicePollOutcome::SlowDown)) => {
                AppState::WaitingForDeviceAuthorization {
                    next_poll_at: Instant::now() + challenge.interval * 2,
                    challenge,
                    poll_job: None,
                }
            }
            PollJob::Done(Ok(DevicePollOutcome::Authorized(tokens))) => self.finish_login(tokens),
            PollJob::Done(Ok(DevicePollOutcome::Expired)) => AppState::Error {
                message: "El código expiró antes de completar el login. Inténtalo de nuevo."
                    .to_owned(),
                retry: ErrorRetry::RestartLogin,
            },
            PollJob::Done(Ok(DevicePollOutcome::Denied)) => AppState::Error {
                message: "Inicio de sesión rechazado.".to_owned(),
                retry: ErrorRetry::RestartLogin,
            },
            PollJob::Done(Err(error)) => AppState::Error {
                message: format!("Fallo comprobando el login: {error:#}"),
                retry: ErrorRetry::RestartLogin,
            },
        }
    }

    fn finish_login(&mut self, tokens: AuthTokens) -> AppState {
        if let Err(error) = auth::save_tokens(&tokens) {
            eprintln!("Could not persist GFN login: {error:#}");
        }
        let user = match auth::user_from_tokens(&tokens) {
            Ok(user) => user,
            Err(error) => {
                return AppState::Error {
                    message: format!("Login correcto pero no se pudo leer el perfil: {error:#}"),
                    retry: ErrorRetry::RestartLogin,
                };
            }
        };
        let state = Self::start_catalog_fetch(&self.http_client, &tokens, user);
        self.tokens = Some(tokens);
        state
    }

    async fn advance_catalog_load(user: GfnUser, job: PollJob<Vec<GameSummary>>) -> AppState {
        let PollJob::Pending(handle) = job else {
            return AppState::LoadingCatalog { user, job };
        };
        match poll_job(handle).await {
            PollJob::Pending(handle) => AppState::LoadingCatalog {
                user,
                job: PollJob::Pending(handle),
            },
            PollJob::Done(Ok(games)) => {
                let filtered_indices = filter_indices(&games, "");
                AppState::Catalog {
                    user,
                    games,
                    selected: 0,
                    filtered_indices,
                    search_query: String::new(),
                    search_requested: false,
                    covers: CoverStore::new(),
                }
            }
            PollJob::Done(Err(error)) => AppState::Error {
                message: format!("No se pudo cargar tu biblioteca de juegos: {error:#}"),
                retry: ErrorRetry::ReloadCatalog(user),
            },
        }
    }

    async fn advance_session_creation(
        user: GfnUser,
        games: Vec<GameSummary>,
        selected: usize,
        filtered_indices: Vec<usize>,
        search_query: String,
        search_requested: bool,
        covers: CoverStore,
        job: PollJob<SessionInfo>,
        queue_tracker: cloudmatch::QueueProgressTracker,
    ) -> AppState {
        let PollJob::Pending(handle) = job else {
            return AppState::CreatingSession {
                user,
                games,
                selected,
                filtered_indices,
                search_query,
                search_requested,
                covers,
                job,
                queue_tracker,
            };
        };
        match poll_job(handle).await {
            PollJob::Pending(handle) => AppState::CreatingSession {
                user,
                games,
                selected,
                filtered_indices,
                search_query,
                search_requested,
                covers,
                job: PollJob::Pending(handle),
                queue_tracker,
            },
            PollJob::Done(Ok(session)) => AppState::SessionReady {
                user,
                games,
                selected,
                filtered_indices,
                search_query,
                search_requested,
                covers,
                session,
            },
            PollJob::Done(Err(error)) => AppState::Error {
                message: format!("No se pudo crear la sesión de streaming: {error:#}"),
                retry: ErrorRetry::BackToGameDetail {
                    user,
                    games,
                    selected,
                    filtered_indices,
                    search_query,
                    search_requested,
                    covers,
                },
            },
        }
    }
}
