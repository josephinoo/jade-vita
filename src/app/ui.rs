use super::{App, AppState, GRID_COLUMNS};
use crate::gfn::catalog::GameSummary;
use crate::gfn::covers::{CoverSnapshot, CoverStore};
use crate::input::AppCommand;
use reqwest::Client;
use std::sync::Arc;

/// Builds the egui UI for the current frame and returns any commands produced by widget
/// interaction (buttons etc.) so the caller can feed them back through
/// `App::handle_command` - mirrors `green-vita`'s `build_ui` shape.
pub fn build_ui(ctx: &egui::Context, app: &App) -> Vec<AppCommand> {
    let mut commands = Vec::new();

    match &app.state {
        AppState::Login => login_screen(ctx, app),
        AppState::StartingDeviceLogin(_) => starting_login_screen(ctx),
        AppState::WaitingForDeviceAuthorization { challenge, .. } => {
            device_code_screen(ctx, challenge)
        }
        AppState::LoadingCatalog { user, .. } => loading_catalog_screen(ctx, user),
        AppState::Catalog {
            user,
            games,
            selected,
            filtered_indices,
            search_query,
            search_requested,
            covers,
        } => {
            if let Some(command) = catalog_screen(
                ctx,
                user,
                games,
                *selected,
                filtered_indices,
                search_query,
                *search_requested,
                covers,
                &app.http_client,
                app.status_note.as_deref(),
            ) {
                commands.push(command);
            }
        }
        AppState::GameDetail {
            user,
            games,
            selected,
            filtered_indices,
            search_query,
            search_requested,
            covers,
        } => game_detail_screen(
            ctx,
            user,
            games,
            *selected,
            filtered_indices,
            search_query,
            *search_requested,
            covers,
            &app.http_client,
            app.status_note.as_deref(),
        ),
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
            let queue_status = queue_tracker
                .lock()
                .map(|st| st.clone())
                .unwrap_or_default();
            if let Some(cmd) = creating_session_screen(
                ctx,
                user,
                games,
                *selected,
                filtered_indices,
                search_query,
                *search_requested,
                covers,
                job.is_pending(),
                &queue_status,
                app.status_note.as_deref(),
            ) {
                commands.push(cmd);
            }
        }
        AppState::SessionReady {
            user,
            games,
            selected,
            filtered_indices,
            search_query,
            search_requested,
            covers,
            session,
        } => {
            if let Some(cmd) = session_ready_screen(
                ctx,
                user,
                games,
                *selected,
                filtered_indices,
                search_query,
                *search_requested,
                covers,
                session,
                &app.http_client,
                app.status_note.as_deref(),
            ) {
                commands.push(cmd);
            }
        }
        AppState::Signaling {
            user,
            games,
            selected,
            session,
            offer_sdp,
            ..
        } => {
            if let Some(cmd) = signaling_screen(
                ctx,
                user,
                games,
                *selected,
                session,
                offer_sdp.as_deref(),
                app.status_note.as_deref(),
            ) {
                commands.push(cmd);
            }
        }
        AppState::Streaming {
            user,
            games,
            selected,
            peer,
            ..
        } => {
            if let Some(cmd) = streaming_screen(
                ctx,
                user,
                games,
                *selected,
                peer.video_frame().is_some(),
                app.status_note.as_deref(),
            ) {
                commands.push(cmd);
            }
        }
        AppState::Error { message, .. } => error_screen(ctx, message),
    }

    if app.confirm_exit {
        if let Some(cmd) = confirm_exit_modal(ctx) {
            commands.push(cmd);
        }
    }

    commands
}

fn login_screen(ctx: &egui::Context, app: &App) {
    egui::CentralPanel::default().show(ctx, |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space(80.0);
            ui.heading("Jade Vita");
            ui.label("Cliente no oficial de GeForce NOW para PS Vita");
            ui.add_space(24.0);
            ui.label("Pulsa Confirmar (✕) para iniciar sesión con tu cuenta de NVIDIA.");
            ui.add_space(24.0);
            if let Some(last_input) = app.last_input {
                ui.weak(format!("Última entrada detectada: {last_input:?}"));
            }
        });
    });
}

