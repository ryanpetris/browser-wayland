//! Native mixer integration check; run inside the Docker audio rig.
#[allow(dead_code)]
#[path = "../src/audio.rs"]
mod audio;

use anyhow::{Context, Result, ensure};
use bw_core::audio::{Command as MixerCommand, Event, Kind, Request, Snapshot};
use std::{collections::HashMap, process::{Child, Command, Stdio}, sync::{Arc, atomic::{AtomicBool, Ordering}}, time::{Duration, Instant}};

struct Probe(Child);
impl Drop for Probe { fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); } }

struct Rig {
    requests: audio::mixer::Control,
    epoch: Arc<bw_core::audio::Epoch>,
    events: tokio::sync::mpsc::Receiver<Event>,
    state: Snapshot,
    levels: HashMap<String, f32>,
    errors: Vec<(u64, String)>,
    stopped: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() { let _ = thread.join(); }
    }
}

impl Rig {
    fn wait(&mut self, label: &str, predicate: impl Fn(&Self) -> bool) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            while let Ok(event) = self.events.try_recv() {
                match event {
                    Event::State(state) => { eprintln!("state {}: {:?}", state.generation, state.nodes.iter().map(|n| (&n.name, n.volume, n.mute, &n.targets)).collect::<Vec<_>>()); self.state = state; }
                    Event::Levels(levels) => self.levels = levels.into_iter().map(|level| (level.id, level.peak)).collect(),
                    Event::Error { viewer, message } => { eprintln!("error {viewer}: {message}"); self.errors.push((viewer, message)); }
                }
            }
            if predicate(self) { eprintln!("passed: {label}"); return Ok(()); }
            ensure!(Instant::now() < deadline, "timed out: {label}; state={:?}; levels={:?}", self.state, self.levels);
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn send(&self, viewer: u64, epoch: u64, command: MixerCommand) -> Result<()> {
        self.requests.send(Request::Command { viewer, epoch, command })?; Ok(())
    }
    fn audience(&self, subscribed: bool, controller: u64, epoch: u64) -> Result<()> {
        self.epoch.publish(epoch);
        self.requests.send(Request::Audience { subscribed, controller: Some(controller), epoch })?; Ok(())
    }
    fn id(&self, name: &str) -> Result<String> {
        self.state.nodes.iter().find(|n| n.name == name).map(|n| n.id.clone()).with_context(|| format!("missing {name}"))
    }
    fn peak(&self, id: &str, expected: f32) -> bool { self.levels.get(id).is_some_and(|p| (*p - expected).abs() < 0.002) }
}

