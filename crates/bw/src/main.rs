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
    /// for machines without a usable GPU encoder. Slower, and capped at 30 fps.
    #[arg(long)]
    software_encoding: bool,
    /// Command to run (via `sh -c`) at startup, with WAYLAND_DISPLAY, DISPLAY, PULSE_SINK and a
    /// Wayland session's environment set for it.
    #[arg(long)]
    exec: Option<String>,
    /// Fullscreen every window: for running a nested desktop such as
    /// `--exec 'dbus-run-session -- gnome-shell --devkit'`.
    #[arg(long)]
    kiosk: bool,
    #[arg(long, default_value = "/dev/dri/renderD128")]
    render_node: PathBuf,
    #[arg(long, default_value = "wayland-browser")]
    socket_name: String,
    /// Don't capture the clients' audio for the browser.
    #[arg(long)]
    no_audio: bool,
    /// Serve each window's UI elements (roles, names, rectangles) on /api/windows/{id}/elements, read from
    /// the toolkits' accessibility trees over the D-Bus session this process was started in.
    #[arg(long)]
    elements: bool,
    /// Where files dropped on the page land and the page's downloads come from (default: the XDG
    /// download directory, `~/Downloads`).
    #[arg(long)]
    files_dir: Option<PathBuf>,
}

/// A private PulseAudio/PipeWire sink for this instance's clients; its monitor is what we stream.
/// Per process, so two instances never hear each other; unloaded on drop.
struct AudioSink {
    name: String,
    module: String,
}

impl AudioSink {
    fn create() -> Result<AudioSink> {
        let name = format!("browser-wayland-{}", std::process::id());
        let out = std::process::Command::new("pactl")
            .args(["load-module", "module-null-sink", &format!("sink_name={name}"), &format!("sink_properties=device.description={name}")])
            .output()?;
        anyhow::ensure!(out.status.success(), "pactl load-module failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        Ok(AudioSink { name, module: String::from_utf8_lossy(&out.stdout).trim().to_string() })
    }
}

impl Drop for AudioSink {
    fn drop(&mut self) {
        let _ = std::process::Command::new("pactl").args(["unload-module", &self.module]).status();
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
    let va = bw_stream::va_prefix(&cli.render_node);
    let software = cli.software_encoding;
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

    let mut audio = None;
    if !cli.no_audio {
        match AudioSink::create().and_then(|sink| bw_stream::audio_source(&format!("{}.monitor", sink.name), audio_tx).map(|s| (sink, s))) {
            Ok((sink, stream)) => {
                tracing::info!("audio: clients started with PULSE_SINK={} play in the browser", sink.name);
                audio = Some((sink, stream));
            }
            Err(e) => tracing::warn!("audio disabled: {e:#}"),
        }
    }

    // one encoder per viewer and per window stream, made when the session starts
    let bitrate = cli.bitrate;
    let va_for_sinks = va.clone();
    let sinks: bw_server::SinkFactory = Box::new(move |tx| {
        let sink = bw_stream::GstSink::new(bitrate, &va_for_sinks, software, tx)?;
        Ok((Box::new(sink.clone()) as Box<dyn FrameSink>, Box::new(sink.control()) as Box<dyn StreamControl>))
    });
    let mut exec_env: Vec<(String, String)> = audio.as_ref().map(|(sink, _)| ("PULSE_SINK".to_string(), sink.name.clone())).into_iter().collect();
    if cli.elements {
        // GTK always publishes its tree; Firefox and Qt only when asked. (Chromium needs --force-renderer-accessibility.)
        exec_env.extend([("GNOME_ACCESSIBILITY", "1"), ("QT_LINUX_ACCESSIBILITY_ALWAYS_ON", "1")].map(|(k, v)| (k.to_string(), v.to_string())));
    }
    let accepted_formats = bw_stream::accepted_formats(&va, software);
    tracing::info!(?accepted_formats, "vapostproc dmabuf import formats (fourcc, modifier)");
    let bw_compositor::CompositorHandle { commands, socket_name, x11_display, join } = bw_compositor::spawn(
        bw_compositor::Config {
            render_node: cli.render_node,
            socket_name: cli.socket_name,
            initial: bw_core::INITIAL_OUTPUT,
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

    let server = bw_server::Config { listen: cli.listen, tls: !cli.no_tls, codec, codecs, data_dir: bw_server::Config::default_data_dir()?, elements: cli.elements, files_dir: cli.files_dir.unwrap_or_else(bw_server::files::default_dir), version: env!("BW_VERSION"), sinks };
    // Ctrl+C returns here so the audio sink gets unloaded and the pipelines stopped.
    let result = tokio::runtime::Runtime::new()?.block_on(async {
        tokio::select! {
            r = bw_server::run(server, commands, audio_rx, events_rx) => r,
            _ = tokio::signal::ctrl_c() => Ok(()),
            ok = exited_rx => if ok.unwrap_or(false) { Ok(()) } else { Err(anyhow::anyhow!("the compositor thread died")) },
        }
    });
    drop(audio);
    result
}