fn starting_login_screen(ctx: &egui::Context) {
    egui::CentralPanel::default().show(ctx, |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space(120.0);
            ui.spinner();
            ui.add_space(12.0);
            ui.label("Solicitando código de acceso a NVIDIA...");
        });
    });
}

fn device_code_screen(ctx: &egui::Context, challenge: &crate::gfn::auth::DeviceCodeChallenge) {
    egui::CentralPanel::default().show(ctx, |ui| {
        ui.add_space(24.0);
        ui.vertical_centered(|ui| {
            ui.heading("Inicia sesión en otro dispositivo");
        });
        ui.add_space(16.0);
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.set_width(ui.available_width() - 220.0);
                ui.label("1. Abre esta dirección en el navegador de tu teléfono u ordenador:");
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(&challenge.verification_uri_complete)
                        .monospace()
                        .strong(),
                );
                ui.add_space(20.0);
                ui.label("2. O escanea el código QR e introduce este código:");
                ui.add_space(12.0);
                egui::Frame::NONE
                    .fill(egui::Color32::from_rgb(0x26, 0x27, 0x2c))
                    .corner_radius(12.0)
                    .inner_margin(egui::Margin::symmetric(28, 20))
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new(&challenge.user_code)
                                .size(48.0)
                                .monospace()
                                .strong(),
                        );
                    });
                ui.add_space(20.0);
                ui.label("Esperando a que completes el login... (Atrás para cancelar)");
            });

            ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                draw_qr(ui, &challenge.verification_uri_complete, 200.0);
            });
        });
    });
}

fn loading_catalog_screen(ctx: &egui::Context, user: &crate::gfn::auth::GfnUser) {
    egui::CentralPanel::default().show(ctx, |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space(80.0);
            ui.heading(format!("Bienvenido, {}", user.display_name));
            ui.add_space(20.0);
            ui.spinner();
            ui.add_space(12.0);
            ui.label("Cargando el catálogo de GeForce NOW...");
        });
    });
}

