use std::{net::SocketAddr, path::PathBuf};

use anyhow::Result;
use bw_core::{Codec, FrameSink, StreamControl};
use clap::Parser;
use tokio::sync::mpsc;

#[derive(Parser)]
#[command(about = "A Wayland compositor whose screen is a browser tab", version = env!("BW_VERSION"))]
struct Cli {
    /// Address to serve the viewer on.
    #[arg(long, default_value = "0.0.0.0:8443")]
    listen: SocketAddr,
    /// Plain HTTP (WebCodecs then only works from localhost).
    #[arg(long)]
    no_tls: bool,
    /// Encoder bitrate in kbit/s.
    #[arg(long, default_value_t = 8000)]
    bitrate: u32,
    /// Video codec: auto picks AV1, HEVC, VP9 or H.264 (or VP8 with --software-encoding) by what the
    /// browser decodes in hardware, among what this machine encodes.
    #[arg(long, default_value = "auto", value_parser = ["auto", "h264", "hevc", "vp9", "av1", "vp8"])]
    codec: String,
    /// Encode on the CPU (libvpx, x264, x265, SVT-AV1: whichever is installed) instead of with VA-API,
    /// for machines without a usable GPU encoder. Slower; the desktop runs at 30 Hz.
    #[arg(long)]
    software_encoding: bool,
    /// Command to run (via `sh -c`) at startup, with WAYLAND_DISPLAY, DISPLAY, PULSE_SINK, PULSE_SOURCE
    /// and a Wayland session's environment set for it.
    #[arg(long)]
    exec: Option<String>,
    /// Fullscreen every window: for running a nested desktop such as
    /// `--exec 'dbus-run-session -- gnome-shell --devkit'`.
    #[arg(long)]
    kiosk: bool,
    /// The GPU's render node. `none`, or the default node not being there, renders with Mesa's llvmpipe
    /// (no GPU at all: a VPS, a container without devices) and encodes in software.
    #[arg(long, default_value = DEFAULT_RENDER_NODE)]
    render_node: PathBuf,
    #[arg(long, default_value = "wayland-browser")]
    socket_name: String,
    /// No audio either way: neither the clients' for the browser nor the browser's microphone for them.
    #[arg(long)]
    no_audio: bool,
    /// No WebRTC: the video stays on the WebSocket (TCP) for every viewer.
    #[arg(long)]
    no_rtc: bool,
    /// UDP port for the WebRTC data channels (default: the listen port's number), on every local address.
    #[arg(long)]
    rtc_port: Option<u16>,
    /// A STUN server for the browsers (`stun:host:3478`); none means host candidates only, enough on a LAN.
    #[arg(long)]
    stun: Vec<String>,
    /// A TURN server for browsers behind a strict NAT (`turn:host:3478`), with its credentials.
    #[arg(long)]
    turn: Option<String>,
    #[arg(long, requires = "turn")]
    turn_user: Option<String>,
    #[arg(long, requires = "turn")]
    turn_pass: Option<String>,
    /// A v4l2loopback device (`modprobe v4l2loopback exclusive_caps=1 card_label=browser-wayland`, then its
    /// /dev/videoN) that the browser's webcam is played into, for applications to use as a camera.
    #[arg(long)]
    webcam: Option<PathBuf>,
    /// Serve each window's UI elements (roles, names, rectangles) on /api/windows/{id}/elements, read from
    /// the toolkits' accessibility trees over the D-Bus session this process was started in.
    #[arg(long)]
    elements: bool,
    /// Where files dropped on the page land and the page's downloads come from (default: the XDG
    /// download directory, `~/Downloads`).
    #[arg(long)]
    files_dir: Option<PathBuf>,
}

const DEFAULT_RENDER_NODE: &str = "/dev/dri/renderD128";

/// A private PulseAudio/PipeWire sink for this instance's clients; its monitor is what we stream.
/// Per process, so two instances never hear each other; unloaded on drop.
struct AudioSink {
    name: String,
    module: String,
}

