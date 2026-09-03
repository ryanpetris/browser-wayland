# browser-wayland

A headless Wayland compositor whose screen is a browser tab. Clients render on the GPU, the
composited frame is hardware-encoded to H.264 (VA-API) and streamed over a WebSocket, and the
browser decodes it with WebCodecs. Mouse and keyboard input flow back the same way.

## Requirements

- Linux with a GPU render node (`/dev/dri/renderD128`) and Mesa.
- GStreamer 1.24+ with the VA plugin: `gst-plugin-va` on Arch (`vapostproc`, `vah264enc`).
- `xorg-xwayland` for X11 clients, and PipeWire or PulseAudio with `pactl` for audio (both optional).
- Rust stable. The browser needs WebCodecs (Chromium, Firefox 130+, Safari 26+).

## Run

```sh
cargo run --release -- --exec 'foot'          # any Wayland client; WAYLAND_DISPLAY is set for it
```

The server prints the certificate fingerprint and a URL like `https://<lan-ip>:8443/?token=…`.
Open it in a browser on the LAN, compare the fingerprint before accepting the self-signed
certificate, and the desktop appears. The browser viewport size becomes the output size.
The ⛶ button enters fullscreen with keyboard lock so shortcuts like Ctrl+W reach the desktop.
Frames are painted on a 2D canvas. `?renderer=webgpu` in the URL uses a WebGPU external-texture path
instead; it is opt-in because Chromium on Linux occasionally presents a blank frame that way, which looks like flicker.

Other clients can join later: `WAYLAND_DISPLAY=wayland-browser some-app`. X11 apps work too: an
Xwayland is started automatically and the log prints its `DISPLAY`. Super (or Alt) + left drag moves
any window, which is how undecorated X11 windows get moved.

Audio from clients goes to the browser instead of the host speakers: the server creates a private
`browser-wayland-<pid>` sink (printed in the log) and captures it as Opus. Clients started with
`--exec` get `PULSE_SINK` set; for others use `PULSE_SINK=<that name> some-app`. `--no-audio` turns this off.

`--exec` runs when the first viewer connects, with `BW_WIDTH`/`BW_HEIGHT` set to the browser's size,
and `--kiosk` fullscreens every window. Together they run a whole nested desktop; with the
`mutter-devkit` package installed, GNOME:

```sh
cargo run --release -- --kiosk --exec 'dbus-run-session -- gnome-shell --devkit'
```

The devkit's window follows the browser size. Don't add `--virtual-monitor`: that adds a second
monitor, and GNOME puts its top bar only on the first one.

Useful flags: `--no-tls` (localhost development), `--listen`, `--bitrate <kbps>`,
`--codec auto|h264|hevc|vp9` (auto prefers whatever the browser decodes in hardware: HEVC, then VP9,
then H.264), `--fake-source` (a test pattern instead of the compositor), `--socket-name`, `--render-node`.

Games and other clients that lock the pointer get raw mouse deltas: the page mirrors the lock with the
Pointer Lock API; Escape releases it.

Certificate and token live in `$XDG_CONFIG_HOME/browser-wayland/`; delete them to regenerate.
