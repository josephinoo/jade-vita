# Jade Vita

A native homebrew **GeForce NOW client for the PlayStation Vita**, written in Rust. It signs
in to your GFN account, browses your game library, negotiates a real WebRTC session against
NVIDIA's streaming servers, and plays the game on the Vita's screen using the console's
hardware H.264 decoder — with your controller input forwarded back to the game.

Architecturally it follows the trail blazed by
[green-vita](https://github.com/Day-OS/green-vita) (Xbox Cloud Gaming on the Vita): SDL2 +
egui for the UI, direct-to-texture hardware video decoding, and VPK packaging via
`cargo-vita`. The GFN protocol work builds on
[OpenNOW](https://github.com/OpenCloudGaming/OpenNOW) as a reference.

> **Disclaimer**: this project is **not affiliated with, endorsed by, or associated with
> NVIDIA or GeForce NOW** in any way. It is an unofficial alternative client. You need your
> own GeForce NOW account to use it.

## Features

- **NVIDIA login on the console** — device-code flow (QR code + short code on a second
  device), with tokens encrypted at rest (ChaCha20-Poly1305, key stored in the Vita's Safe
  Memory).
- **Game library** — your GFN catalog with cover art, server-side search, and game detail
  pages (GFN GraphQL API).
- **Session brokering** — CloudMatch session creation, queue position tracking, and
  seat-aware polling (re-polls the assigned game server directly once seated).
- **Real WebRTC streaming** — NVST WebSocket signaling, SDP offer/answer against NVIDIA's
  ICE-lite servers, DTLS-SRTP, and H.264 RTP depacketization, all through the sans-I/O
  [`rtc`](https://github.com/webrtc-rs/rtc) stack (no GStreamer, no browser).
- **Hardware video decoding** — `sceAvcdec` decodes each access unit straight into SDL/GXM
  textures (green-vita's direct-texture path: zero per-frame allocations, double-buffered,
  dynamic YUV420/BGR565 negotiation based on the stream's actual resolution).
- **Audio playback** — Opus RTP packets decoded via `libopus` and played through SDL2, tuned
  for low end-to-end latency (small jitter buffers on both audio and video so neither track
  drifts ahead of the other).
- **Controller input** — full gamepad state (buttons, sticks) sent 60×/s over the NVST
  `input_channel_v1` data channel in XInput format.
- **Session resilience** — CloudMatch session polling tolerates transient server errors
  (isolated 5xx responses from NVIDIA's zone load balancer) instead of aborting a session
  that would have come up fine on the next poll; disconnects clean up the CloudMatch session
  server-side instead of leaking it.
- **Language picker** — a gear icon next to the account avatar on the catalog screen switches
  the UI between English and Spanish (more languages can be added under `src/i18n/`).

## Status

| Phase | Scope | State |
|---|---|---|
| 0 | Protocol research (`docs/protocol-notes.md`) | ✅ Done |
| 1 | App skeleton: VitaSDK/`cargo-vita` build, SDL2 + egui loop | ✅ Done |
| 2 | Authentication + game library | ✅ Done |
| 3 | Signaling + CloudMatch session lifecycle | ✅ Done |
| 4 | WebRTC peer, H.264 decode, gamepad input | ✅ Working (Vita3K + real PS Vita hardware) |
| 5 | Audio (Opus), session resilience, UI polish | ✅ Working (Vita3K + real PS Vita hardware) |
| 6 | Real-hardware validation | ✅ Confirmed working on an original PS Vita |

Known gaps: analog triggers/L3/R3 need a rear-touchpad mapping (the Vita has no such physical
controls); the language picker only ships English/Spanish text so far, the rest of the UI
outside the catalog screen is still hardcoded Spanish. Development is validated against both
[Vita3K](https://vita3k.org/) (whose `sceAvcdec` only implements YUV420 output, handled at
runtime) and real PS Vita hardware.

See `THIRD_PARTY_NOTICES.md` for what is reused from green-vita (MPL-2.0) and what is
protocol knowledge referenced from OpenNOW.

## Build requirements

- [VitaSDK](https://vitasdk.org/) installed, with the `VITASDK` environment variable
  pointing at it (this project does not install VitaSDK for you).
- Rust nightly + [`cargo-vita`](https://github.com/vita-rust/cargo-vita):
  ```sh
  rustup toolchain install nightly
  cargo +nightly install cargo-vita
  rustup target add armv7-sony-vita-newlibeabihf --toolchain nightly
  ```
- `pkg-config` (e.g. `brew install pkg-config` on macOS).

## Building

```sh
make vpk                                    # builds target/armv7-sony-vita-newlibeabihf/release/jade-vita.vpk
make upload-vpk VITA_IP=192.168.0.103       # uploads the VPK to ux0:/data/ via VitaShell/vitacompanion
make update-run-vita VITA_IP=192.168.0.103  # build + update + launch in one step
```

Uploading requires [VitaShell](https://github.com/TheOfficialFloW/VitaShell)'s FTP server or
`vitacompanion` running on the console, on the same network as your computer. The VPK also
installs and runs in the Vita3K emulator.

## Project layout

```
.cargo/config.toml      Cross-compilation target/toolchain (armv7-sony-vita-newlibeabihf)
tools/                  vita-gcc/vita-ar/vita-pkg-config wrappers (VitaSDK)
static/sce_sys/         App metadata (icon, LiveArea) packaged into the VPK
src/
  main.rs               Entry point; Vita heap/stack sizing, CDRAM reservation
  app/                  Application state machine and egui UI
  shell/                Main loop: SDL2 window, egui painter, direct video surface
  input.rs              SDL2 event mapping (keyboard/controller/touch) + XInput snapshots
  locale.rs             Supported UI locales (currently English/Spanish)
  i18n.rs, i18n/*.ftl   Fluent-based UI translations (wired into the catalog screen so far)
  streaming/
    video/              Direct-texture video pipeline: decoder sync, sceAvcdec, decode worker
    audio.rs            Opus RTP decode + SDL2 audio playback
  gfn/
    auth.rs             NVIDIA device-code OAuth + encrypted token storage
    catalog.rs          Game library (GraphQL) + server-side search
    covers.rs           Cover-art cache with bounded async downloads
    cloudmatch.rs       Session create/poll/stop against the CloudMatch REST API
    signaling.rs        NVST WebSocket signaling (offer/answer/ICE trickle)
    sdp.rs              Offer sanitation + NVST answer blob construction
    peer.rs             Sans-I/O WebRTC peer: ICE/DTLS/SRTP, RTP → H.264 access units
    input_protocol.rs   NVST input-channel binary protocol (gamepad + heartbeat)
docs/protocol-notes.md  GFN protocol reverse-engineering notes (Phase 0)
```

The assets in `static/sce_sys/` (icon, LiveArea backgrounds) are auto-generated solid-color
placeholders — replace them with real art before distributing a VPK.

## Acknowledgements

- [green-vita](https://github.com/Day-OS/green-vita) — the direct-texture video pipeline,
  the Vita-patched `ring`/`rtc-shared` forks, and proof that cloud gaming on a Vita is
  possible at all.
- [OpenNOW](https://github.com/OpenCloudGaming/OpenNOW) — the GFN protocol reference
  (CloudMatch, NVST signaling, and the input-channel wire format).
- MattKC's [Vanilla](https://github.com/vanilla-wiiu/vanilla) — the single-reference-frame
  decoder trick.