fn catalog_screen(
    ctx: &egui::Context,
    user: &crate::gfn::auth::GfnUser,
    games: &[GameSummary],
    selected: usize,
    filtered_indices: &[usize],
    search_query: &str,
    search_requested: bool,
    covers: &CoverStore,
    http_client: &Client,
    status_note: Option<&str>,
) -> Option<AppCommand> {
    let mut search_command: Option<AppCommand> = None;
    let panel_frame = egui::Frame::NONE;
    egui::CentralPanel::default()
        .frame(panel_frame)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Catálogo de GeForce NOW");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.vertical(|ui| {
                        ui.label(&user.display_name);
                        if let Some(email) = &user.email {
                            ui.weak(email);
                        }
                    });
                });
            });
            ui.separator();

            // Search box. On the Vita the user will typically need a system keyboard or
            // touch input to edit it; the d-pad still moves the grid selection when the box
            // is not focused.
            let mut query = search_query.to_owned();
            ui.horizontal(|ui| {
                ui.label("Buscar:");
                let response = ui.add(
                    egui::TextEdit::singleline(&mut query)
                        .hint_text("escribe el titulo...")
                        .desired_width(300.0),
                );
                if search_requested && !response.has_focus() {
                    response.request_focus();
                }
                if response.gained_focus() && !search_requested {
                    search_command = Some(AppCommand::RequestSearch);
                }
                if response.changed() {
                    search_command = Some(AppCommand::SetSearchQuery(query));
                }
                let enter_pressed = ui.input(|i| i.key_pressed(egui::Key::Enter));
                if ui.button("Buscar").clicked() || enter_pressed || (search_requested && response.lost_focus()) {
                    search_command = Some(AppCommand::CloseSearch);
                }
            });
            ui.separator();

            ui.label(format!(
                "Mostrando {} de {} juegos",
                filtered_indices.len(),
                games.len()
            ));

            if filtered_indices.is_empty() {
                ui.add_space(40.0);
                ui.vertical_centered(|ui| {
                    if games.is_empty() {
                        ui.label("No se encontraron juegos disponibles (la API no devolvió ninguno).");
                    } else {
                        ui.label("Ningun juego coincide con la busqueda.");
                    }
                });
            } else {
                let columns = GRID_COLUMNS;
                let tile_width = 176.0;
                let cover_height = 220.0;
                let row_spacing = 8.0;
                let col_spacing = 8.0;
                let row_height = cover_height + 24.0; // title + margins
                let total_rows = filtered_indices.len().div_ceil(columns);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show_rows(ui, row_height, total_rows, |ui, row_range| {
                        let mut selected_response: Option<egui::Response> = None;
                        for row in row_range {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = col_spacing;
                                for column in 0..columns {
                                    let filtered_index = row * columns + column;
                                    let Some(&game_index) = filtered_indices.get(filtered_index) else {
                                        ui.add_space(tile_width);
                                        continue;
                                    };
                                    let game = &games[game_index];
                                    let tile_response = draw_tile(
                                        ui,
                                        ctx,
                                        http_client,
                                        covers,
                                        game,
                                        filtered_index == selected,
                                    );
                                    if filtered_index == selected {
                                        selected_response = Some(tile_response);
                                    }
                                }
                            });
                            ui.add_space(row_spacing);
                        }
                        // D-pad / stick navigation only updates `selected`; this makes the
                        // scroll area follow the active tile so it stays visible without the
                        // user having to drag the touchscreen.
                        if let Some(response) = selected_response {
                            response.scroll_to_me(Some(egui::Align::Center));
                        }
                    });
            }

            ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                ui.add_space(6.0);
                if let Some(note) = status_note {
                    ui.label(egui::RichText::new(note).italics());
                }
                ui.label("Arriba/Abajo/Izquierda/Derecha para navegar · Confirmar (X) para ver detalle · Atras (O) para volver");
                ui.separator();
            });
        });
    search_command
}