/// `pactl load-module`, returning the module's id.
fn load_module(args: &[&str]) -> Result<String> {
    let out = std::process::Command::new("pactl").arg("load-module").args(args).output()?;
    anyhow::ensure!(out.status.success(), "pactl load-module failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

impl AudioSink {
    fn create() -> Result<AudioSink> {
        let name = format!("browser-wayland-{}", std::process::id());
        let module = load_module(&["module-null-sink", &format!("sink_name={name}"), &format!("sink_properties=device.description={name}")])?;
        Ok(AudioSink { name, module })
    }
}

impl Drop for AudioSink {
    fn drop(&mut self) {
        let _ = std::process::Command::new("pactl").args(["unload-module", &self.module]).status();
    }
}

/// The browser's microphone as a source applications can record from: a second null sink the Opus
/// packets play into, and a remap of its monitor, so it is a real source (with a name and a description),
/// not a monitor. Unloaded on drop, in reverse.
struct MicSource {
    /// The source's name (`PULSE_SOURCE` for clients) and the sink's (`pulsesink device=`).
    name: String,
    sink: String,
    modules: Vec<String>,
}

impl MicSource {
    fn create() -> Result<MicSource> {
        let sink = format!("browser-wayland-mic-{}", std::process::id());
        let name = format!("browser-wayland-microphone-{}", std::process::id());
        let m1 = load_module(&["module-null-sink", &format!("sink_name={sink}"), &format!("sink_properties=device.description={sink}")])?;
        let mut mic = MicSource { name: name.clone(), sink: sink.clone(), modules: vec![m1] };
        let m2 = load_module(&["module-remap-source", &format!("master={sink}.monitor"), &format!("source_name={name}"), &format!("source_properties=device.description={name}")])?;
        mic.modules.push(m2);
        Ok(mic)
    }
}

impl Drop for MicSource {
    fn drop(&mut self) {
        for module in self.modules.iter().rev() {
            let _ = std::process::Command::new("pactl").args(["unload-module", module]).status();
        }
    }
}

fn main() -> Result<()> {
    // Headless machines (no session) may lack XDG_RUNTIME_DIR; give the Wayland socket a private home.
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = format!("/tmp/browser-wayland-{}", std::fs::metadata("/proc/self")?.uid());
        std::fs::create_dir_all(&dir)?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        // Safety: single-threaded at this point.
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", &dir) };
    }
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env().add_directive("info".parse()?))
        .init();
    let cli = Cli::parse();
    let codec = match cli.codec.as_str() {
        "h264" => Some(Codec::H264),
        "hevc" => Some(Codec::Hevc),
        "vp9" => Some(Codec::Vp9),
        "av1" => Some(Codec::Av1),
        "vp8" => Some(Codec::Vp8),
        _ => None,
    };
    // a node given by hand must be there; only the default one may be missing (a machine without a GPU)
    let render_node = match cli.render_node.as_os_str().to_str() {
        Some("none") => None,
        _ if cli.render_node.exists() => Some(cli.render_node.clone()),
        Some(DEFAULT_RENDER_NODE) => None,
        _ => anyhow::bail!("render node {} isn't there (--render-node none renders without a GPU)", cli.render_node.display()),
    };
    if render_node.is_none() {
        tracing::info!("no GPU ({}): rendering in software, encoding in software", cli.render_node.display());
    }
    let va = bw_stream::va_prefix(&cli.render_node);
    let software = cli.software_encoding || render_node.is_none();
    let codecs = bw_stream::codecs(&va, software);
    if software {
        anyhow::ensure!(!codecs.is_empty(), "no software video encoder found (GStreamer's vpx, x264, x265 or svtav1 plugins)");
    } else {
        anyhow::ensure!(!codecs.is_empty(), "no VA-API video encoder found for {} (gst-plugin-va and a driver for this GPU are needed; --software-encoding encodes on the CPU)", cli.render_node.display());
    }
    anyhow::ensure!(codec.is_none_or(|c| codecs.contains(&c)), "no {codec:?} encoder here; available: {codecs:?}");
    tracing::info!(?codecs, software, "video encoders");
    let (audio_tx, audio_rx) = mpsc::channel(16);
    let (events_tx, events_rx) = mpsc::unbounded_channel();

    // pipelines first in these pairs: dropped before the devices they play into or capture are unloaded
    let mut audio = None;
    let mut mic = None; // the browser's microphone: its playback pipeline, the source, and the packets' way in
    if !cli.no_audio {
        match AudioSink::create().and_then(|sink| bw_stream::audio_source(&format!("{}.monitor", sink.name), audio_tx).map(|s| (sink, s))) {
            Ok((sink, stream)) => {
                tracing::info!("audio: clients started with PULSE_SINK={} play in the browser", sink.name);
                audio = Some((stream, sink));
            }
            Err(e) => tracing::warn!("audio disabled: {e:#}"),
        }
        let (mic_tx, mic_rx) = mpsc::channel(64);
        match MicSource::create().and_then(|source| bw_stream::audio_sink(&source.sink, mic_rx).map(|s| (source, s))) {
            Ok((source, stream)) => {
                tracing::info!("microphone: clients started with PULSE_SOURCE={} hear the browser's", source.name);
                mic = Some((stream, source, mic_tx));
            }
            Err(e) => tracing::warn!("microphone disabled: {e:#}"),
        }
    }

    let mut cam = None; // the browser's webcam: its playback pipeline and the frames' way in
    if let Some(device) = &cli.webcam {
        let (cam_tx, cam_rx) = mpsc::channel(16);
        match bw_stream::video_sink(device, cam_rx) {
            Ok(stream) => {
                tracing::info!("webcam: the browser's camera plays into {}", device.display());
                cam = Some((stream, cam_tx));
            }
            Err(e) => tracing::warn!("webcam disabled: {e:#}"),
        }
    }

    // one encoder per viewer and per window stream, made when the session starts
    let bitrate = cli.bitrate;
    let va_for_sinks = va.clone();
    let sinks: bw_server::SinkFactory = Box::new(move |tx| {
        let sink = bw_stream::GstSink::new(bitrate, &va_for_sinks, software, tx)?;
        Ok((Box::new(sink.clone()) as Box<dyn FrameSink>, Box::new(sink.control()) as Box<dyn StreamControl>))
    });
    let mut exec_env: Vec<(String, String)> = audio.as_ref().map(|(_, sink)| ("PULSE_SINK".to_string(), sink.name.clone())).into_iter().collect();
    exec_env.extend(mic.as_ref().map(|(_, source, _)| ("PULSE_SOURCE".to_string(), source.name.clone())));
    if cli.elements {
        // GTK always publishes its tree; Firefox and Qt only when asked. (Chromium needs --force-renderer-accessibility.)
        exec_env.extend([("GNOME_ACCESSIBILITY", "1"), ("QT_LINUX_ACCESSIBILITY_ALWAYS_ON", "1")].map(|(k, v)| (k.to_string(), v.to_string())));
    }
    let accepted_formats = bw_stream::accepted_formats(&va, software);
    tracing::info!(?accepted_formats, "render target formats the encoders take (fourcc, modifier)");
    let bw_compositor::CompositorHandle { commands, socket_name, x11_display, join } = bw_compositor::spawn(
        bw_compositor::Config {
            render_node,
            socket_name: cli.socket_name,
            // the CPU encoders get every frame at half the rate instead of every other frame
            initial: bw_core::OutputGeometry { refresh_mhz: if software { 30_000 } else { 60_000 }, ..bw_core::INITIAL_OUTPUT },
            exec: cli.exec.clone(),
            exec_env,
            kiosk: cli.kiosk,
            accepted_formats,
        },
        events_tx,
    )?;
    tracing::info!(socket = %socket_name, x11_display = ?x11_display.map(|d| format!(":{d}")), "compositor ready");
    // the compositor ends on Quit (the API, the viewer's power menu) or when it panics
    let (exited_tx, exited_rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let _ = exited_tx.send(join.join().is_ok());
    });

    let rtc = (!cli.no_rtc).then(|| {
        let mut ice_servers: Vec<serde_json::Value> = cli.stun.iter().map(|s| serde_json::json!({ "urls": s })).collect();
        if let Some(turn) = &cli.turn {
            ice_servers.push(serde_json::json!({ "urls": turn, "username": cli.turn_user, "credential": cli.turn_pass }));
        }
        bw_server::rtc::Config { port: cli.rtc_port.unwrap_or(cli.listen.port()), ice_servers }
    });
    let server = bw_server::Config { listen: cli.listen, tls: !cli.no_tls, codec, codecs, software, bitrate_kbps: cli.bitrate, refresh_mhz: if software { 30_000 } else { 60_000 }, data_dir: bw_server::Config::default_data_dir()?, elements: cli.elements, files_dir: std::path::absolute(cli.files_dir.unwrap_or_else(bw_server::files::default_dir))?, version: env!("BW_VERSION"), sinks, mic: mic.as_ref().map(|(_, _, tx)| tx.clone()), cam: cam.as_ref().map(|(_, tx)| tx.clone()), rtc };
    // Ctrl+C and SIGTERM (`docker stop`, a service manager) return here so the audio devices get unloaded
    // and the pipelines stopped.
    let result = tokio::runtime::Runtime::new()?.block_on(async {
        let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            r = bw_server::run(server, commands, audio_rx, events_rx) => r,
            _ = tokio::signal::ctrl_c() => Ok(()),
            _ = terminate.recv() => Ok(()),
            ok = exited_rx => if ok.unwrap_or(false) { Ok(()) } else { Err(anyhow::anyhow!("the compositor thread died")) },
        }
    });
    drop(audio);
    drop(mic);
    drop(cam);
    result
}
