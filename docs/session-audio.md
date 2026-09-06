# Session audio

Each desktop owns a private PipeWire server, pipewire-pulse and WirePlumber. Their native and Pulse
sockets, configuration and state live in a private temporary directory. The existing Wayland runtime
directory remains usable. The services load distribution modules and policy, with hardware discovery
and persistent host routing state disabled. They do not enumerate ALSA, Bluetooth or video devices.
This scopes the audio graph; it does not sandbox arbitrary applications running as the same user.

The graph has three virtual devices:

| Name | Purpose |
| --- | --- |
| `elsewhere-output` | Default stereo application output, captured for the browser |
| `elsewhere-microphone` | Default mono source carrying the controlling browser's microphone |
| `elsewhere-microphone-input` | Internal input to the microphone loopback |

The output is a native null sink. The microphone uses a native loopback module. The output and
microphone keep processing while idle, so recording starts without waiting for a playing application
and inactive microphone input produces silence. WirePlumber chooses defaults by priority; the
microphone outranks sink monitors, including on older WirePlumber versions.

GStreamer captures the output through `pipewiresrc` and encodes 48 kHz stereo Opus. Browser microphone
Opus is decoded and injected through `pipewiresink`. These pipelines run in an owned helper process,
with explicit native socket descriptors. No Pulse modules or Pulse GStreamer elements implement this
media path. `pactl info` is only a startup check of the compatibility protocol and its defaults.

## Starting applications

Startup commands, menu/API launches and descendants receive `PIPEWIRE_REMOTE`, `PULSE_SERVER` and
`PIPEWIRE_CONFIG_DIR` for the private session. Inherited `PULSE_SINK`, `PULSE_SOURCE`, `PIPEWIRE_NODE`
and PipeWire configuration-name overrides are cleared. Playback and recording use different
WirePlumber defaults, so a global device override is inappropriate.

The startup log prints these connection variables. Clients started separately can use those values:

```sh
PIPEWIRE_REMOTE="<native-socket>" PULSE_SERVER="unix:<pulse-socket>" \
PIPEWIRE_CONFIG_DIR="<session-config-directory>" application
```

The selectors identify sockets already owned by the desktop; no WirePlumber lookup is needed to find
them. Device names remain stable within each isolated graph, including across desktop restarts.

## Readiness and failure

Service version checks, native graph discovery and Pulse default checks share an eight-second
deadline. Pipeline startup has a separate eight-second deadline and must produce an encoded audio
packet before clients launch. A blocked GStreamer initialization cannot hold the desktop indefinitely.

`--no-audio` starts no audio services. Initialization failure cleans up partial resources and leaves
the desktop running with audio unavailable. Applications receive failing socket selectors rather
than inheriting a host audio server. A later service or pipeline failure stops the owned stack and
withdraws playback and microphone capabilities from connected viewers. It does not restart audio.

SIGTERM and Ctrl+C are handled from startup. Audio children have separate process groups so terminal
signals go through the owner's cleanup path. Shutdown asks the pipeline helper to stop, gives it
500 ms to exit, then kills and reaps it if necessary before removing services and their directory.
The compositor thread is also joined before process exit. Repeated signals do not interrupt this join.
The container runs the compositor as PID 1 so `docker stop` reaches its signal handler.
Forced termination such as SIGKILL cannot run this cleanup. Outside a container, it can leave audio
children and the temporary directory behind.

## Session mixer

Open **Mixer** in the desktop status bar to inspect session devices and application playback and
recording streams. Streams with application metadata are grouped together, but each row controls one
stream. Read-only viewers can inspect; only the current controlling viewer can change audio.
Changes affect the shared session for every viewer.

Volume ranges from 0 to 100 percent with cubic gain: 50 percent means linear gain 0.125. Mute and
volume use the object's native controls. Unsupported controls are disabled or explained. Current
routing is shown for application streams. When more than one compatible session endpoint exists,
the target selector and device default buttons ask WirePlumber to change routing. These controls do
not enumerate host hardware or alter host defaults.

Meters measure actual peaks. Playback, output and microphone meters follow their volume and mute;
recording-stream meters measure before that stream's own controls. Inactive means no recent samples,
not a stored volume setting. Monitor streams are shared between viewers, publish scalar peaks at
about 10 Hz, and are removed when the last visible mixer closes. Meter failures are shown separately
from control availability. The optional output visualiser is independent of this panel.

Muting **Elsewhere microphone** silences the session source while browser capture continues. Use the
microphone capture toggle to stop recording permission use. Opening the mixer, changing its controls,
and subscribing to meters never start browser capture.

