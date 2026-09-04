use std::{net::SocketAddr, path::PathBuf};

use anyhow::Result;
use bw_core::{Codec, Command, OutputGeometry, StreamControl};
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
    /// Stream a test pattern instead of running the compositor.
    #[arg(long)]
    fake_source: bool,
    /// Encoder bitrate in kbit/s.
    #[arg(long, default_value_t = 8000)]
    bitrate: u32,
    /// Video codec: auto picks HEVC, VP9 or H.264 by what the browser decodes in hardware.
    #[arg(long, default_value = "auto", value_parser = ["auto", "h264", "hevc", "vp9"])]
    codec: String,
    /// Command to run (via `sh -c`) at startup; WAYLAND_DISPLAY, DISPLAY, PULSE_SINK and
    /// BW_WIDTH/BW_HEIGHT (the output size: 1920×1080 until a viewer connects) are set for it.
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
        _ => None,
    };
    let (stream_tx, stream_rx) = mpsc::channel(16);
    let (events_tx, events_rx) = mpsc::unbounded_channel();

    let mut audio = None;
    if !cli.fake_source && !cli.no_audio {
        match AudioSink::create().and_then(|sink| bw_stream::audio_source(&format!("{}.monitor", sink.name), stream_tx.clone()).map(|s| (sink, s))) {
            Ok((sink, stream)) => {
                tracing::info!("audio: clients started with PULSE_SINK={} play in the browser", sink.name);
                audio = Some((sink, stream));
            }
            Err(e) => tracing::warn!("audio disabled: {e:#}"),
        }
    }

    let (commands, control): (calloop::channel::Sender<Command>, Box<dyn StreamControl>) = if cli.fake_source {
        // No compositor: just log what the browser sends.
        let (commands, rx) = calloop::channel::channel();
        std::thread::spawn(move || {
            let mut event_loop = calloop::EventLoop::<()>::try_new().unwrap();
            event_loop
                .handle()
                .insert_source(rx, |ev, _, _| {
                    if let calloop::channel::Event::Msg(cmd) = ev {
                        tracing::info!(?cmd);
                    }
                })
                .unwrap();
            event_loop.run(None, &mut (), |_| {}).unwrap();
        });
        (commands, Box::new(bw_stream::fake_source(cli.bitrate, codec.unwrap_or(Codec::H264), stream_tx)?))
    } else {
        let sink = bw_stream::GstSink::new(cli.bitrate, stream_tx)?;
        let mut exec_env: Vec<(String, String)> = audio.as_ref().map(|(sink, _)| ("PULSE_SINK".to_string(), sink.name.clone())).into_iter().collect();
        if cli.elements {
            // GTK always publishes its tree; Firefox and Qt only when asked. (Chromium needs --force-renderer-accessibility.)
            exec_env.extend([("GNOME_ACCESSIBILITY", "1"), ("QT_LINUX_ACCESSIBILITY_ALWAYS_ON", "1")].map(|(k, v)| (k.to_string(), v.to_string())));
        }
        let bw_compositor::CompositorHandle { commands, socket_name, x11_display, join } = bw_compositor::spawn(
            bw_compositor::Config {
                render_node: cli.render_node,
                socket_name: cli.socket_name,
                initial: OutputGeometry { width_px: 1920, height_px: 1080, scale: 1.0, refresh_mhz: 60_000 },
                exec: cli.exec.clone(),
                exec_env,
                kiosk: cli.kiosk,
            },
            Box::new(sink.clone()),
            events_tx,
        )?;
        tracing::info!(socket = %socket_name, x11_display = ?x11_display.map(|d| format!(":{d}")), "compositor ready");
        std::thread::spawn(move || {
            let _ = join.join();
            tracing::error!("compositor thread exited; shutting down");
            std::process::exit(1);
        });
        (commands, Box::new(sink))
    };

    // the fake source can't switch codecs, so its policy is whatever it was built with
    let codec = if cli.fake_source { Some(codec.unwrap_or(Codec::H264)) } else { codec };
    let server = bw_server::Config { listen: cli.listen, tls: !cli.no_tls, codec, data_dir: bw_server::Config::default_data_dir()?, elements: cli.elements, version: env!("BW_VERSION") };
    // Ctrl+C returns here so the audio sink gets unloaded and the pipelines stopped.
    let result = tokio::runtime::Runtime::new()?.block_on(async {
        tokio::select! {
            r = bw_server::run(server, commands, stream_rx, events_rx, control) => r,
            _ = tokio::signal::ctrl_c() => Ok(()),
        }
    });
    drop(audio);
    result
}
