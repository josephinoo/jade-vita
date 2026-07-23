#[link(name = "SDL2", kind = "static")]
unsafe extern "C" {}

mod egui_painter;
mod surface;

use crate::app::ui::build_ui;
use crate::app::{App, AppState};
use crate::input::{
    gamepad_snapshot, held_menu_direction, map_controller_button_event, map_keyboard_event,
    map_pointer_event, open_first_controller, register_vita_controller_mapping,
};
use anyhow::Result;
use std::time::{Duration, Instant};
use surface::{HEIGHT, VitaSurface, WIDTH};
use tokio::time::sleep;

/// Scales `pixels_per_point` up so the UI reads legibly on the Vita's small screen.
const UI_SCALE: f32 = 1.3;
// D-Pad/left-stick auto-repeat: immediate on press, then repeating once held past the delay.
const DIRECTION_REPEAT_INITIAL_DELAY: Duration = Duration::from_millis(350);
const DIRECTION_REPEAT_INTERVAL: Duration = Duration::from_millis(90);

pub(crate) const TARGET_FRAME_TIME: Duration = Duration::from_millis(16);

pub async fn run(mut app: App) -> Result<()> {
    let sdl = sdl2::init().map_err(anyhow::Error::msg)?;
    let video = sdl.video().map_err(anyhow::Error::msg)?;
    register_vita_controller_mapping(&sdl).map_err(anyhow::Error::msg)?;
    let game_controller_subsystem = sdl.game_controller().map_err(anyhow::Error::msg)?;
    let mut controller = open_first_controller(&game_controller_subsystem);
    let mut event_pump = sdl.event_pump().map_err(anyhow::Error::msg)?;
    let mut surface = VitaSurface::new(&video)?;
    let egui_ctx = egui::Context::default();
    let start_time = Instant::now();
    let mut pointer_pos = egui::Pos2::ZERO;
    let mut held_direction = None;
    let mut held_direction_since = Instant::now();
    let mut last_direction_repeat_at = Instant::now();
    let mut text_input_active = false;

    loop {
        let loop_started_at = Instant::now();
        let mut egui_events = Vec::new();
        let mut direct_commands = Vec::new();

        for event in event_pump.poll_iter() {
            if let Some(command) = map_keyboard_event(&event)
                && !direct_commands.contains(&command)
            {
                direct_commands.push(command);
            }
            if let Some(command) = map_controller_button_event(&event)
                && !direct_commands.contains(&command)
            {
                direct_commands.push(command);
            }
            if let Some(egui_event) = map_pointer_event(
                &event,
                (WIDTH as f32 / UI_SCALE, HEIGHT as f32 / UI_SCALE),
                UI_SCALE,
                &mut pointer_pos,
            ) {
                egui_events.push(egui_event);
            }
            match event {
                // Forwarded straight to egui (as the platform text-input method - SDL's own
                // IME/on-screen-keyboard machinery, active by default - would deliver it)
                // instead of accumulated by hand here: `TextInput`'s `text` field is only the
                // newly-composed fragment (usually one character on desktop SDL), not the whole
                // field content, so treating it as a full replacement (what an earlier version
                // of this code did) overwrote the query with just the latest keystroke on every
                // character. Letting the focused `egui::TextEdit` accumulate it internally and
                // report the full string back via `Response::changed()` (see
                // `app::ui::catalog_screen`) sidesteps that entirely.
                sdl2::event::Event::TextInput { ref text, .. } => {
                    egui_events.push(egui::Event::Text(text.clone()));
                }
                sdl2::event::Event::KeyDown {
                    keycode: Some(sdl2::keyboard::Keycode::Backspace),
                    repeat,
                    ..
                } => {
                    egui_events.push(egui::Event::Key {
                        key: egui::Key::Backspace,
                        physical_key: None,
                        pressed: true,
                        repeat,
                        modifiers: egui::Modifiers::default(),
                    });
                }
                sdl2::event::Event::ControllerDeviceAdded { .. } if controller.is_none() => {
                    controller = open_first_controller(&game_controller_subsystem);
                }
                sdl2::event::Event::ControllerDeviceRemoved { .. } => {
                    controller = None;
                }
                _ => {}
            }
        }

        match held_menu_direction(controller.as_ref()) {
            Some(direction) if held_direction == Some(direction) => {
                if held_direction_since.elapsed() >= DIRECTION_REPEAT_INITIAL_DELAY
                    && last_direction_repeat_at.elapsed() >= DIRECTION_REPEAT_INTERVAL
                {
                    direct_commands.push(direction.into());
                    last_direction_repeat_at = Instant::now();
                }
            }
            Some(direction) => {
                direct_commands.push(direction.into());
                held_direction = Some(direction);
                held_direction_since = Instant::now();
                last_direction_repeat_at = Instant::now();
            }
            None => held_direction = None,
        }

        for command in direct_commands {
            app.handle_command(command).await?;
        }
        app.tick().await?;

        // Register/flip the direct video textures against the current streaming peer (if any)
        // and decide whether draw_scene should blit the video quad under the egui overlay.
        let show_video = {
            let streaming_peer = match &app.state {
                AppState::Streaming { peer, .. } => Some(peer),
                _ => None,
            };
            surface.sync_video_frame(streaming_peer)?;
            // Ship the controller state to the game once per frame (~60 Hz) while streaming.
            if let (Some(peer), Some(active_controller)) = (streaming_peer, controller.as_ref()) {
                peer.send_gamepad(gamepad_snapshot(active_controller));
            }
            streaming_peer.is_some_and(|peer| peer.video_frame().is_some())
        };

        // Start/stop SDL's text-input method (system/on-screen keyboard) based on the app's
        // request flag. `Event::TextInput`/Backspace are forwarded to egui above, which feeds
        // the focused `TextEdit` in `catalog_screen` - see the comment on that match arm.
        let search_requested = matches!(
            &app.state,
            AppState::Catalog {
                search_requested: true,
                ..
            }
        );
        if search_requested && !text_input_active {
            video.text_input().start();
            text_input_active = true;
        } else if !search_requested && text_input_active {
            video.text_input().stop();
            text_input_active = false;
        }

        let raw_input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(WIDTH as f32 / UI_SCALE, HEIGHT as f32 / UI_SCALE),
            )),
            viewport_id: egui::ViewportId::ROOT,
            viewports: std::iter::once((
                egui::ViewportId::ROOT,
                egui::ViewportInfo {
                    native_pixels_per_point: Some(UI_SCALE),
                    ..Default::default()
                },
            ))
            .collect(),
            time: Some(start_time.elapsed().as_secs_f64()),
            predicted_dt: TARGET_FRAME_TIME.as_secs_f32(),
            events: egui_events,
            ..Default::default()
        };

        let mut ui_commands = Vec::new();
        let full_output = egui_ctx.run(raw_input, |ctx| {
            ui_commands = build_ui(ctx, &app);
        });

        for command in ui_commands {
            app.handle_command(command).await?;
        }

        surface.draw_scene(show_video)?;
        let clipped_primitives =
            egui_ctx.tessellate(full_output.shapes, full_output.pixels_per_point);
        surface.paint_egui(
            full_output.pixels_per_point,
            &clipped_primitives,
            &full_output.textures_delta,
        )?;

        let frame_deadline = loop_started_at + TARGET_FRAME_TIME;
        let now = Instant::now();
        if now < frame_deadline {
            sleep(frame_deadline - now).await;
        } else {
            tokio::task::yield_now().await;
        }
    }
}