The helper owns one native management connection. Object identifiers combine its connection
generation with PipeWire's object serial; reconnects invalidate old identifiers. Authoritative state
is broadcast through latest-value channels, and control queues are bounded. A shared atomic control
epoch rejects queued commands from a revoked controller even if helper input is delayed. An operation
already admitted before handoff may complete afterward. Changes not confirmed by native state within
three seconds produce an error.

## Dependencies

The supported baseline is PipeWire 1.4.2 and WirePlumber 0.5.6. The latter introduces the policy and
stateless profile blocks used by this configuration. PipeWire 1.4.2 is the oldest validated native
device and GStreamer implementation; older versions are unsupported. The runtime checks daemon and
policy versions and reports missing elements during pipeline initialization.

| Distribution | Audio packages |
| --- | --- |
| Arch | `pipewire`, `pipewire-pulse`, `wireplumber`, `gst-plugin-pipewire`, `libpulse` |
| Debian/Ubuntu | `pipewire`, `pipewire-pulse`, `wireplumber`, `gstreamer1.0-pipewire`, `pulseaudio-utils` |
| Fedora | `pipewire`, `pipewire-pulseaudio`, `wireplumber`, `pipewire-gstreamer`, `pulseaudio-utils` |

Check versions as well as package names. Audio services are optional for installations using
`--no-audio`; Debian metadata recommends them and Arch metadata lists them as optional dependencies.
The native PipeWire client library is a linked runtime dependency even when audio is disabled.
The Docker image contains the complete audio stack and the application starts it.

Runtime isolation does not guarantee that distribution packages coexist with host PulseAudio.
Arch's `pipewire-pulse` conflicts with `pulseaudio`; Fedora also uses mutually exclusive Pulse server
packages. Debian 13's `pipewire-pulse` conflicts with `pulseaudio-module-gsettings` and ships user
service/socket activation. Check the package transaction and service activation before installing on
a host that uses PulseAudio. The Docker image provides these dependencies without changing host audio
packages. Runtime startup never changes host defaults or invokes a service manager.

## Verification

Run verification inside Docker with the checkout and release build mounted. The lifecycle check is
`crates/elsewhere/checks/audio-lifecycle.py`, taking the release binary as its argument. It covers idle
startup, signal handling, missing services/plugins, service and worker readiness timeouts, and
individual service exits. Failed audio must be cleaned up while the desktop keeps running.
`web/checks/private-audio.mjs` exercises native and Pulse playback/recording through a live browser,
microphone silence transitions, session microphone mute without changing consent, sustained mixer traffic,
malformed packet handling and capability withdrawal.
`crates/elsewhere/checks/audio-isolation.py` checks two desktops alongside an unrelated audio graph, separate
tones and lifecycles, and native/Pulse mpv playback with saved device choices across app restarts. It
also runs `web/checks/mixer-isolation.mjs` against both live sessions to check mixer membership and
foreign-object rejection.
The native meter check is `cargo run --release -p elsewhere --example audio-graph`, also
run inside Docker. It creates its own private services and checks playback, output, microphone and
recording peaks through mute and gain changes, then verifies monitoring nodes disappear when meters
are dropped while the management connection stays open. Output monitors follow channel volume and mute. Recording-stream monitoring is before that
stream's own controls; the check separately verifies muted samples delivered to the recorder.

`cargo run --release -p elsewhere --example audio-mixer` checks native and Pulse controls,
per-stream isolation, real routing to a second output, default selection, object removal, reconnect,
monitor cleanup and cross-process control revocation. `web/checks/session-mixer.mjs` checks the rendered
panel, authoritative controls, three-viewer authorization and shared subscriptions.

The browser check's fake capture source is a test WAV;
production capture still requires browser consent.

Lifecycle and isolation checks pass on PipeWire 1.4.2 with WirePlumber 0.5.6 and 0.5.8, and on PipeWire
1.6.8 with WirePlumber 0.5.17. Direct GStreamer device publication is unsuitable for idle startup; native virtual
nodes keep device lifetime independent of browser capture and application streams.

References: [PipeWire configuration](https://docs.pipewire.org/page_daemon.html),
[native loopback](https://docs.pipewire.org/page_module_loopback.html),
[WirePlumber file isolation](https://pipewire.pages.freedesktop.org/wireplumber/daemon/locations.html),
[WirePlumber 0.5.6 profiles](https://github.com/PipeWire/wireplumber/blob/0.5.6/src/config/wireplumber.conf),
[Fedora Pulse server packaging](https://fedoraproject.org/wiki/Changes/DefaultPipeWire).
