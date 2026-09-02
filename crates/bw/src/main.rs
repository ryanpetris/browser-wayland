use std::{net::SocketAddr, path::PathBuf};

use anyhow::Result;
use bw_core::{Command, OutputGeometry};
use clap::Parser;
use tokio::sync::mpsc;

#[derive(Parser)]
#[command(about = "A Wayland compositor whose screen is a browser tab")]
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
    /// Command to run (via `sh -c`) with WAYLAND_DISPLAY pointing at this compositor.
    #[arg(long)]
    exec: Option<String>,
    #[arg(long, default_value = "/dev/dri/renderD128")]
    render_node: PathBuf,
    #[arg(long, default_value = "wayland-browser")]
    socket_name: String,
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
    let (stream_tx, stream_rx) = mpsc::channel(16);
    let (events_tx, events_rx) = mpsc::unbounded_channel();

    let (commands, request_keyframe): (calloop::channel::Sender<Command>, Box<dyn Fn() + Send + Sync>) = if cli.fake_source {
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
        let stream = bw_stream::fake_source(cli.bitrate, stream_tx)?;
        (commands, Box::new(move || stream.request_keyframe()))
    } else {
        let sink = bw_stream::GstSink::new(cli.bitrate, stream_tx)?;
        let bw_compositor::CompositorHandle { commands, socket_name, join } = bw_compositor::spawn(
            bw_compositor::Config {
                render_node: cli.render_node,
                socket_name: cli.socket_name,
                initial: OutputGeometry { width_px: 1920, height_px: 1080, scale: 1.0, refresh_mhz: 60_000 },
            },
            Box::new(sink.clone()),
            events_tx,
        )?;
        tracing::info!(socket = %socket_name, "compositor ready");
        std::thread::spawn(move || {
            let _ = join.join();
            tracing::error!("compositor thread exited; shutting down");
            std::process::exit(1);
        });
        if let Some(cmd) = &cli.exec {
            let mut child = std::process::Command::new("sh")
                .arg("-c")
                .arg(cmd)
                .env("WAYLAND_DISPLAY", &socket_name)
                .env_remove("DISPLAY")
                .env_remove("WAYLAND_SOCKET")
                .env("GDK_BACKEND", "wayland")
                .env("QT_QPA_PLATFORM", "wayland")
                .env("SDL_VIDEODRIVER", "wayland")
                .env("MOZ_ENABLE_WAYLAND", "1")
                .spawn()?;
            std::thread::spawn(move || tracing::info!(status = ?child.wait(), "--exec client exited"));
        }
        (commands, Box::new(move || sink.request_keyframe()))
    };

    let server = bw_server::Config { listen: cli.listen, tls: !cli.no_tls, data_dir: bw_server::Config::default_data_dir()? };
    tokio::runtime::Runtime::new()?.block_on(bw_server::run(server, commands, stream_rx, events_rx, request_keyframe))
}