/// Draws one grid tile: cover thumbnail (or placeholder while still loading/failed/missing) +
/// title underneath. Side-effect: kicks off a background download for `game.cover_url` the
/// first time this tile is laid out - that's what makes covers appear "as the user scrolls"
/// instead of upfront for all ~1500 catalog entries.
fn draw_tile(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    http_client: &Client,
    covers: &CoverStore,
    game: &GameSummary,
    selected: bool,
) -> egui::Response {
    // Tuned so 4 columns × 176pt + 3 × 8pt item spacing ≈ 728pt - matches the ~738pt usable
    // width of the Vita's 960px display at UI_SCALE=1.3, so the grid fills the screen instead
    // of clustering in its left half.
    let tile_width = 176.0;
    // Portrait slot (4:5) - matches the typical box-art aspect so covers fill the frame
    // without letterboxing.
    let cover_height = 220.0;

    let response = ui.vertical(|ui| {
        ui.set_width(tile_width);

        let (rect, _slot_response) =
            ui.allocate_exact_size(egui::vec2(tile_width, cover_height), egui::Sense::click());
        let painter = ui.painter_at(rect);

        // Very obvious selection: bright blue background glow behind the cover when selected.
        if selected {
            let bg = rect.expand(10.0);
            painter.rect_filled(
                bg,
                12.0,
                egui::Color32::from_rgba_premultiplied(0x4d, 0xa8, 0xff, 60),
            );
        }

        let tile_frame_color = if selected {
            egui::Color32::from_rgb(0x6c, 0xc4, 0xff)
        } else {
            egui::Color32::from_rgb(0x3a, 0x3c, 0x44)
        };
        painter.rect_filled(rect, 8.0, egui::Color32::from_rgb(0x1a, 0x1c, 0x22));
        painter.rect_stroke(
            rect,
            8.0,
            egui::Stroke::new(3.0_f32, tile_frame_color),
            egui::StrokeKind::Inside,
        );
        // Draw a small "▶" indicator in the top-left corner so selection is unambiguous even on
        // covers with similar colors to the blue border.
        if selected {
            painter.text(
                rect.left_top() + egui::vec2(8.0, 6.0),
                egui::Align2::LEFT_TOP,
                "▶",
                egui::FontId::proportional(18.0),
                egui::Color32::WHITE,
            );
        }

        let copy = match covers.get(&game.app_id) {
            Some(CoverSnapshot::Ready(image)) => Some((image, ctx)),
            Some(CoverSnapshot::Loading | CoverSnapshot::Failed) | None => {
                if let Some(url) = game.cover_url.clone() {
                    covers.request(http_client, ctx, game.app_id.clone(), url);
                }
                None
            }
        };
        match copy {
            Some((image, ctx)) => {
                let tex = image.texture(ctx, &format!("gfn_cover_{}", game.app_id));
                let tex_size = tex.size_vec2();
                let src_aspect = tex_size.x / tex_size.y.max(1.0);
                let slot_aspect = tile_width / cover_height;
                // Aspect-fit: smaller of width-fit / height-fit, so the whole image is visible
                // inside the portrait slot. For portrait box art this fills nearly the entire
                // slot; for landscape covers it leaves small horizontal bands.
                let (draw_width, draw_height) = if src_aspect > slot_aspect {
                    let draw_width = tile_width;
                    let draw_height = tile_width / src_aspect;
                    (draw_width, draw_height)
                } else {
                    let draw_height = cover_height;
                    let draw_width = cover_height * src_aspect;
                    (draw_width, draw_height)
                };
                let x = rect.center().x - draw_width / 2.0;
                let y = rect.center().y - draw_height / 2.0;
                let draw_rect = egui::Rect::from_min_size(
                    egui::pos2(x, y),
                    egui::vec2(draw_width, draw_height),
                );
                painter.image(
                    tex.id(),
                    draw_rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            }
            None => {
                let initial = game.title.chars().next().unwrap_or('?');
                painter.text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    initial.to_string(),
                    egui::FontId::proportional(48.0),
                    egui::Color32::from_rgb(0xa0, 0xa4, 0xac),
                );
            }
        }

        let title_text = egui::RichText::new(&game.title).size(12.0);
        let title_text = if selected {
            title_text.strong().color(egui::Color32::WHITE)
        } else {
            title_text.color(egui::Color32::from_rgb(0xb0, 0xb4, 0xbc))
        };
        ui.add(egui::Label::new(title_text).truncate());
    });

    if selected {
        // Thick outer halo around the whole tile (cover + title) so selection pops even more.
        ui.painter().rect_stroke(
            response.response.rect,
            10.0,
            egui::Stroke::new(4.0_f32, egui::Color32::from_rgb(0x6c, 0xc4, 0xff)),
            egui::StrokeKind::Outside,
        );
    }

    response.response
}

