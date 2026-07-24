# Third-party notices

This project reuses code and protocol knowledge from two external projects, neither of which
is distributed inside this repository (they live only under `reference/`, gitignored, and are
used as development references).

## green-vita (MPL-2.0)

https://github.com/Day-OS/green-vita

A native Rust client for Xbox Cloud Gaming on the PS Vita. This project follows its same
architecture (SDL2 + egui, VPK packaging with `cargo-vita`, VitaSDK build) and reuses specific
code under the terms of the Mozilla Public License 2.0 (see `LICENSE`, the same license
green-vita uses):

- `.cargo/config.toml`, `Makefile`, `tools/vita-*`: adapted from green-vita's build
  configuration for the `armv7-sony-vita-newlibeabihf` target.
- `src/shell/egui_painter.rs`: copied essentially unchanged (a generic egui-over-SDL2
  renderer, with no streaming/Xbox-specific logic).
- `src/input.rs`: the Vita controller's SDL mapping GUID
  (`register_vita_controller_mapping`) and the front touch device id
  (`FRONT_TOUCH_DEVICE_ID`) are adapted from green-vita — undocumented platform details found
  nowhere else.
- `src/jobs.rs`: copied unchanged (a generic helper for polling a background Tokio task
  without blocking the render loop).
- `src/safe_memory.rs`: copied unchanged (a thin wrapper over `sceAppUtil*SafeMemory`).
- `src/gfn/auth.rs`: the token-at-rest encryption scheme (ChaCha20-Poly1305 via `ring`, with
  the key stored in Safe Memory rather than in the file itself) and the `encode_hex`/
  `decode_hex` functions are adapted from green-vita's Xbox token storage
  (`src/api_xbox/auth.rs`).
- `src/app/ui.rs` (`draw_qr`): adapted from green-vita
  (`src/app/ui/screens/token_setup.rs`) — draws the QR code as filled rectangles directly with
  egui's painter, with no dependency on a texture/image.
- `Cargo.toml`: the `[patch.crates-io] ring = { git = ".../vita-rust/ring", branch =
  "v0.17.14-vita" }` patch is needed because crates.io's `ring` doesn't build for
  `armv7-sony-vita-newlibeabihf` (no recognized random number source for that target) —
  reused from green-vita's `Cargo.toml`.

Under MPL-2.0, each modified file remains under that license; the rest of the project may use
a different license as long as the covered files stay available under MPL-2.0 (see the header
in each reused file).

## OpenNOW (original project's license — see its repository)

https://github.com/OpenCloudGaming/OpenNOW

An open-source GFN client (Electron + a native Rust streamer). No code from OpenNOW is reused
directly in this repository — it was used only as a protocol reference (authentication,
WebRTC signaling, the proprietary "NVST" SDP format, the input packet format), documented in
`docs/protocol-notes.md`. All Rust code in this project that implements that protocol logic
is written from scratch.

## `rtc` / `rtc-media` (green-vita's Vita fork)

This project uses green-vita's fork of the `rtc`/`rtc-media`/`ring` crates, patched to build
for `armv7-sony-vita-newlibeabihf` (https://github.com/Day-OS/rtc, `vita` branch;
https://github.com/vita-rust/ring, `v0.17.14-vita` branch), for the WebRTC peer connection
(ICE/DTLS/SRTP) and H.264 RTP depacketization.
