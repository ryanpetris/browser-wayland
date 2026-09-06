//! Private audio services. Pipelines must stop before this owner is dropped.
#[path = "audio/meter.rs"]
pub mod meter;
#[path = "audio/mixer.rs"]
pub mod mixer;

use anyhow::{Context, Result, bail, ensure};
use std::{
    fs,
    io::{Read, Seek, Write},
    path::Path,
    process::{Child, Command, Stdio},
    sync::{Arc, atomic::{AtomicBool, Ordering}},
    time::{Duration, Instant},
};

const OUTPUT: &str = "elsewhere-output";
const MICROPHONE: &str = "elsewhere-microphone";
const MICROPHONE_INPUT: &str = "elsewhere-microphone-input";

pub struct Services {
    children: Vec<(&'static str, Child)>,
    env: Vec<(String, String)>,
    directory: tempfile::TempDir,
    stopping: Arc<AtomicBool>,
}

impl Services {
    pub fn start(stopping: &Arc<AtomicBool>) -> Result<Self> {
        let directory = tempfile::Builder::new().prefix("elsewhere-audio-").tempdir_in("/tmp")?;
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
        for (name, args) in [("pipewire", &[][..]), ("wireplumber", &["--profile=elsewhere"][..]), ("pipewire-pulse", &[][..])] {
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
    pub mic: tokio::sync::mpsc::Sender<elsewhere_core::Bytes>,
    pub mixer: Option<elsewhere_server::Mixer>,
}

struct Worker {
    child: Child,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    writer: Option<std::thread::JoinHandle<()>>,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl Session {
    pub fn start(stopping: &Arc<AtomicBool>, audio: tokio::sync::mpsc::Sender<elsewhere_core::StreamMsg>) -> Result<Self> {
        let mut services = Services::start(stopping)?;
        use std::os::fd::AsRawFd;
        let epoch_file = tempfile::tempfile_in(services.directory.path())?;
        epoch_file.set_len(8)?;
        let epoch_path = format!("/proc/{}/fd/{}", std::process::id(), epoch_file.as_raw_fd());
        // The anonymous file is fixed-size and shared only with the helper through Epoch.
        let epoch = Arc::new(unsafe { elsewhere_core::audio::Epoch::map(epoch_file)? });
        let child = services.command(std::env::current_exe()?.to_str().context("executable path")?)
            .arg("--audio-worker").env("ELSEWHERE_MIXER_EPOCH", epoch_path).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit()).spawn()?;
        let mut worker = Worker { child, stop: None, writer: None, reader: None };
        let mut stdout = worker.child.stdout.take().context("audio worker output")?;
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let (mixer_state, state) = tokio::sync::watch::channel(elsewhere_core::audio::Snapshot::default());
        let (mixer_levels, levels) = tokio::sync::watch::channel(Vec::new());
        let (mixer_errors, errors) = tokio::sync::mpsc::channel(32);
        let writer_errors = mixer_errors.clone();
        let (mixer_commands, mut requests) = tokio::sync::mpsc::channel::<elsewhere_core::audio::Request>(64);
        let (audience, mut authority) = tokio::sync::watch::channel(elsewhere_server::MixerAudience::default());
        worker.reader = Some(std::thread::Builder::new().name("audio-output".into()).spawn(move || {
            let mut ready = [0];
            if stdout.read_exact(&mut ready).is_err() || ready != [1] { return; }
            let _ = ready_tx.send(());
            let mut pending_errors = std::collections::HashMap::new();
            loop {
                let mut kind = [0];
                if stdout.read_exact(&mut kind).is_err() { break; }
                match kind[0] {
                    1 => {
                        let mut pts = [0; 8];
                        if stdout.read_exact(&mut pts).is_err() { break; }
                        let Ok(data) = read_packet(&mut stdout) else { break; };
                        if data.is_empty() { break; }
                        let _ = audio.try_send(elsewhere_core::StreamMsg::Audio { pts_us: u64::from_le_bytes(pts), data: data.into() });
                    }
                    2 => {
                        let Ok(data) = read_limited(&mut stdout, 4 * 1024 * 1024) else { break; };
                        let Ok(event) = serde_json::from_slice::<elsewhere_core::audio::Event>(&data) else { break; };
                        match event {
                            elsewhere_core::audio::Event::State(state) => { mixer_state.send_replace(state); }
                            elsewhere_core::audio::Event::Levels(levels) => { mixer_levels.send_replace(levels); }
                            elsewhere_core::audio::Event::Error { viewer, message } => {
                                if pending_errors.len() < 64 || pending_errors.contains_key(&viewer) { pending_errors.insert(viewer, message); }
                            }
                        }
                    }
                    _ => break,
                }
                pending_errors.retain(|viewer, message| mixer_errors.try_send((*viewer, message.clone())).is_err());
            }
            mixer_state.send_replace(elsewhere_core::audio::Snapshot { error: Some("Session mixer disconnected.".into()), ..Default::default() });
            mixer_levels.send_replace(Vec::new());
        })?);
        let mut stdin = worker.child.stdin.take().context("audio worker input")?;
        let (mic, mut packets) = tokio::sync::mpsc::channel::<elsewhere_core::Bytes>(64);
        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel();
        let runtime = tokio::runtime::Builder::new_current_thread().build()?;
        worker.stop = Some(stop_tx);
        worker.writer = Some(std::thread::Builder::new().name("microphone-input".into()).spawn(move || runtime.block_on(async {
            loop {
                let result = tokio::select! {
                    biased;
                    _ = &mut stop_rx => break,
                    changed = authority.changed() => {
                        if changed.is_err() { break; }
                        let audience = *authority.borrow();
                        write_request(&mut stdin, &elsewhere_core::audio::Request::Audience { subscribed: audience.subscribed, controller: audience.controller, epoch: audience.epoch })
                    }
                    Some(request) = requests.recv() => {
                        let audience = *authority.borrow();
                        if matches!(&request, elsewhere_core::audio::Request::Command { viewer, epoch, .. } if audience.controller != Some(*viewer) || audience.epoch != *epoch) {
                            if let elsewhere_core::audio::Request::Command { viewer, .. } = request { let _ = writer_errors.try_send((viewer, "Audio control permission changed.".into())); }
                            Ok(())
                        } else { write_request(&mut stdin, &request) }
                    }
                    Some(packet) = packets.recv() => write_tagged(&mut stdin, 1, &packet),
                    else => break,
                };
                if result.is_err() { break; }
                if let Ok(packet) = packets.try_recv() {
                    if write_tagged(&mut stdin, 1, &packet).is_err() { break; }
                }
            }
            let _ = stdin.write_all(&[0]);
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
        Ok(Self { worker, services, mic, mixer: Some(elsewhere_server::Mixer { commands: mixer_commands, audience, epoch, state, levels, errors: Some(errors) }) })
    }

    pub fn check(&mut self) -> Result<()> {
        self.services.check()?;
        ensure!(self.worker.child.try_wait()?.is_none(), "audio pipeline process exited");
        ensure!(!self.mic.is_closed(), "microphone transport ended");
        ensure!(!self.worker.reader.as_ref().is_some_and(|reader| reader.is_finished()), "audio output transport ended");
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

fn read_packet(input: &mut impl Read) -> Result<Vec<u8>> { read_limited(input, 65536) }

fn read_limited(input: &mut impl Read, limit: usize) -> Result<Vec<u8>> {
    let mut size = [0; 4];
    input.read_exact(&mut size)?;
    let size = u32::from_le_bytes(size) as usize;
    ensure!(size <= limit, "invalid audio packet size");
    let mut data = vec![0; size];
    input.read_exact(&mut data)?;
    Ok(data)
}

fn write_packet(output: &mut impl Write, packet: &[u8]) -> Result<()> {
    output.write_all(&(packet.len() as u32).to_le_bytes())?;
    output.write_all(packet)?;
    Ok(())
}

fn write_tagged(output: &mut impl Write, kind: u8, packet: &[u8]) -> Result<()> {
    output.write_all(&[kind])?;
    write_packet(output, packet)
}

fn write_request(output: &mut impl Write, request: &elsewhere_core::audio::Request) -> Result<()> {
    write_tagged(output, 2, &serde_json::to_vec(request)?)
}

fn write_mixer(output: &mut impl Write, event: &elsewhere_core::audio::Event) -> Result<()> {
    let mut packet = serde_json::to_vec(event)?;
    if packet.len() > 4 * 1024 * 1024 {
        packet = serde_json::to_vec(&elsewhere_core::audio::Event::State(elsewhere_core::audio::Snapshot {
            error: Some("Session mixer state exceeds its size limit.".into()), ..Default::default()
        }))?;
    }
    write_tagged(output, 2, &packet)
}

struct MixerThread {
    stopped: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for MixerThread {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() { let _ = thread.join(); }
    }
}

/// Runs only in the private helper process. Standard output carries framed Opus and mixer events.
pub fn worker() -> Result<()> {
    let socket = std::env::var_os("PIPEWIRE_REMOTE").context("private PipeWire socket is required")?;
    let (audio_tx, mut audio_rx) = tokio::sync::mpsc::channel(16);
    let (mic_tx, mic_rx) = tokio::sync::mpsc::channel(64);
    let output = elsewhere_stream::audio_source(Path::new(&socket), OUTPUT, audio_tx)?;
    let microphone = elsewhere_stream::audio_sink(Path::new(&socket), MICROPHONE_INPUT, mic_rx)?;
    let ended = Arc::new(AtomicBool::new(false));
    let input_ended = ended.clone();
    let mic_health = mic_tx.clone();
    let (control, mut requests) = mixer::channel();
    let epoch_path = std::env::var_os("ELSEWHERE_MIXER_EPOCH").context("private mixer epoch is required")?;
    let epoch_file = fs::OpenOptions::new().read(true).write(true).open(epoch_path)?;
    // The parent owns the fixed-size file; both processes access it only through Epoch.
    requests.epoch = Some(Arc::new(unsafe { elsewhere_core::audio::Epoch::map(epoch_file)? }));
    let (mixer_events, mut mixer_rx) = tokio::sync::mpsc::channel(32);
    let input_events = mixer_events.clone();
    let mixer_stop = ended.clone();
    let thread = std::thread::Builder::new().name("session-mixer".into()).spawn(move || mixer::run(socket.into(), requests, mixer_events, mixer_stop))?;
    let mixer_thread = MixerThread { stopped: ended.clone(), thread: Some(thread) };
    std::thread::Builder::new().name("audio-input".into()).spawn(move || {
        let mut input = std::io::stdin().lock();
        let result = (|| -> Result<()> {
            loop {
                let mut kind = [0];
                input.read_exact(&mut kind)?;
                match kind[0] {
                    0 => return Ok(()),
                    1 => {
                        let data = read_packet(&mut input)?;
                        if let Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) = mic_tx.try_send(data.into()) { return Ok(()); }
                    }
                    2 => {
                        let request = serde_json::from_slice::<elsewhere_core::audio::Request>(&read_limited(&mut input, 4096)?)?;
                        if let Err(error) = control.send(request) {
                            let request = match error { std::sync::mpsc::TrySendError::Full(request) | std::sync::mpsc::TrySendError::Disconnected(request) => request };
                            if let elsewhere_core::audio::Request::Command { viewer, .. } = request {
                                let _ = input_events.try_send(elsewhere_core::audio::Event::Error { viewer, message: "Session mixer is busy or disconnected. Try again.".into() });
                            }
                        }
                    }
                    _ => bail!("invalid audio input frame"),
                }
            }
        })();
        if let Err(error) = result { tracing::debug!("audio input ended: {error}"); }
        input_ended.store(true, Ordering::Relaxed);
    })?;
    let mut stdout = std::io::stdout().lock();
    let mut ready = false;
    let mut mixer_failed = false;
    tokio::runtime::Builder::new_current_thread().enable_time().build()?.block_on(async {
        let mut health = tokio::time::interval(Duration::from_millis(50));
        loop {
            tokio::select! {
                _ = health.tick() => {
                    if ended.load(Ordering::Relaxed) { return Ok(()); }
                    output.check()?;
                    microphone.check()?;
                    ensure!(!mic_health.is_closed(), "microphone pipeline ended");
                    if ready && !mixer_failed && mixer_thread.thread.as_ref().is_some_and(|thread| thread.is_finished()) {
                        mixer_failed = true;
                        write_mixer(&mut stdout, &elsewhere_core::audio::Event::State(elsewhere_core::audio::Snapshot { error: Some("Session mixer stopped.".into()), ..Default::default() }))?;
                        stdout.flush()?;
                    }
                }
                packet = audio_rx.recv() => {
                    let Some(elsewhere_core::StreamMsg::Audio { pts_us, data }) = packet else { bail!("audio capture ended"); };
                    if !ready { stdout.write_all(&[1])?; ready = true; }
                    stdout.write_all(&[1])?;
                    stdout.write_all(&pts_us.to_le_bytes())?;
                    write_packet(&mut stdout, &data)?;
                    stdout.flush()?;
                }
                Some(event) = mixer_rx.recv(), if ready => {
                    write_mixer(&mut stdout, &event)?;
                    stdout.flush()?;
                }
            }
        }
    })
}