fn game_detail_screen(
    ctx: &egui::Context,
    user: &crate::gfn::auth::GfnUser,
    games: &[GameSummary],
    selected: usize,
    _filtered_indices: &[usize],
    _search_query: &str,
    _search_requested: bool,
    covers: &CoverStore,
    http_client: &Client,
    status_note: Option<&str>,
) {
    let Some(game) = games.get(selected) else {
        // Should never happen, but render a graceful fallback instead of panicking.
        return error_screen(ctx, "No se encontró el juego seleccionado.");
    };

    // Kick off a cover download if we don't have it yet; the detail screen needs a big cover.
    if let Some(url) = game.cover_url.clone() {
        covers.request(http_client, ctx, game.app_id.clone(), url);
    }

    egui::CentralPanel::default().show(ctx, |ui| {
        ui.horizontal(|ui| {
            ui.heading("Detalle del juego");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.vertical(|ui| {
                    ui.label(&user.display_name);
                    if let Some(email) = &user.email {
                        ui.weak(email);
                    }
                });
            });
        });
        ui.separator();

        ui.horizontal(|ui| {
            // Big cover on the left.
            let cover_width = 260.0;
            let cover_height = 340.0;
            let (rect, _) =
                ui.allocate_exact_size(egui::vec2(cover_width, cover_height), egui::Sense::hover());
            let painter = ui.painter_at(rect);
            painter.rect_filled(rect, 12.0, egui::Color32::from_rgb(0x1a, 0x1c, 0x22));
            painter.rect_stroke(
                rect,
                12.0,
                egui::Stroke::new(3.0_f32, egui::Color32::from_rgb(0x3a, 0x3c, 0x44)),
                egui::StrokeKind::Inside,
            );

            match covers.get(&game.app_id) {
                Some(CoverSnapshot::Ready(image)) => {
                    let tex = image.texture(ctx, &format!("gfn_cover_{}", game.app_id));
                    let tex_size = tex.size_vec2();
                    let src_aspect = tex_size.x / tex_size.y.max(1.0);
                    let slot_aspect = cover_width / cover_height;
                    let (draw_width, draw_height) = if src_aspect > slot_aspect {
                        (cover_width, cover_width / src_aspect)
                    } else {
                        (cover_height * src_aspect, cover_height)
                    };
                    let x = rect.center().x - draw_width / 2.0;
                    let y = rect.center().y - draw_height / 2.0;
                    let draw_rect = egui::Rect::from_min_size(
                        egui::pos2(x, y),
                        egui::vec2(draw_width, draw_height),
                    );
                    painter.image(
                        tex.id(),
                        draw_rect,
                        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                        egui::Color32::WHITE,
                    );
                }
                Some(CoverSnapshot::Loading) => {
                    ui.put(rect, egui::Spinner::new());
                }
                Some(CoverSnapshot::Failed) | None => {
                    let initial = game.title.chars().next().unwrap_or('?');
                    painter.text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        initial.to_string(),
                        egui::FontId::proportional(72.0),
                        egui::Color32::from_rgb(0xa0, 0xa4, 0xac),
                    );
                }
            }

            // Info on the right.
            ui.vertical(|ui| {
                ui.set_width(ui.available_width());
                ui.add_space(20.0);
                ui.heading(&game.title);
                ui.add_space(12.0);
                ui.label(
                    egui::RichText::new(format!("appId: {}", game.app_id))
                        .monospace()
                        .size(14.0),
                );
                ui.add_space(24.0);
                ui.label(
                    egui::RichText::new("Fase 3 - streaming real aún no implementado.").size(14.0),
                );
                ui.add_space(8.0);
                ui.label("Pulsa Confirmar (×) para simular el lanzamiento.");
                ui.add_space(4.0);
                ui.label("Pulsa Atrás (○) para volver al catálogo.");
            });
        });

        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            ui.add_space(6.0);
            if let Some(note) = status_note {
                ui.label(egui::RichText::new(note).italics());
            }
            ui.label("Confirmar (X) para simular lanzamiento · Atras (O) para volver");
            ui.separator();
        });
    });
}

