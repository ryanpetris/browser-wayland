use std::{net::SocketAddr, path::PathBuf};

use anyhow::Result;
use elsewhere_core::{Codec, FrameSink, StreamControl};
use clap::Parser;
use tokio::sync::mpsc;

mod audio;

#[derive(Parser)]
#[command(about = "A Wayland compositor whose screen is a browser tab", version = env!("ELSEWHERE_VERSION"))]
struct Cli {
    #[arg(long, hide = true)]
    audio_worker: bool,
    /// Address to serve the viewer on.
    #[arg(long, default_value = "0.0.0.0:8443")]
    listen: SocketAddr,
    /// Plain HTTP (WebCodecs then only works from localhost).
    #[arg(long)]
    no_tls: bool,
    /// Medium quality bitrate ceiling in kbit/s; other quality levels have their own ceilings.
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
    /// Command to run (via `sh -c`) at startup, with WAYLAND_DISPLAY, DISPLAY, PIPEWIRE_REMOTE, PULSE_SERVER
    /// and a Wayland session's environment set for it.
    #[arg(long)]
    exec: Option<String>,
    /// Keep the desktop at WIDTHxHEIGHT pixels and scale it to fit each browser.
    /// Both dimensions must be even and between 2 and 8192.
    #[arg(long, value_name = "WIDTHxHEIGHT", value_parser = parse_screen_size)]
    screen_size: Option<(u32, u32)>,
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
    /// Local UDP port for WebRTC (default: the listen port's number). Without --rtc-addr, viewers use the page's port.
    #[arg(long)]
    rtc_port: Option<u16>,
    /// Advertise this WebRTC IP and --rtc-port instead of the page's hostname and port.
    /// Use when UDP is reached at a different endpoint from HTTPS; forward the UDP port to this server.
    #[arg(long)]
    rtc_addr: Option<std::net::IpAddr>,
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
    /// A v4l2loopback device (`modprobe v4l2loopback exclusive_caps=1 card_label=elsewhere`, then its
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

fn parse_screen_size(value: &str) -> std::result::Result<(u32, u32), String> {
    let invalid = || "expected WIDTHxHEIGHT with even dimensions between 2 and 8192".to_string();
    let (w, h) = value.split_once('x').ok_or_else(invalid)?;
    let dimension = |s: &str| s.parse::<u32>().ok().filter(|n| (2..=8192).contains(n) && n % 2 == 0).ok_or_else(invalid);
    Ok((dimension(w)?, dimension(h)?))
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.audio_worker {
        tracing_subscriber::fmt().with_writer(std::io::stderr).init();
        return audio::worker();
    }
    let stopping = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGINT, stopping.clone())?;
    signal_hook::flag::register(signal_hook::consts::SIGTERM, stopping.clone())?;
    // Headless machines (no session) may lack XDG_RUNTIME_DIR; give the Wayland socket a private home.
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = format!("/tmp/elsewhere-{}", std::fs::metadata("/proc/self")?.uid());
        std::fs::create_dir_all(&dir)?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        // Safety: single-threaded at this point.
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", &dir) };
    }
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env().add_directive("info".parse()?))
        .init();
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
    let va = elsewhere_stream::va_prefix(&cli.render_node);
    let software = cli.software_encoding || render_node.is_none();
    let codecs = elsewhere_stream::codecs(&va, software);
    if software {
        anyhow::ensure!(!codecs.is_empty(), "no software video encoder found (GStreamer's vpx, x264, x265 or svtav1 plugins)");
    } else {
        anyhow::ensure!(!codecs.is_empty(), "no VA-API video encoder found for {} (gst-plugin-va and a driver for this GPU are needed; --software-encoding encodes on the CPU)", cli.render_node.display());
    }
    anyhow::ensure!(codec.is_none_or(|c| codecs.contains(&c)), "no {codec:?} encoder here; available: {codecs:?}");
    tracing::info!(?codecs, software, "video encoders");
    let (audio_tx, audio_rx) = mpsc::channel(16);
    let (events_tx, events_rx) = mpsc::unbounded_channel();

    let mut audio = if cli.no_audio { None } else {
        match audio::Session::start(&stopping, audio_tx) {
            Ok(session) => {
                tracing::info!(environment = ?session.client_env(), "private session audio ready");
                Some(session)
            }
            Err(e) => { tracing::warn!("audio unavailable: {e:#}"); None }
        }
    };
    if stopping.load(std::sync::atomic::Ordering::Relaxed) { return Ok(()); }

    let mut cam = None; // the browser's webcam: its playback pipeline and the frames' way in
    if let Some(device) = &cli.webcam {
        let (cam_tx, cam_rx) = mpsc::channel(16);
        match elsewhere_stream::video_sink(device, cam_rx) {
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
    let sinks: elsewhere_server::SinkFactory = Box::new(move |tx| {
        let sink = elsewhere_stream::GstSink::new(bitrate, &va_for_sinks, software, tx)?;
        Ok((Box::new(sink.clone()) as Box<dyn FrameSink>, Box::new(sink.control()) as Box<dyn StreamControl>))
    });
    let mut exec_env = audio.as_ref().map(audio::Session::client_env).unwrap_or_else(|| {
        // An unavailable session must not send its applications to the host audio server.
        vec![("PIPEWIRE_REMOTE".into(), "/dev/null".into()), ("PULSE_SERVER".into(), "unix:/dev/null".into()), ("PIPEWIRE_CONFIG_DIR".into(), "/dev/null".into())]
    });
    exec_env.push(("ELSEWHERE_WEBCAM_DEVICE".into(), cli.webcam.as_ref().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default()));
    if cli.elements {
        // GTK always publishes its tree; Firefox and Qt only when asked. (Chromium needs --force-renderer-accessibility.)
        exec_env.extend([("GNOME_ACCESSIBILITY", "1"), ("QT_LINUX_ACCESSIBILITY_ALWAYS_ON", "1")].map(|(k, v)| (k.to_string(), v.to_string())));
    }
    let accepted_formats = elsewhere_stream::accepted_formats(&va, software);
    tracing::info!(?accepted_formats, "render target formats the encoders take (fourcc, modifier)");
    let data_dir = elsewhere_server::Config::default_data_dir()?;
    let files_dir = std::path::absolute(cli.files_dir.unwrap_or_else(elsewhere_server::files::default_dir))?;
    let mut initial = elsewhere_core::OutputGeometry {
        // The CPU encoders get every frame at half the rate instead of every other frame.
        refresh_mhz: if software { 30_000 } else { 60_000 },
        ..elsewhere_core::INITIAL_OUTPUT
    };
    if let Some((width, height)) = cli.screen_size {
        initial.width_px = width;
        initial.height_px = height;
    }
    let runtime = tokio::runtime::Runtime::new()?;
    let elsewhere_compositor::CompositorHandle { commands, socket_name, x11_display, join } = elsewhere_compositor::spawn(
        elsewhere_compositor::Config {
            render_node,
            socket_name: cli.socket_name,
            initial,
            exec: cli.exec.clone(),
            exec_env,
            kiosk: cli.kiosk,
            accepted_formats,
        },
        events_tx,
    )?;
    tracing::info!(socket = %socket_name, x11_display = ?x11_display.map(|d| format!(":{d}")), "compositor ready");
    // the compositor ends on Quit (the API, the viewer's power menu) or when it panics
    let (exited_tx, mut exited_rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let _ = exited_tx.send(join.join().is_ok());
    });

    let rtc = (!cli.no_rtc).then(|| {
        let mut ice_servers: Vec<serde_json::Value> = cli.stun.iter().map(|s| serde_json::json!({ "urls": s })).collect();
        if let Some(turn) = &cli.turn {
            ice_servers.push(serde_json::json!({ "urls": turn, "username": cli.turn_user, "credential": cli.turn_pass }));
        }
        elsewhere_server::rtc::Config { port: cli.rtc_port.unwrap_or(cli.listen.port()), addr: cli.rtc_addr, ice_servers }
    });
    let server = elsewhere_server::Config { listen: cli.listen, tls: !cli.no_tls, codec, codecs, software, bitrate_kbps: cli.bitrate, initial, fixed_size: cli.screen_size.is_some(), data_dir, elements: cli.elements, files_dir, version: env!("ELSEWHERE_VERSION"), sinks, audio_available: audio.is_some(), mixer: audio.as_mut().and_then(|session| session.mixer.take()), mic: audio.as_ref().map(|session| session.mic.clone()), cam: cam.as_ref().map(|(_, tx)| tx.clone()), rtc };
    // Ctrl+C and SIGTERM (`docker stop`, a service manager) return here so the audio devices get unloaded
    // and the pipelines stopped.
    let result = runtime.block_on(async {
        let mut health = tokio::time::interval(std::time::Duration::from_millis(100));
        let server = elsewhere_server::run(server, commands.clone(), audio_rx, events_rx);
        tokio::pin!(server);
        let mut compositor_exited = false;
        let result = loop {
        tokio::select! {
            _ = health.tick() => {
                if stopping.load(std::sync::atomic::Ordering::Relaxed) { break Ok(()); }
                if let Some(session) = &mut audio {
                    if let Err(e) = session.check() {
                        tracing::warn!("audio unavailable: {e:#}");
                        audio.take();
                    }
                }
            }
            r = &mut server => break r,
            ok = &mut exited_rx => {
                compositor_exited = true;
                break if ok.unwrap_or(false) { Ok(()) } else { Err(anyhow::anyhow!("the compositor thread died")) };
            },
        }
        };
        if !compositor_exited {
            let _ = commands.send(elsewhere_core::Command::Quit);
            let _ = exited_rx.await;
        }
        result
    });
    drop(audio);
    drop(cam);
    result
}