fn graph() -> Result<serde_json::Value> {
    let output = Command::new("pw-dump").output()?;
    ensure!(output.status.success(), "pw-dump failed");
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn main() -> Result<()> {
    if std::env::args().nth(1).as_deref() == Some("--publish-epoch") {
        let file = std::fs::OpenOptions::new().read(true).write(true).open(std::env::args().nth(2).context("epoch file")?)?;
        // The probe parent owns a fixed-size file accessed only through Epoch.
        unsafe { bw_core::audio::Epoch::map(file)? }.publish(3);
        return Ok(());
    }
    if std::env::args().nth(1).as_deref() != Some("--native") {
        let services = audio::Services::start(&Arc::new(AtomicBool::new(false)))?;
        let mut probes = Vec::new();
        for (frequency, volume, sink, properties) in [
            ("440", "0.1", "pipewiresink", "stream-properties=properties,node.name=NativeTest,node.description=NativeTest,media.name=NativeTest,application.name=MixerTest"),
            ("880", "0.2", "pulsesink", "stream-properties=properties,media.name=PulseTest,application.name=MixerTest"),
        ] {
            probes.push(Probe(Command::new("gst-launch-1.0").args(["-q", "audiotestsrc", "is-live=true", &format!("freq={frequency}"), &format!("volume={volume}"), "!", "audioconvert", "!", "audio/x-raw,format=S16LE,rate=48000,channels=2", "!", sink, "sync=false", properties])
                .envs(services.client_env()).stdout(Stdio::null()).stderr(Stdio::inherit()).spawn()?));
        }
        probes.push(Probe(Command::new("pw-record").args(["--raw", "--format=f32", "--rate=48000", "--channels=1", "-"])
            .envs(services.client_env()).stdout(Stdio::null()).stderr(Stdio::inherit()).spawn()?));
        let mut child = Probe(Command::new(std::env::current_exe()?).arg("--native").envs(services.client_env()).spawn()?);
        let deadline = Instant::now() + Duration::from_secs(60);
        let status = loop { if let Some(status) = child.0.try_wait()? { break status; } ensure!(Instant::now() < deadline, "mixer check timed out"); std::thread::sleep(Duration::from_millis(10)); };
        ensure!(status.success(), "native mixer check failed");
        return Ok(());
    }
    let (requests, mut receiver) = audio::mixer::channel();
    use std::os::fd::AsRawFd;
    let epoch_file = tempfile::tempfile()?;
    epoch_file.set_len(8)?;
    let epoch_path = format!("/proc/{}/fd/{}", std::process::id(), epoch_file.as_raw_fd());
    // Both descriptors refer to the fixed-size file; only Epoch accesses its bytes.
    let epoch = Arc::new(unsafe { bw_core::audio::Epoch::map(epoch_file)? });
    let backend_file = std::fs::OpenOptions::new().read(true).write(true).open(&epoch_path)?;
    receiver.epoch = Some(Arc::new(unsafe { bw_core::audio::Epoch::map(backend_file)? }));
    let (events_tx, events) = tokio::sync::mpsc::channel(32);
    let stopped = Arc::new(AtomicBool::new(false));
    let stop = stopped.clone();
    let remote = std::env::var_os("PIPEWIRE_REMOTE").context("private socket")?.into();
    let thread = std::thread::spawn(move || audio::mixer::run(remote, receiver, events_tx, stop));
    let mut rig = Rig { requests, epoch, events, state: Snapshot::default(), levels: HashMap::new(), errors: Vec::new(), stopped, thread: Some(thread) };
    rig.wait("native and Pulse streams", |r| r.state.nodes.iter().any(|n| n.name == "NativeTest" && n.volume_writable && n.mute_writable)
        && r.state.nodes.iter().any(|n| n.kind == Kind::Playback && n.name != "NativeTest" && n.volume_writable && n.mute_writable))?;
    ensure!(rig.state.nodes.len() == 5, "unexpected mixer rows: {:?}", rig.state.nodes);
    let native = rig.id("NativeTest")?;
    let pulse = rig.state.nodes.iter().find(|n| n.kind == Kind::Playback && n.id != native).context("Pulse stream")?.id.clone();
    ensure!(graph()?.as_array().context("graph array")?.iter().all(|o| !o["info"]["props"]["node.name"].as_str().is_some_and(|n| n.starts_with(audio::meter::NAME))), "meters exist without subscribers");
    rig.audience(true, 1, 1)?;
    rig.wait("distinct per-stream peaks", |r| r.peak(&native, 0.1) && r.peak(&pulse, 0.2))?;
    rig.send(1, 1, MixerCommand::Volume { id: native.clone(), value: 50.0 })?;
    rig.wait("authoritative volume and stream isolation", |r| r.state.nodes.iter().any(|n| n.id == native && n.volume.is_some_and(|v| (v - 50.0).abs() < 0.01)) && r.peak(&native, 0.0125) && r.peak(&pulse, 0.2))?;
    rig.send(9, 1, MixerCommand::Mute { id: native.clone(), value: true })?;
    rig.wait("non-controller rejected", |r| r.errors.iter().any(|(viewer, _)| *viewer == 9))?;
    ensure!(rig.state.nodes.iter().any(|n| n.id == native && n.mute == Some(false)), "unauthorized mute applied");
    rig.audience(true, 2, 2)?;
    rig.send(1, 1, MixerCommand::Mute { id: native.clone(), value: true })?;
    rig.wait("old controller epoch rejected", |r| r.errors.iter().any(|(viewer, _)| *viewer == 1))?;
    rig.send(2, 2, MixerCommand::Mute { id: native.clone(), value: true })?;
    rig.wait("new controller can mute", |r| r.state.nodes.iter().any(|n| n.id == native && n.mute == Some(true)) && r.peak(&native, 0.0) && r.peak(&pulse, 0.2))?;
    rig.send(2, 2, MixerCommand::Volume { id: native.clone(), value: f32::NAN })?;
    rig.wait("non-finite control rejected", |r| r.errors.iter().any(|(viewer, message)| *viewer == 2 && message.contains("between")))?;
    let output = rig.state.nodes.iter().find(|n| n.kind == Kind::Output).context("output")?.id.clone();
    ensure!(Command::new("pw-cli").args(["create-node", "adapter", "{ factory.name = support.null-audio-sink node.name = OtherOutput node.description = OtherOutput media.class = Audio/Sink node.virtual = true node.always-process = true monitor.channel-volumes = true audio.position = [ FL FR ] object.linger = true }"]).stdout(Stdio::null()).status()?.success(), "second endpoint creation failed");
    rig.wait("second session output", |r| r.state.nodes.iter().any(|n| n.name == "OtherOutput" && n.kind == Kind::Output))?;
    let other = rig.id("OtherOutput")?;
    rig.send(2, 2, MixerCommand::Mute { id: native.clone(), value: false })?;
    rig.send(2, 2, MixerCommand::Target { id: native.clone(), target: Some(other.clone()) })?;
    rig.wait("WirePlumber moves stream and signal", |r| r.state.nodes.iter().any(|n| n.id == native && n.targets == [other.clone()])
        && r.state.nodes.iter().any(|n| n.id == pulse && n.targets == [output.clone()])
        && r.peak(&other, 0.0125) && r.peak(&output, 0.2))?;
    rig.send(2, 2, MixerCommand::Target { id: native.clone(), target: None })?;
    rig.wait("cleared target follows the session default", |r| r.state.nodes.iter().any(|n| n.id == native && n.targets == [output.clone()]) && r.peak(&other, 0.0))?;
    rig.send(2, 2, MixerCommand::Target { id: native.clone(), target: Some(other.clone()) })?;
    rig.wait("explicit target restored", |r| r.state.nodes.iter().any(|n| n.id == native && n.targets == [other.clone()]) && r.peak(&other, 0.0125))?;
    rig.send(2, 2, MixerCommand::Default { id: other.clone() })?;
    rig.wait("WirePlumber changes the session default", |r| r.state.nodes.iter().any(|n| n.id == other && n.is_default) && r.state.nodes.iter().any(|n| n.id == output && !n.is_default))?;
    let next_player = || -> Result<Probe> { Ok(Probe(Command::new("gst-launch-1.0").args(["-q", "audiotestsrc", "is-live=true", "freq=1320", "volume=0.15", "!", "audioconvert", "!", "audio/x-raw,rate=48000,channels=2", "!", "pipewiresink", "sync=false", "stream-properties=properties,node.name=NextTest,node.description=NextTest,media.name=NextTest"]).stdout(Stdio::null()).spawn()?)) };
    let next = next_player()?;
    rig.wait("new application uses changed default", |r| r.state.nodes.iter().any(|n| n.name == "NextTest" && n.targets == [other.clone()]))?;
    let removed = rig.id("NextTest")?;
    let global_id = || -> Result<u64> {
        graph()?.as_array().context("graph array")?.iter().find(|o| o["info"]["props"]["node.name"] == "NextTest").context("NextTest global")?["id"].as_u64().context("global id")
    };
    let mut used_ids = HashMap::from([(global_id()?, removed.clone())]);
    drop(next);
    rig.wait("application removal is live", |r| !r.state.nodes.iter().any(|n| n.id == removed))?;
    rig.errors.clear();
    rig.send(2, 2, MixerCommand::Mute { id: removed, value: false })?;
    rig.wait("removed application rejects controls", |r| r.errors.iter().any(|(_, message)| message.contains("earlier connection")))?;
    let mut reused = false;
    for _ in 0..12 {
        let next = next_player()?;
        rig.wait("application restart is live", |r| r.state.nodes.iter().any(|n| n.name == "NextTest" && n.mute == Some(false)))?;
        let current = rig.id("NextTest")?;
        rig.wait("restarted application has its own meter", |r| r.peak(&current, 0.15))?;
        let global = global_id()?;
        if let Some(stale) = used_ids.insert(global, current.clone()) {
            ensure!(stale != current, "reused global retained an old object identifier");
            rig.errors.clear();
            rig.send(2, 2, MixerCommand::Mute { id: stale, value: true })?;
            rig.wait("reused numeric ID rejects previous object controls", |r| r.errors.iter().any(|(_, message)| message.contains("earlier connection")))?;
            ensure!(rig.state.nodes.iter().any(|n| n.id == current && n.mute == Some(false)) && rig.peak(&current, 0.15), "stale command affected replacement stream");
            reused = true;
        }
        drop(next);
        rig.wait("restarted application exits cleanly", |r| !r.state.nodes.iter().any(|n| n.id == current))?;
        if reused { break; }
    }
    ensure!(reused, "numeric global ID reuse was not exercised");
    let old_generation = rig.state.generation.clone();
    let graph = graph()?;
    let client = graph.as_array().context("graph array")?.iter().find(|o| o["type"] == "PipeWire:Interface:Client" && o["info"]["props"]["application.id"] == "browser-wayland-mixer").context("management client")?["id"].as_u64().context("client id")?;
    ensure!(Command::new("pw-cli").args(["destroy", &client.to_string()]).status()?.success(), "disconnect failed");
    rig.wait("new generation after reconnect", |r| r.state.available && r.state.generation != old_generation && r.state.nodes.len() == 6)?;
    rig.errors.clear();
    rig.send(2, 2, MixerCommand::Mute { id: native, value: false })?;
    rig.wait("stale object rejected", |r| r.errors.iter().any(|(_, message)| message.contains("earlier connection")))?;
    let native = rig.id("NativeTest")?;
    rig.send(2, 2, MixerCommand::Mute { id: native.clone(), value: false })?;
    rig.wait("controller and subscriptions survive reconnect", |r| r.peak(&native, 0.0125))?;
    let meter_graph = self::graph()?;
    let serial = native.rsplit(':').next().context("native serial")?;
    let meter = meter_graph.as_array().context("graph array")?.iter().find(|o| o["info"]["props"]["node.name"] == audio::meter::NAME && (o["info"]["props"]["target.object"].as_str() == Some(serial) || o["info"]["props"]["target.object"].as_u64() == serial.parse().ok())).context("native monitor")?["id"].as_u64().context("monitor id")?;
    let before_fault = rig.state.generation.clone();
    ensure!(Command::new("pw-cli").args(["destroy", &meter.to_string()]).status()?.success(), "meter fault injection failed");
    // PipeWire disconnects the owning management client when its exported monitor is destroyed.
    rig.wait("monitor removal reconnects the graph", |r| r.state.available && r.state.generation != before_fault && r.state.nodes.iter().any(|n| n.name == "NativeTest"))?;
    let native = rig.id("NativeTest")?;
    rig.levels.clear();
    rig.wait("monitor removal recovers real signal", |r| r.peak(&native, 0.0125))?;
    rig.errors.clear();
    ensure!(Command::new(std::env::current_exe()?).args(["--publish-epoch", &epoch_path]).status()?.success(), "cross-process epoch publication failed");
    // The local Audience still grants viewer 2 epoch 2: only shared authority revokes it.
    rig.send(2, 2, MixerCommand::Mute { id: native.clone(), value: true })?;
    rig.wait("shared epoch revokes commands before local authority arrives", |r| r.errors.iter().any(|(_, message)| message.contains("permission changed")))?;
    ensure!(rig.state.nodes.iter().any(|n| n.id == native && n.mute == Some(false)), "stale local authority applied mute");
    rig.audience(false, 2, 3)?;
    rig.wait("meter activity withdrawn", |r| r.state.nodes.iter().all(|n| !n.meter_active))?;
    std::thread::sleep(Duration::from_millis(300));
    ensure!(self::graph()?.as_array().context("graph array")?.iter().all(|o| !o["info"]["props"]["node.name"].as_str().is_some_and(|n| n.starts_with(audio::meter::NAME))), "meters survived unsubscribe");
    eprintln!("native mixer checks passed");
    Ok(())
}
