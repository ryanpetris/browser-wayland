# Session audio visualiser

The desktop viewer's audio controls open an expandable visualiser panel. It offers
spectrum bars, a line/area spectrum, a radial spectrum and a stereo spectrum.
Style, colours and the animation switch are local display preferences. Read-only
viewers can use them. Reduced-motion preferences pause animation.

This displays the mixed audio at this viewer's Web Audio playback node, before
browser or system muting. It cannot identify applications or confirm sound at the
speakers. Text distinguishes unavailable audio, waiting for playback permission,
signal and silence. The microphone capture control remains separate.

The renderer loads when playback is available and the panel opens. Its analysis
branch never connects to the speakers. Closing the panel disposes that branch;
hiding the page or entering stage fullscreen, disabling animation and reduced motion
pause drawing and disconnect the analysis input. Playback retains its context and
speaker connection. Animation is capped at 30 fps.

## Build switch

`BW_VISUALISER` defaults to `1`. Set it to `0` to exclude audioMotion:

```sh
BW_VISUALISER=0 make
cd web && BW_VISUALISER=0 npm run build
docker build --build-arg BW_VISUALISER=0 -t browser-wayland .
make docker BW_VISUALISER=0
```

The viewer build needs Node 24 and GNU tar. Run the viewer build before a direct
Cargo or cargo-deb build. `BW_VISUALISER=0 make web` followed by `cargo deb` produces
the disabled Debian package. For Arch, export `BW_VISUALISER=0` before `makepkg`.
Every viewer rebuild clears its output directory, including chunks from a
previous enabled build. Cargo embeds the current emitted assets. The disabled
build has no visualiser imports, chunks, source archive, dependency notices or controls.
Ordinary playback and browser microphone capture work in either variant.

## Licences and source

Original browser-wayland code remains MIT licensed. audioMotion-analyzer 4.5.4 is
AGPL-3.0-or-later; an enabled distribution includes that dependency and must meet
its applicable licence requirements. Excluding it does not change other
dependencies' licences. Debian and tarball distributions include THIRD_PARTY.txt
with the full applicable notice; Docker and Arch install it under the package's
licence directory. Package metadata for our original Rust code remains MIT.

Enabled binaries serve the audioMotion licence and the distributed library
source through links in the panel. They also serve a source archive containing
the corresponding browser-wayland integration, build scripts and dependency
lockfiles, and audioMotion source and licence. The archive is built from the
checkout used to build the viewer. Other dependencies are resolved from the
lockfiles. Rebuild the viewer after changing source before packaging the binary.

The Vite build applies one marked modification to audioMotion: the viewer owns
context resumption, so the library's persistent click listener is omitted.
The downloadable library source contains that modification. The source archive
includes the original dependency source and the build transform that applies it.
MIT integration code does not copy the library implementation.

Downstream enabled distributions must retain required notices and provide
corresponding source for the version they distribute, including their changes.
Preserve the source links for network users. This feature switch is an option to
exclude this dependency, not permission to relicense it.

## Verification

In the Docker image, install Node, npm and Chromium, then run `npm ci` in `web`.
`npm run build && npm run check:visualiser` exercises the emitted viewer in
Chromium. Follow with `BW_VISUALISER=0 npm run build` and
`BW_VISUALISER=0 npm run check:visualiser` to check an enabled-to-disabled rebuild.
The checks cover graph ownership, repeated disposal and click listener counts,
style changes, HiDPI, fullscreen, reduced motion, animation off, delayed audio
initialization, source replacement and absence of disabled renderer assets.

The live check is `node checks/session-audio.mjs`, with `BW_TEST_URL` and
`BW_TEST_TOKEN_FILE` pointing at an isolated Docker desktop and its control token.
It uses finite GStreamer audio and mpv software-Wayland video test signals, then
Chromium's fake microphone device. It terminates test processes whose command
lines match its own signals; use a dedicated rig.

A software-rendered Docker run with a 440 Hz signal decoded 85 video frames in
three seconds both with the panel closed and open, with zero audio underruns.
The measured PCM peak was 0.10113 closed and 0.10112 open. Browser task time was
151 ms/s closed, 132 ms/s open and 160 ms/s closed again. Opening the panel also
shrinks the desktop video viewport, so those task-time samples do not isolate
the renderer's cost. They establish that video and audio continued smoothly in
that rig, not a general performance guarantee. Enabled playback returned to silence. Fake microphone start/stop passed in both
build variants. Restarting
an audio-enabled session with audio disabled cleared the old visualiser and
reported unavailable audio after reconnect.
