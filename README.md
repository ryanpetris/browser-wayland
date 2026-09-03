# browser-wayland

A headless Wayland compositor whose screen is a browser tab. Clients render on the GPU, the
composited frame is hardware-encoded to H.264 (VA-API) and streamed over a WebSocket, and the
browser decodes it with WebCodecs. Mouse and keyboard input flow back the same way.

## Requirements

- Linux with a GPU render node (`/dev/dri/renderD128`) and Mesa.
- GStreamer 1.24+ with the VA plugin: `gst-plugin-va` on Arch (`vapostproc`, `vah264enc`).
- Rust stable. The browser needs WebCodecs (Chromium, Firefox 130+, Safari 26+).

## Run

```sh
cargo run --release -- --exec 'foot'          # any Wayland client; WAYLAND_DISPLAY is set for it
```

The server prints the certificate fingerprint and a URL like `https://<lan-ip>:8443/?token=…`.
Open it in a browser on the LAN, compare the fingerprint before accepting the self-signed
certificate, and the desktop appears. The browser viewport size becomes the output size.
The ⛶ button enters fullscreen with keyboard lock so shortcuts like Ctrl+W reach the desktop.
Frames are drawn with WebGPU when the browser has it (zero-copy import of the decoded frame), else a 2D canvas.

Other clients can join later: `WAYLAND_DISPLAY=wayland-browser some-app`.

Useful flags: `--no-tls` (localhost development), `--listen`, `--bitrate <kbps>`,
`--codec auto|h264|hevc|vp9` (auto prefers whatever the browser decodes in hardware: HEVC, then VP9,
then H.264), `--fake-source` (a test pattern instead of the compositor), `--socket-name`, `--render-node`.

Games and other clients that lock the pointer get raw mouse deltas: the page mirrors the lock with the
Pointer Lock API; Escape releases it.

Certificate and token live in `$XDG_CONFIG_HOME/browser-wayland/`; delete them to regenerate.
