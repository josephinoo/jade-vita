# Avisos de terceros

Este proyecto reutiliza código y conocimiento de dos proyectos externos, ninguno de los
cuales se distribuye dentro de este repositorio (viven solo en `reference/`, con gitignore, y
se usan como referencia de desarrollo).

## green-vita (MPL-2.0)

https://github.com/Day-OS/green-vita

Cliente Rust nativo para Xbox Cloud Gaming en PS Vita. Este proyecto sigue su misma
arquitectura (SDL2 + egui, empaquetado VPK con `cargo-vita`, build para VitaSDK) y reutiliza
código puntual bajo los términos de la Mozilla Public License 2.0 (ver `LICENSE`, idéntica
licencia que usa green-vita):

- `.cargo/config.toml`, `Makefile`, `tools/vita-*`: adaptados de la configuración de build de
  green-vita para el target `armv7-sony-vita-newlibeabihf`.
- `src/shell/egui_painter.rs`: copiado prácticamente sin cambios (renderer genérico de egui
  sobre SDL2, sin lógica específica de streaming/Xbox).
- `src/input.rs`: el GUID de mapeo del mando de la Vita para SDL
  (`register_vita_controller_mapping`) y el id de dispositivo táctil frontal
  (`FRONT_TOUCH_DEVICE_ID`) están adaptados de green-vita — son detalles de plataforma no
  documentados en ningún otro sitio.
- `src/jobs.rs`: copiado sin cambios (helper genérico para sondear una tarea Tokio en segundo
  plano sin bloquear el bucle de renderizado).
- `src/safe_memory.rs`: copiado sin cambios (wrapper fino sobre `sceAppUtil*SafeMemory`).
- `src/gfn/auth.rs`: el esquema de cifrado en reposo de los tokens (ChaCha20-Poly1305 vía
  `ring`, con la clave guardada en Safe Memory en vez de en el propio archivo) y las funciones
  `encode_hex`/`decode_hex` están adaptados del almacenamiento de tokens de Xbox de green-vita
  (`src/api_xbox/auth.rs`).
- `src/app/ui.rs` (`draw_qr`): adaptado de green-vita
  (`src/app/ui/screens/token_setup.rs`) — dibuja el QR como rectángulos rellenos directamente
  con el pintor de egui, sin depender de una textura/imagen.
- `Cargo.toml`: el parche `[patch.crates-io] ring = { git = ".../vita-rust/ring", branch =
  "v0.17.14-vita" }` es necesario porque el `ring` de crates.io no compila para
  `armv7-sony-vita-newlibeabihf` (sin fuente de números aleatorios reconocida para ese
  target) — reutilizado del `Cargo.toml` de green-vita.

Bajo MPL-2.0, cada archivo modificado permanece bajo esa licencia; el resto del proyecto
puede tener una licencia distinta siempre que los archivos cubiertos se mantengan
disponibles bajo MPL-2.0 (ver cabecera en cada archivo reutilizado).

## OpenNOW (licencia del proyecto original — ver su repositorio)

https://github.com/OpenCloudGaming/OpenNOW

Cliente GFN de código abierto (Electron + streamer nativo en Rust). No se reutiliza código
de OpenNOW directamente en este repositorio — se usó únicamente como referencia de protocolo
(autenticación, señalización WebRTC, formato del SDP propietario "NVST", formato de paquetes
de input) documentada en `docs/protocol-notes.md`. Todo el código Rust de este proyecto que
implementa esa lógica de protocolo está escrito desde cero.

## `rtc` / `rtc-media` (fork de green-vita para Vita)

Fase 3 de este proyecto reutilizará el fork de green-vita de los crates `rtc`/`rtc-media`/
`ring` parcheados para compilar en `armv7-sony-vita-newlibeabihf`
(https://github.com/Day-OS/rtc, rama `vita`; https://github.com/vita-rust/ring, rama
`v0.17.14-vita`). Se documentará aquí de nuevo cuando se añadan como dependencias en
`Cargo.toml`.
