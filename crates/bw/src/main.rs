use std::{net::SocketAddr, sync::Arc};

use anyhow::Result;
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
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env().add_directive("info".parse()?))
        .init();
    let cli = Cli::parse();

    let (commands, command_rx) = calloop::channel::channel();
    let (stream_tx, stream_rx) = mpsc::channel(16);

    if !cli.fake_source {
        anyhow::bail!("the compositor isn't wired up yet; run with --fake-source");
    }
    // No compositor: just log what the browser sends.
    std::thread::spawn(move || {
        let mut loop_ = calloop::EventLoop::<()>::try_new().unwrap();
        loop_
            .handle()
            .insert_source(command_rx, |ev, _, _| {
                if let calloop::channel::Event::Msg(cmd) = ev {
                    tracing::info!(?cmd);
                }
            })
            .unwrap();
        loop_.run(None, &mut (), |_| {}).unwrap();
    });
    let stream = Arc::new(bw_stream::fake_source(
        bw_stream::EncodeOpts { bitrate_kbps: cli.bitrate, ..Default::default() },
        stream_tx,
    )?);

    let server = bw_server::Config { listen: cli.listen, tls: !cli.no_tls, data_dir: bw_server::Config::default_data_dir() };
    tokio::runtime::Runtime::new()?.block_on(bw_server::run(server, commands, stream_rx, move || stream.request_keyframe()))
}
