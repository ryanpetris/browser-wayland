//! Private audio services. Pipelines must stop before this owner is dropped.
#[path = "audio/meter.rs"]
pub mod meter;

use anyhow::{Context, Result, bail, ensure};
use std::{
    fs,
    io::{Read, Seek, Write},
    path::Path,
    process::{Child, Command, Stdio},
    sync::{Arc, atomic::{AtomicBool, Ordering}},
    time::{Duration, Instant},
};

const OUTPUT: &str = "browser-wayland-output";
const MICROPHONE: &str = "browser-wayland-microphone";
const MICROPHONE_INPUT: &str = "browser-wayland-microphone-input";

pub struct Services {
    children: Vec<(&'static str, Child)>,
    env: Vec<(String, String)>,
    directory: tempfile::TempDir,
    stopping: Arc<AtomicBool>,
}

impl Services {
    pub fn start(stopping: &Arc<AtomicBool>) -> Result<Self> {
        let directory = tempfile::Builder::new().prefix("bw-audio-").tempdir_in("/tmp")?;
        let root = directory.path();
        let mut services = Self {
            children: Vec::new(),
            env: [
                ("PIPEWIRE_RUNTIME_DIR", root.to_path_buf()),
                // Absolute selectors override any runtime directory inherited by clients.
                ("PIPEWIRE_REMOTE", root.join("pipewire-0")),
                ("PIPEWIRE_CONFIG_DIR", root.to_path_buf()),
                ("WIREPLUMBER_CONFIG_DIR", root.to_path_buf()),
                ("PULSE_RUNTIME_PATH", root.join("pulse")),
                ("XDG_CONFIG_HOME", root.join("config")),
                ("XDG_STATE_HOME", root.join("state")),
            ].into_iter().map(|(k, v)| (k.to_owned(), v.to_string_lossy().into_owned())).collect(),
            directory,
            stopping: stopping.clone(),
        };
        services.env.push(("PULSE_SERVER".into(), format!("unix:{}", services.directory.path().join("pulse/native").display())));
        let root = services.directory.path();
        fs::write(root.join("pipewire.conf"), include_str!("audio/pipewire.conf"))?;
        fs::write(root.join("pipewire-pulse.conf"), include_str!("audio/pipewire-pulse.conf"))?;
        // Distribution policy and client modules, without host configuration fragments.
        fs::copy("/usr/share/pipewire/client.conf", root.join("client.conf"))
            .context("PipeWire client configuration is required")?;
        let policy = fs::read_to_string("/usr/share/wireplumber/wireplumber.conf")
            .context("WirePlumber 0.5.6 or later and its distribution policy are required")?;
        fs::write(root.join("wireplumber.conf"), format!("{policy}\n{}", include_str!("audio/wireplumber.conf")))?;
        let deadline = Instant::now() + Duration::from_secs(8);
        for (program, minimum) in [("pipewire", [1, 4, 2]), ("wireplumber", [0, 5, 6])] {
            let version = services.output(program, &["--version"], deadline)?;
            let version = version.split_whitespace().filter_map(|s| {
                let parts: Vec<u32> = s.split('.').map(str::parse).collect::<std::result::Result<_, _>>().ok()?;
                <[u32; 3]>::try_from(parts).ok()
            }).next_back().context("unrecognized audio dependency version")?;
            ensure!(version >= minimum, "{program} {}.{}.{} or later is required", minimum[0], minimum[1], minimum[2]);
        }
        for (name, args) in [("pipewire", &[][..]), ("wireplumber", &["--profile=browser-wayland"][..]), ("pipewire-pulse", &[][..])] {
            let log = fs::File::create(root.join(format!("{name}.log")))?;
            let child = services.command(name).args(args).stdout(log.try_clone()?).stderr(log).spawn()
                .with_context(|| format!("starting {name}; install PipeWire, pipewire-pulse and WirePlumber"))?;
            services.children.push((name, child));
        }
        let mut last_error = String::new();
        while Instant::now() < deadline {
            ensure!(!stopping.load(Ordering::Relaxed), "audio startup cancelled");
            services.check()?;
            match services.ready(deadline) {
                Ok(true) => return Ok(services),
                Ok(false) => last_error = "waiting for private devices and default routing".into(),
                Err(e) => last_error = format!("{e:#}"),
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        bail!("private audio startup timed out: {last_error}; {}", services.logs());
    }

    fn command(&self, program: &str) -> Command {
        use std::os::unix::process::CommandExt;
        let mut command = Command::new(program);
        command.process_group(0).envs(self.env.iter().map(|(k, v)| (k, v))).env("LC_ALL", "C").stdin(Stdio::null());
        for key in ["PIPEWIRE_CONFIG_PREFIX", "PIPEWIRE_CONFIG_NAME", "PIPEWIRE_NODE", "PULSE_SINK", "PULSE_SOURCE", "WIREPLUMBER_DATA_DIR"] {
            command.env_remove(key);
        }
        command
    }

    fn output(&self, program: &str, args: &[&str], deadline: Instant) -> Result<String> {
        // A file avoids filling a pipe while waiting for a bounded probe to exit.
        let mut output = tempfile::tempfile()?;
        let mut child = self.command(program).args(args).stdout(output.try_clone()?).stderr(output.try_clone()?).spawn()
            .with_context(|| format!("starting audio readiness probe {program}"))?;
        let status = loop {
            if let Some(status) = child.try_wait()? { break status; }
            if Instant::now() >= deadline || self.stopping.load(Ordering::Relaxed) {
                let _ = child.kill();
                let _ = child.wait();
                bail!("{program} readiness probe timed out or was cancelled");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        output.rewind()?;
        let mut text = String::new();
        output.take(2 * 1024 * 1024).read_to_string(&mut text)?;
        ensure!(status.success(), "{program}: {}", text.trim());
        Ok(text)
    }

    fn ready(&self, deadline: Instant) -> Result<bool> {
        let graph: serde_json::Value = serde_json::from_str(&self.output("pw-dump", &[], deadline)?)?;
        let Some(objects) = graph.as_array() else { return Ok(false); };
        for name in [OUTPUT, MICROPHONE, MICROPHONE_INPUT] {
            if !objects.iter().any(|o| o["type"] == "PipeWire:Interface:Node" && o["info"]["props"]["node.name"] == name) {
                return Ok(false);
            }
        }
        let pulse = self.output("pactl", &["info"], deadline)?;
        Ok(pulse.lines().any(|line| line == format!("Default Sink: {OUTPUT}"))
            && pulse.lines().any(|line| line == format!("Default Source: {MICROPHONE}")))
    }

    pub fn check(&mut self) -> Result<()> {
        for (name, child) in &mut self.children {
            if let Some(status) = child.try_wait()? {
                let name = *name;
                bail!("private {name} exited ({status}); {}", self.logs());
            }
        }
        Ok(())
    }

    fn logs(&self) -> String {
        self.children.iter().filter_map(|(name, _)| {
            let mut log = fs::File::open(self.directory.path().join(format!("{name}.log"))).ok()?;
            let start = log.metadata().ok()?.len().saturating_sub(4096);
            log.seek(std::io::SeekFrom::Start(start)).ok()?;
            let mut bytes = Vec::new();
            log.take(4096).read_to_end(&mut bytes).ok()?;
            let text = String::from_utf8_lossy(&bytes);
            let mut lines = text.lines().rev().take(4).collect::<Vec<_>>();
            lines.reverse();
            Some(format!("{name}: {}", lines.join("; ")))
        }).collect::<Vec<_>>().join("; ")
    }

    #[cfg(test)]
    fn remote(&self) -> &Path { self.directory.path() }

    pub fn client_env(&self) -> Vec<(String, String)> {
        self.env.iter().filter(|(k, _)| matches!(k.as_str(), "PIPEWIRE_REMOTE" | "PULSE_SERVER" | "PIPEWIRE_CONFIG_DIR")).cloned().collect()
    }
}

impl Drop for Services {
    fn drop(&mut self) {
        for (_, child) in self.children.iter_mut().rev() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires the Docker audio rig"]
    fn private_services_have_independent_defaults_and_cleanup() -> Result<()> {
        let stopping = Arc::new(AtomicBool::new(false));
        let first = Services::start(&stopping)?;
        let mut second = Services::start(&stopping)?;
        let first_root = first.remote().to_owned();
        assert_ne!(first.remote(), second.remote());
        let deadline = Instant::now() + Duration::from_secs(3);
        let graph = first.output("pw-dump", &[], deadline)?;
        let graph: serde_json::Value = serde_json::from_str(&graph)?;
        assert_eq!(graph.as_array().unwrap().iter().filter(|o| o["type"] == "PipeWire:Interface:Node").count(), 4);
        drop(first);
        assert!(!first_root.exists());
        second.check()?;
        assert!(second.ready(Instant::now() + Duration::from_secs(3))?);
        let second_root = second.remote().to_owned();
        drop(second);
        assert!(!second_root.exists());
        Ok(())
    }
}

// Field order stops the pipeline process before its devices and services.
pub struct Session {
    worker: Worker,
    services: Services,
    pub mic: tokio::sync::mpsc::Sender<bw_core::Bytes>,
}

struct Worker {
    child: Child,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    writer: Option<std::thread::JoinHandle<()>>,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl Session {
    pub fn start(stopping: &Arc<AtomicBool>, audio: tokio::sync::mpsc::Sender<bw_core::StreamMsg>) -> Result<Self> {
        let mut services = Services::start(stopping)?;
        let child = services.command(std::env::current_exe()?.to_str().context("executable path")?)
            .arg("--audio-worker").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit()).spawn()?;
        let mut worker = Worker { child, stop: None, writer: None, reader: None };
        let mut stdout = worker.child.stdout.take().context("audio worker output")?;
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        worker.reader = Some(std::thread::Builder::new().name("audio-output".into()).spawn(move || {
            let mut ready = [0];
            if stdout.read_exact(&mut ready).is_err() || ready != [1] { return; }
            let _ = ready_tx.send(());
            loop {
                let mut pts = [0; 8];
                if stdout.read_exact(&mut pts).is_err() { break; }
                let Ok(data) = read_packet(&mut stdout) else { break; };
                if data.is_empty() { break; }
                let _ = audio.try_send(bw_core::StreamMsg::Audio { pts_us: u64::from_le_bytes(pts), data: data.into() });
            }
        })?);
        let mut stdin = worker.child.stdin.take().context("audio worker input")?;
        let (mic, mut packets) = tokio::sync::mpsc::channel::<bw_core::Bytes>(64);
        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel();
        let runtime = tokio::runtime::Builder::new_current_thread().build()?;
        worker.stop = Some(stop_tx);
        worker.writer = Some(std::thread::Builder::new().name("microphone-input".into()).spawn(move || runtime.block_on(async {
            loop {
                let packet = tokio::select! {
                    _ = &mut stop_rx => None,
                    packet = packets.recv() => packet,
                };
                let Some(packet) = packet else {
                    let _ = write_packet(&mut stdin, &[]);
                    break;
                };
                if write_packet(&mut stdin, &packet).is_err() { break; }
            }
        }))?);
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            services.check()?;
            worker_alive(&mut worker, stopping)?;
            match ready_rx.try_recv() {
                Ok(()) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => bail!("audio pipeline initialization failed; see audio worker error"),
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
            ensure!(Instant::now() < deadline, "native audio pipeline startup timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(Self { worker, services, mic })
    }

    pub fn check(&mut self) -> Result<()> {
        self.services.check()?;
        ensure!(self.worker.child.try_wait()?.is_none(), "audio pipeline process exited");
        ensure!(!self.mic.is_closed(), "microphone transport ended");
        Ok(())
    }

    pub fn client_env(&self) -> Vec<(String, String)> { self.services.client_env() }
}

fn worker_alive(worker: &mut Worker, stopping: &AtomicBool) -> Result<()> {
    ensure!(!stopping.load(Ordering::Relaxed), "audio startup cancelled");
    ensure!(worker.child.try_wait()?.is_none(), "audio pipeline process exited during startup");
    Ok(())
}

impl Drop for Worker {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() { let _ = stop.send(()); }
        self.child.stdin.take();
        let deadline = Instant::now() + Duration::from_millis(500);
        while matches!(self.child.try_wait(), Ok(None)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(writer) = self.writer.take() { let _ = writer.join(); }
        if let Some(reader) = self.reader.take() { let _ = reader.join(); }
    }
}

fn read_packet(input: &mut impl Read) -> Result<Vec<u8>> {
    let mut size = [0; 4];
    input.read_exact(&mut size)?;
    let size = u32::from_le_bytes(size) as usize;
    ensure!(size <= 65536, "invalid audio packet size");
    let mut data = vec![0; size];
    input.read_exact(&mut data)?;
    Ok(data)
}

fn write_packet(output: &mut impl Write, packet: &[u8]) -> Result<()> {
    output.write_all(&(packet.len() as u32).to_le_bytes())?;
    output.write_all(packet)?;
    Ok(())
}

/// Runs only in the private helper process. Standard output carries framed Opus, never logs.
pub fn worker() -> Result<()> {
    let socket = std::env::var_os("PIPEWIRE_REMOTE").context("private PipeWire socket is required")?;
    let (audio_tx, mut audio_rx) = tokio::sync::mpsc::channel(16);
    let (mic_tx, mic_rx) = tokio::sync::mpsc::channel(64);
    let output = bw_stream::audio_source(Path::new(&socket), OUTPUT, audio_tx)?;
    let microphone = bw_stream::audio_sink(Path::new(&socket), MICROPHONE_INPUT, mic_rx)?;
    let ended = Arc::new(AtomicBool::new(false));
    let input_ended = ended.clone();
    let mic_health = mic_tx.clone();
    std::thread::Builder::new().name("microphone-packets".into()).spawn(move || {
        let mut input = std::io::stdin().lock();
        while let Ok(data) = read_packet(&mut input) {
            if data.is_empty() || mic_tx.blocking_send(data.into()).is_err() { break; }
        }
        input_ended.store(true, Ordering::Relaxed);
    })?;
    let mut stdout = std::io::stdout().lock();
    let mut ready = false;
    tokio::runtime::Builder::new_current_thread().enable_time().build()?.block_on(async {
        let mut health = tokio::time::interval(Duration::from_millis(50));
        loop {
            tokio::select! {
                _ = health.tick() => {
                    if ended.load(Ordering::Relaxed) { return Ok(()); }
                    output.check()?;
                    microphone.check()?;
                    ensure!(!mic_health.is_closed(), "microphone pipeline ended");
                }
                packet = audio_rx.recv() => {
                    let Some(bw_core::StreamMsg::Audio { pts_us, data }) = packet else { bail!("audio capture ended"); };
                    if !ready { stdout.write_all(&[1])?; ready = true; }
                    stdout.write_all(&pts_us.to_le_bytes())?;
                    write_packet(&mut stdout, &data)?;
                    stdout.flush()?;
                }
            }
        }
    })
}