fn creating_session_screen(
    ctx: &egui::Context,
    user: &crate::gfn::auth::GfnUser,
    games: &[GameSummary],
    selected: usize,
    _filtered_indices: &[usize],
    _search_query: &str,
    _search_requested: bool,
    _covers: &CoverStore,
    is_polling: bool,
    queue_status: &crate::gfn::cloudmatch::QueueStatus,
    status_note: Option<&str>,
) -> Option<AppCommand> {
    let mut command = None;
    let game = games.get(selected);
    egui::CentralPanel::default().show(ctx, |ui| {
        ui.horizontal(|ui| {
            ui.heading("Creando sesion de streaming");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(egui::RichText::new("[X] Detener Sesión").color(egui::Color32::from_rgb(0xff, 0x6b, 0x6b))).clicked() {
                    command = Some(AppCommand::ToggleConfirmExit);
                }
                ui.add_space(10.0);
                ui.vertical(|ui| {
                    ui.label(&user.display_name);
                    if let Some(email) = &user.email {
                        ui.weak(email);
                    }
                });
            });
        });
        ui.separator();

        ui.vertical_centered(|ui| {
            ui.add_space(50.0);
            ui.spinner();
            ui.add_space(16.0);
            if let Some(game) = game {
                ui.heading(format!("Preparando sesion para \"{}\"...", game.title));
            } else {
                ui.heading("Preparando sesion...");
            }
            if is_polling {
                ui.add_space(16.0);
                if queue_status.queue_position > 0 {
                    ui.label(
                        egui::RichText::new(format!("• Posición en la cola de NVIDIA: #{}", queue_status.queue_position))
                            .color(egui::Color32::from_rgb(0x00, 0xe6, 0x76))
                            .strong()
                            .size(18.0),
                    );
                    ui.add_space(8.0);
                    if queue_status.eta_ms > 0 {
                        let secs = (queue_status.eta_ms / 1000) % 60;
                        let mins = queue_status.eta_ms / 60000;
                        if mins > 0 {
                            ui.label(egui::RichText::new(format!("Tiempo estimado: ~{} min {} s", mins, secs)).size(15.0));
                        } else {
                            ui.label(egui::RichText::new(format!("Tiempo estimado: ~{} segundos", secs)).size(15.0));
                        }
                    }
                    ui.add_space(6.0);
                    ui.weak(format!("Actualizando estado en vivo (comprobación {})...", queue_status.attempt));
                } else if queue_status.attempt > 0 {
                    ui.label(egui::RichText::new(format!("Conectando con el servidor de NVIDIA (comprobación {})...", queue_status.attempt)).size(15.0));
                } else {
                    ui.label("Esperando a que el servidor de NVIDIA este listo...");
                }
            }
        });

        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            ui.add_space(6.0);
            if let Some(note) = status_note {
                ui.label(egui::RichText::new(note).italics());
            }
            ui.horizontal(|ui| {
                ui.label("Toca '[X] Detener Sesión' o pulsa Atrás (O) para confirmar salida");
            });
            ui.separator();
        });
    });
    command
}

fn session_ready_screen(
    ctx: &egui::Context,
    user: &crate::gfn::auth::GfnUser,
    games: &[GameSummary],
    selected: usize,
    _filtered_indices: &[usize],
    _search_query: &str,
    _search_requested: bool,
    _covers: &CoverStore,
    session: &crate::gfn::cloudmatch::SessionInfo,
    _http_client: &Client,
    status_note: Option<&str>,
) -> Option<AppCommand> {
    let mut command = None;
    let game = games.get(selected);
    egui::CentralPanel::default().show(ctx, |ui| {
        ui.horizontal(|ui| {
            ui.heading("Sesion lista");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(egui::RichText::new("🛑 Detener Sesión").color(egui::Color32::from_rgb(0xff, 0x6b, 0x6b))).clicked() {
                    command = Some(AppCommand::ToggleConfirmExit);
                }
                ui.add_space(10.0);
                ui.vertical(|ui| {
                    ui.label(&user.display_name);
                    if let Some(email) = &user.email {
                        ui.weak(email);
                    }
                });
            });
        });
        ui.separator();

        ui.vertical(|ui| {
            ui.set_width(ui.available_width());
            ui.add_space(20.0);
            if let Some(game) = game {
                ui.heading(format!("Juego: {}", game.title));
                ui.add_space(8.0);
            }
            ui.label(format!("Session ID: {}", session.session_id));
            ui.label(format!("Server IP: {}", session.server_ip));
            ui.label(format!("Signaling: {}", session.signaling_server));
            ui.label(format!("Signaling URL: {}", session.signaling_url));
            if let Some(profile) = &session.negotiated_stream_profile {
                if let Some(res) = &profile.resolution {
                    ui.label(format!("Resolucion: {}", res));
                }
                if let Some(fps) = profile.fps {
                    ui.label(format!("FPS: {}", fps));
                }
                if let Some(codec) = &profile.codec {
                    ui.label(format!("Codec: {}", codec));
                }
            }
            ui.add_space(20.0);
            ui.label("Confirmar (X) para conectar la señalización de NVIDIA.");
        });

        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            ui.add_space(6.0);
            if let Some(note) = status_note {
                ui.label(egui::RichText::new(note).italics());
            }
            ui.label("Confirmar (X) para conectar · Toca '🛑 Detener Sesión' para salir");
            ui.separator();
        });
    });
    command
}

