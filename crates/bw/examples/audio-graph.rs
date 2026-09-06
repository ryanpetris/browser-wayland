//! Docker prototype for native graph events and per-node monitor peaks.
#[allow(dead_code)]
#[path = "../src/audio.rs"]
mod audio;

use anyhow::Context;
use pipewire::{
    self as pw,
    spa::{
        param::ParamType,
        pod::{Object, Pod, Value, deserialize::PodDeserializer, serialize::PodSerializer},
    },
};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    io::{Cursor, Read, Seek},
    os::unix::net::UnixStream,
    process::{Command, Stdio},
    rc::Rc,
    sync::{Arc, atomic::AtomicBool},
    time::{Duration, Instant},
};

struct Observed {
    _listener: pw::node::NodeListener,
    _node: pw::node::Node,
    meter: Option<audio::meter::Meter>,
    name: String,
}

struct Probe(std::process::Child);

impl Probe {
    fn wait(&mut self, timeout: Duration) -> anyhow::Result<std::process::ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.0.try_wait()? { return Ok(status); }
            anyhow::ensure!(Instant::now() < deadline, "probe timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

fn graph(env: &[(String, String)]) -> anyhow::Result<serde_json::Value> {
    let mut output = tempfile::tempfile()?;
    let mut child = Probe(Command::new("pw-dump").envs(env.iter().cloned()).stdout(output.try_clone()?).spawn()?);
    anyhow::ensure!(child.wait(Duration::from_secs(5))?.success(), "graph inspection failed");
    output.rewind()?;
    Ok(serde_json::from_reader(output)?)
}

fn check_links() -> anyhow::Result<()> {
    let graph = graph(&[])?;
    let objects = graph.as_array().context("graph array")?;
    let meters: Vec<_> = objects.iter().filter(|o| o["info"]["props"]["node.name"].as_str().is_some_and(|name| name.starts_with(audio::meter::NAME))).collect();
    anyhow::ensure!(meters.len() == 5, "expected five monitor nodes");
    for meter in meters {
        let serial = &meter["info"]["props"]["target.object"];
        let target = objects.iter().find(|o| &o["info"]["props"]["object.serial"] == serial).context("meter target missing")?;
        let links: Vec<_> = objects.iter().filter(|o| o["type"] == "PipeWire:Interface:Link" && o["info"]["input-node-id"] == meter["id"]).collect();
        anyhow::ensure!(!links.is_empty(), "meter has no input links");
        anyhow::ensure!(links.iter().all(|o| o["info"]["output-node-id"] == target["id"]), "meter linked to the wrong target");
    }
    Ok(())
}

impl Drop for Probe {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn main() -> anyhow::Result<()> {
    if std::env::args().nth(1).as_deref() != Some("--native") {
        let services = audio::Services::start(&Arc::new(AtomicBool::new(false)))?;
        let env = services.client_env();
        let _player = Probe(
            Command::new("gst-launch-1.0")
                .args([
                    "-q",
                    "audiotestsrc",
                    "is-live=true",
                    "freq=440",
                    "volume=0.1",
                    "!",
                    "audioconvert",
                    "!",
                    "audio/x-raw,rate=48000,channels=2",
                    "!",
                    "pipewiresink",
                    "sync=false",
                    "stream-properties=properties,node.name=probe-playback",
                ])
                .envs(env.clone())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?,
        );
        let _microphone = Probe(
            Command::new("gst-launch-1.0")
                .args([
                    "-q",
                    "audiotestsrc",
                    "is-live=true",
                    "freq=880",
                    "volume=0.05",
                    "!",
                    "audioconvert",
                    "!",
                    "audio/x-raw,rate=48000,channels=1",
                    "!",
                    "pipewiresink",
                    "sync=false",
                    "target-object=browser-wayland-microphone-input",
                    "stream-properties=properties,node.name=probe-microphone",
                ])
                .envs(env.clone())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?,
        );
        let mut recording = tempfile::NamedTempFile::new()?;
        let recorder = Probe(
            Command::new("pw-record")
                .args(["--raw", "--format=f32", "--rate=48000", "--channels=1", "-"])
                .envs(env.clone())
                .stdout(recording.as_file().try_clone()?)
                .stderr(Stdio::null())
                .spawn().context("starting probe recorder")?,
        );
        let mut child = Probe(
            Command::new(std::env::current_exe()?)
                .arg("--native").env("BW_PROBE_RECORDING", recording.path())
                .envs(env.clone())
                .spawn()?,
        );
        let result = child.wait(Duration::from_secs(30));
        anyhow::ensure!(result?.success(), "native probe failed");
        let graph = graph(&services.client_env())?;
        anyhow::ensure!(
            !graph
                .as_array()
                .unwrap()
                .iter()
                .any(|object| object["info"]["props"]["node.name"].as_str().is_some_and(|name| name.starts_with(audio::meter::NAME))),
            "monitor nodes survived their client"
        );
        drop(recorder);
        recording.rewind()?;
        let mut bytes = Vec::new();
        recording.read_to_end(&mut bytes)?;
        let samples: Vec<f32> = bytes.chunks_exact(4).map(|sample| f32::from_ne_bytes(sample.try_into().unwrap())).collect();
        anyhow::ensure!(samples.len() >= 48000, "recorder did not receive one second of audio");
        anyhow::ensure!(samples.iter().all(|sample| sample.is_finite()), "non-finite recording");
        anyhow::ensure!(samples.iter().any(|sample| sample.abs() > 0.04), "recorder never received microphone signal");
        anyhow::ensure!(samples[samples.len() - 12000..].iter().all(|sample| sample.abs() < 0.0001), "recording mute did not silence delivered samples");
        return Ok(());
    }
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let socket = UnixStream::connect(std::env::var("PIPEWIRE_REMOTE")?)?;
    let core = context.connect_fd_rc(socket.into(), None)?;
    let registry = core.get_registry_rc()?;
    let nodes: Rc<RefCell<HashMap<u32, Observed>>> = Rc::default();
    let monitor_ids: Rc<RefCell<HashSet<u32>>> = Rc::default();
    let added_monitors = monitor_ids.clone();
    let removed_monitors = monitor_ids.clone();
    let failures = Rc::new(RefCell::new(Vec::new()));
    let binding_errors = failures.clone();
    let all_nodes = nodes.clone();
    let bind_registry = registry.clone();
    let monitor_core = core.clone();
    let _registry_listener = registry.add_listener_local().global(move |global| {
        if global.type_ != pw::types::ObjectType::Node { return; }
        let Some(props) = global.props.as_ref() else { return; };
        let class = props.get("media.class").unwrap_or("");
        let name = props.get("node.name").unwrap_or("");
        if name.starts_with(audio::meter::NAME) { added_monitors.borrow_mut().insert(global.id); return; }
        if !matches!(class, "Audio/Sink" | "Audio/Source" | "Stream/Output/Audio" | "Stream/Input/Audio") || name.starts_with(audio::meter::NAME) || name == "browser-wayland-microphone-input" { return; }
        let Some(serial) = props.get("object.serial") else { binding_errors.borrow_mut().push("node has no serial".into()); return; };
        let id = global.id;
        println!("node {id} serial={serial} {class} {name}");
        let node: pw::node::Node = match bind_registry.bind(global) { Ok(node) => node, Err(error) => { binding_errors.borrow_mut().push(error.to_string()); return; } };
        let listener = node.add_listener_local().param(move |_, _, _, _, pod| {
            if let Some(pod) = pod {
                if let Ok((_, value)) = PodDeserializer::deserialize_any_from(pod.as_bytes()) { println!("params {id}: {value:?}"); }
            }
        }).register();
        node.subscribe_params(&[ParamType::Props]);
        let kind = match class {
            "Audio/Sink" => bw_core::audio::Kind::Output,
            "Audio/Source" => bw_core::audio::Kind::Input,
            "Stream/Output/Audio" => bw_core::audio::Kind::Playback,
            _ => bw_core::audio::Kind::Recording,
        };
        let meter = match audio::meter::Meter::new(monitor_core.clone(), serial, kind) { Ok(meter) => meter, Err(error) => { binding_errors.borrow_mut().push(error.to_string()); return; } };
        all_nodes.borrow_mut().insert(id, Observed { _node: node, _listener: listener, meter: Some(meter), name: name.into() });
    }).global_remove({ let nodes = nodes.clone(); move |id| { removed_monitors.borrow_mut().remove(&id); nodes.borrow_mut().remove(&id); } }).register();
    let _error = core
        .add_listener_local()
        .error(|id, _, code, message| eprintln!("core error {id}: {code} {message}"))
        .register();
    let ticks = std::cell::Cell::new(0);
    let checks = failures.clone();
    let stop = mainloop.clone();
    let startup = Instant::now();
    let timer = mainloop.loop_().add_timer(move |_| {
        if ticks.get() == 0 && !["probe-playback", "browser-wayland-output", "probe-microphone", "browser-wayland-microphone", "pw-record"].iter().all(|name| nodes.borrow().values().any(|node| node.name == *name && node.meter.as_ref().is_some_and(|meter| meter.peak() > 0.01))) {
            if startup.elapsed() > Duration::from_secs(8) { checks.borrow_mut().push("meters did not receive startup signals".into()); stop.quit(); }
            return;
        }
        ticks.set(ticks.get() + 1);
        if ticks.get() == 24 {
            for node in nodes.borrow_mut().values_mut() { node.meter.take(); }
        }
        if ticks.get() >= 24 {
            if ticks.get() == 27 {
                if !monitor_ids.borrow().is_empty() { checks.borrow_mut().push("monitor nodes survived dropping their meters".into()); }
                stop.quit();
            }
            return;
        }
        if ticks.get() == 2 {
            if let Err(error) = check_links() { checks.borrow_mut().push(error.to_string()); }
        }
        if ticks.get() == 21 {
            let result = (|| -> anyhow::Result<()> {
                let bytes = std::fs::read(std::env::var("BW_PROBE_RECORDING")?)?;
                anyhow::ensure!(bytes.len() >= 48000, "recording gain samples missing");
                let peak = bytes[bytes.len() - 48000..].chunks_exact(4).map(|s| f32::from_ne_bytes(s.try_into().unwrap()).abs()).fold(0.0f32, f32::max);
                anyhow::ensure!((peak - 0.00078125).abs() < 0.0001, "recording gain sample peak: {peak}");
                Ok(())
            })();
            if let Err(error) = result { checks.borrow_mut().push(error.to_string()); }
        }
        let nodes = nodes.borrow();
        if matches!(ticks.get(), 2 | 5 | 8 | 11 | 14 | 17 | 20) {
            let tick = ticks.get();
            for (name, expected) in [
                ("probe-playback", if tick == 5 { 0.0 } else if tick >= 8 { 0.0125 } else { 0.1 }),
                ("browser-wayland-output", if tick == 5 || tick >= 11 { 0.0 } else if tick >= 8 { 0.0125 } else { 0.1 }),
                ("probe-microphone", 0.05),
                ("browser-wayland-microphone", if tick == 14 { 0.0 } else if tick >= 17 { 0.00625 } else { 0.05 }),
                ("pw-record", if tick == 14 { 0.0 } else if tick >= 17 { 0.00625 } else { 0.05 }),
            ] {
                match nodes.values().find(|node| node.name == name) {
                    Some(node) if (node.meter.as_ref().unwrap().peak() - expected).abs() <= 0.001 => {}
                    Some(node) => checks.borrow_mut().push(format!("tick {tick} {name}: expected {expected}, got {}", node.meter.as_ref().unwrap().peak())),
                    None => checks.borrow_mut().push(format!("tick {tick}: missing {name}")),
                }
            }
        }
        for (id, node) in nodes.iter() {
            if let Some(error) = node.meter.as_ref().unwrap().error() { checks.borrow_mut().push(format!("{}: {error}", node.name)); }

            println!(
                "peak {} {id} {} {}",
                ticks.get(),
                node.name,
                node.meter.as_ref().unwrap().take_peak()
            );
            if (ticks.get() == 3 && node.name == "probe-playback")
                || (ticks.get() == 9 && node.name == "browser-wayland-output")
                || (ticks.get() == 12 && node.name == "browser-wayland-microphone")
                || (ticks.get() == 21 && node.name == "pw-record")
            {
                set_props(
                    &node._node,
                    vec![(pw::spa::sys::SPA_PROP_mute, Value::Bool(true))],
                );
            }
            if (ticks.get() == 6 && node.name == "probe-playback")
                || (ticks.get() == 15 && node.name == "browser-wayland-microphone")
                || (ticks.get() == 18 && node.name == "pw-record")
            {
                set_props(
                    &node._node,
                    vec![
                        (pw::spa::sys::SPA_PROP_mute, Value::Bool(false)),
                        (
                            pw::spa::sys::SPA_PROP_channelVolumes,
                            Value::ValueArray(pw::spa::pod::ValueArray::Float(vec![
                                0.125;
                                if node.name == "probe-playback" {
                                    2
                                } else {
                                    1
                                }
                            ])),
                        ),
                    ],
                );
            }
        }
    });
    timer
        .update_timer(
            Some(Duration::from_millis(300)),
            Some(Duration::from_millis(300)),
        )
        .into_result()?;
    mainloop.run();
    anyhow::ensure!(failures.borrow().is_empty(), "meter checks failed: {:?}", failures.borrow());
    Ok(())
}

fn set_props(node: &pw::node::Node, values: Vec<(u32, Value)>) {
    let pod = Value::Object(Object {
        type_: pw::spa::sys::SPA_TYPE_OBJECT_Props,
        id: ParamType::Props.as_raw(),
        properties: values
            .into_iter()
            .map(|(key, value)| pw::spa::pod::Property {
                key,
                flags: pw::spa::pod::PropertyFlags::empty(),
                value,
            })
            .collect(),
    });
    let bytes = PodSerializer::serialize(Cursor::new(Vec::new()), &pod)
        .unwrap()
        .0
        .into_inner();
    node.set_param(ParamType::Props, 0, Pod::from_bytes(&bytes).unwrap());
}