#[allow(clippy::too_many_arguments)]
fn signaling_screen(
    ctx: &egui::Context,
    user: &crate::gfn::auth::GfnUser,
    games: &[GameSummary],
    selected: usize,
    session: &crate::gfn::cloudmatch::SessionInfo,
    offer_sdp: Option<&str>,
    status_note: Option<&str>,
) -> Option<AppCommand> {
    let mut command = None;
    let game = games.get(selected);
    egui::CentralPanel::default().show(ctx, |ui| {
        ui.horizontal(|ui| {
            ui.heading("Señalización");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(egui::RichText::new("🛑 Detener Sesión").color(egui::Color32::from_rgb(0xff, 0x6b, 0x6b))).clicked() {
                    command = Some(AppCommand::ToggleConfirmExit);
                }
                ui.add_space(10.0);
                ui.label(&user.display_name);
            });
        });
        ui.separator();

        ui.vertical(|ui| {
            ui.add_space(20.0);
            if let Some(game) = game {
                ui.heading(format!("Juego: {}", game.title));
                ui.add_space(8.0);
            }
            ui.label(format!("Sesión: {}", session.session_id));
            ui.add_space(12.0);
            ui.spinner();
            ui.add_space(12.0);
            match offer_sdp {
                Some(sdp) => {
                    ui.label(format!("Offer SDP recibido ({} bytes).", sdp.len()));
                    ui.label("Construcción del peer WebRTC: siguiente paso de Fase 3.");
                }
                None => {
                    ui.label("Esperando el offer SDP del servidor de GFN...");
                }
            }
        });

        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            ui.add_space(6.0);
            if let Some(note) = status_note {
                ui.label(egui::RichText::new(note).italics());
            }
            ui.label("Toca '🛑 Detener Sesión' o pulsa Atrás (O) para confirmar salida");
            ui.separator();
        });
    });
    command
}

fn confirm_exit_modal(ctx: &egui::Context) -> Option<AppCommand> {
    let mut command = None;
    egui::Window::new("Confirmar salida")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(8.0);
                ui.heading("¿Detener sesión de streaming?");
                ui.add_space(10.0);
                ui.label("¿Estás seguro de que deseas salir y cancelar la sesión activa de GeForce NOW?");
                ui.add_space(18.0);
                ui.horizontal(|ui| {
                    if ui.button(" ◀ Volver a la sesión ").clicked() {
                        command = Some(AppCommand::CancelConfirmExit);
                    }
                    ui.add_space(16.0);
                    if ui.button(egui::RichText::new(" 🛑 Sí, Salir y Detener ").color(egui::Color32::from_rgb(0xff, 0x6b, 0x6b))).clicked() {
                        command = Some(AppCommand::ConfirmExitSession);
                    }
                });
                ui.add_space(8.0);
            });
        });
    command
}

fn streaming_screen(
    ctx: &egui::Context,
    _user: &crate::gfn::auth::GfnUser,
    games: &[GameSummary],
    selected: usize,
    has_video: bool,
    status_note: Option<&str>,
) -> Option<AppCommand> {
    let mut command = None;
    let game = games.get(selected);

    // The video itself is drawn by the shell (`surface::draw_scene`) straight from the SDL
    // textures the frame producer writes into - green-vita's direct-texture path. This panel
    // is transparent so that quad shows through; egui only overlays UI on top.
    let mut frame = egui::Frame::central_panel(&ctx.style());
    frame.fill = egui::Color32::TRANSPARENT;
    egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
        if !has_video {
            ui.vertical_centered(|ui| {
                ui.add_space(80.0);
                ui.spinner();
                ui.add_space(16.0);
                if let Some(g) = game {
                    ui.heading(format!("Transmitiendo \"{}\"", g.title));
                } else {
                    ui.heading("Transmitiendo juego...");
                }
                ui.add_space(12.0);
                ui.label(egui::RichText::new("• Señalización WebRTC e Intercambio SDP Completados").color(egui::Color32::from_rgb(0x00, 0xe6, 0x76)).strong());
                ui.add_space(8.0);
                // Live pipeline stage from the peer thread - the key diagnostic when the
                // stream stalls before the first decoded frame.
                ui.label(status_note.unwrap_or("Esperando negociación WebRTC..."));
            });
        }

        ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
            ui.horizontal(|ui| {
                if ui.button(egui::RichText::new("[X] Detener Sesión").color(egui::Color32::from_rgb(0xff, 0x6b, 0x6b))).clicked() {
                    command = Some(AppCommand::ToggleConfirmExit);
                }
            });
        });

        // Small always-on pipeline readout - kept visible over the video so a black frame
        // is still diagnosable from a screenshot.
        if let Some(note) = status_note {
            ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                ui.add_space(6.0);
                ui.colored_label(
                    egui::Color32::from_rgba_unmultiplied(255, 255, 255, 140),
                    egui::RichText::new(note).size(11.0),
                );
            });
        }
    });

    command
}

fn error_screen(ctx: &egui::Context, message: &str) {
    egui::CentralPanel::default().show(ctx, |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space(80.0);
            ui.heading("Error");
            ui.add_space(12.0);
            ui.label(message);
            ui.add_space(24.0);
            ui.label("Confirmar o Atrás para volver.");
        });
    });
}

/// Draws a QR code's module grid as plain filled rects (not an image/texture blit) - adapted
/// from green-vita (MPL-2.0), src/app/ui/screens/token_setup.rs. See THIRD_PARTY_NOTICES.md.
struct QrImage {
    uri: String,
    modules: Vec<bool>,
    size: u32,
}

fn draw_qr(ui: &mut egui::Ui, verification_uri: &str, target_size: f32) {
    const QUIET_ZONE_MODULES: u32 = 2;
    let cache_id = egui::Id::new("device_code_qr");
    let cached = ui.ctx().data_mut(|data| {
        if let Some(cached) = data.get_temp::<Arc<QrImage>>(cache_id)
            && cached.uri == verification_uri
        {
            return Some(cached);
        }

        let code = qrcode::QrCode::new(verification_uri).ok()?;
        let image = Arc::new(QrImage {
            uri: verification_uri.to_owned(),
            size: code.width() as u32,
            modules: code
                .to_colors()
                .into_iter()
                .map(|color| color == qrcode::Color::Dark)
                .collect(),
        });
        data.insert_temp(cache_id, image.clone());
        Some(image)
    });
    let Some(cached) = cached else {
        ui.spinner();
        return;
    };
    let total_modules = cached.size + QUIET_ZONE_MODULES * 2;
    let module_size = target_size / total_modules as f32;

    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(target_size, target_size), egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 4.0, egui::Color32::WHITE);
    for y in 0..cached.size {
        for x in 0..cached.size {
            if !cached.modules[(y * cached.size + x) as usize] {
                continue;
            }
            let module_rect = egui::Rect::from_min_size(
                rect.min
                    + egui::vec2(
                        (QUIET_ZONE_MODULES + x) as f32 * module_size,
                        (QUIET_ZONE_MODULES + y) as f32 * module_size,
                    ),
                egui::vec2(module_size, module_size),
            );
            painter.rect_filled(module_rect, 0.0, egui::Color32::BLACK);
        }
    }
}
