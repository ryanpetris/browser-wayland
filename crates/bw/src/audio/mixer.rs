//! Native management of one private graph. PipeWire objects stay on this thread.
use super::meter::{self, Meter};
use anyhow::{Context, Result, bail, ensure};
use bw_core::audio::{Command, Event, Kind, Level, Node, Request, Snapshot};
use pipewire::{self as pw, permissions::PermissionFlags as Access, proxy::ProxyT, spa::{
    param::{ParamInfoFlags, ParamType},
    pod::{Object, Pod, Property, PropertyFlags, Value, ValueArray, deserialize::PodDeserializer, serialize::PodSerializer},
    utils::dict::DictRef,
}};
use std::{cell::{Cell, RefCell}, collections::{HashMap, HashSet}, io::Cursor, os::unix::net::UnixStream,
    path::{Path, PathBuf}, rc::Rc, sync::{Arc, Mutex, atomic::{AtomicBool, AtomicU64, Ordering}, mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel}},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH}};


type Audience = Arc<Mutex<(bool, Option<u64>, u64)>>;

#[derive(Clone)]
pub struct Control {
    requests: SyncSender<Request>,
    audience: Audience,
}

pub struct Requests {
    pub epoch: Option<Arc<bw_core::audio::Epoch>>,
    queue: Receiver<Request>,
    audience: Audience,
}

pub fn channel() -> (Control, Requests) {
    let (requests, queue) = sync_channel(64);
    let audience = Arc::new(Mutex::new((false, None, 0)));
    (Control { requests, audience: audience.clone() }, Requests { queue, audience, epoch: None })
}

impl Control {
    /// Authority bypasses the command backlog and shares the dispatch lock.
    pub fn send(&self, request: Request) -> std::result::Result<(), TrySendError<Request>> {
        if let Request::Audience { subscribed, controller, epoch } = request {
            *self.audience.lock().unwrap_or_else(|p| p.into_inner()) = (subscribed, controller, epoch);
            Ok(())
        } else { self.requests.try_send(request) }
    }
}

#[derive(Default)]
struct Props {
    master: Option<f32>,
    channels: Option<Vec<f32>>,
    mute: Option<bool>,
    volume_write: bool,
    master_write: bool,
    channels_write: bool,
    mute_write: bool,
}

impl Props {
    fn read(&mut self, pod: &Pod) -> Option<()> {
        let (_, Value::Object(object)) = PodDeserializer::deserialize_any_from(pod.as_bytes()).ok()? else { return None; };
        if object.type_ != pw::spa::sys::SPA_TYPE_OBJECT_Props { return None; }
        let props = self;
        for property in object.properties {
            let writable = !property.flags.contains(PropertyFlags::READONLY);
            match (property.key, property.value) {
                (pw::spa::sys::SPA_PROP_volume, Value::Float(value)) if value.is_finite() && value >= 0.0 => {
                    props.master = Some(value); props.master_write = writable;
                }
                (pw::spa::sys::SPA_PROP_channelVolumes, Value::ValueArray(ValueArray::Float(values)))
                    if !values.is_empty() && values.len() <= 64 && values.iter().all(|v| v.is_finite() && *v >= 0.0) => {
                    props.channels = Some(values); props.channels_write = writable;
                }
                (pw::spa::sys::SPA_PROP_mute, Value::Bool(value)) => { props.mute = Some(value); props.mute_write = writable; }
                _ => {}
            }
        }
        props.volume_write = if props.channels.is_some() { props.channels_write && (props.master.is_none() || props.master_write) } else { props.master_write };
        Some(())
    }

    fn percent(&self) -> Option<f32> {
        let gain = self.channels.as_ref().map(|values| values.iter().copied().fold(0.0, f32::max) * self.master.unwrap_or(1.0)).or(self.master)?;
        Some((gain.cbrt() * 100.0).clamp(0.0, 100.0))
    }

    fn volume(&self, percent: f32) -> Vec<(u32, Value)> {
        let gain = (percent / 100.0).powi(3);
        if let Some(channels) = &self.channels {
            let maximum = channels.iter().copied().fold(0.0, f32::max);
            let values = channels.iter().map(|v| if maximum > 0.0 { v / maximum * gain } else { gain }).collect();
            let mut properties = vec![(pw::spa::sys::SPA_PROP_channelVolumes, Value::ValueArray(ValueArray::Float(values)))];
            if self.master.is_some() { properties.push((pw::spa::sys::SPA_PROP_volume, Value::Float(1.0))); }
            properties
        } else { vec![(pw::spa::sys::SPA_PROP_volume, Value::Float(gain))] }
    }
}

struct Record {
    serial: String,
    properties: HashMap<String, String>,
    state: String,
    access: Access,
    props_write: bool,
    props: Props,
    meter_active: bool,
    meter_error: Option<String>,
}

impl Record {
    fn kind(&self) -> Option<Kind> {
        let name = self.properties.get("node.name").map(String::as_str).unwrap_or("");
        if name.starts_with(meter::NAME) || matches!(name, "browser-wayland-microphone-input" | "browser-wayland-capture" | "browser-wayland-microphone-stream") { return None; }
        match self.properties.get("media.class")?.as_str() {
            "Audio/Sink" => Some(Kind::Output), "Audio/Source" => Some(Kind::Input),
            "Stream/Output/Audio" => Some(Kind::Playback), "Stream/Input/Audio" => Some(Kind::Recording), _ => None,
        }
    }
    fn id(&self, generation: &str) -> String { format!("{generation}:{}", self.serial) }
    fn writable(&self) -> bool { self.props_write && self.access.contains(Access::W | Access::X) }
}

#[derive(Default)]
struct State {
    nodes: HashMap<u32, Record>,
    clients: HashMap<u32, HashMap<String, String>>,
    links: HashMap<u32, (u32, u32)>,
    defaults: HashMap<Kind, String>,
    errors: HashMap<u64, String>,
    pending: HashMap<u32, u64>,
    controls: HashMap<String, (u64, Command, Instant)>,
    disconnected: Option<String>,
    binding_errors: HashMap<u32, String>,
}

impl State {
    fn error(&mut self, viewer: u64, message: impl ToString) {
        if self.errors.len() < 64 || self.errors.contains_key(&viewer) { self.errors.insert(viewer, message.to_string()); }
    }

    fn snapshot(&self, generation: &str, routing: bool) -> Snapshot {
        let mut nodes: Vec<_> = self.nodes.iter().filter_map(|(id, record)| {
            let kind = record.kind()?;
            let properties = &record.properties;
            let client = properties.get("client.id").and_then(|id| id.parse().ok()).and_then(|id| self.clients.get(&id));
            let application = ["application.name", "application.process.binary", "application.id"].iter()
                .find_map(|key| properties.get(*key).and_then(|s| display_label(s)).or_else(|| client.and_then(|c| c.get(*key)).and_then(|s| display_label(s))));
            let names = if matches!(kind, Kind::Playback | Kind::Recording) { ["media.name", "node.description", "node.nick", "node.name"] } else { ["node.description", "media.name", "node.nick", "node.name"] };
            let name = names.iter().find_map(|key| properties.get(*key).and_then(|s| display_label(s)))
                .unwrap_or_else(|| format!("{kind:?} {}", record.serial));
            let mut targets: Vec<_> = self.links.values().filter_map(|(output, input)| {
                let target = match kind { Kind::Playback if output == id => input, Kind::Recording if input == id => output, _ => return None };
                let target = self.nodes.get(target)?;
                (target.kind() == kind.target_kind()).then(|| target.id(generation))
            }).collect();
            targets.sort(); targets.dedup();
            Some(Node {
                id: record.id(generation), name, application, kind, state: record.state.clone(),
                volume: record.props.percent(), mute: record.props.mute,
                volume_writable: record.writable() && record.props.volume_write,
                mute_writable: record.writable() && record.props.mute_write,
                routing_writable: routing && record.access.contains(Access::M), targets,
                is_default: self.defaults.get(&kind).is_some_and(|name| properties.get("node.name") == Some(name)),
                meter_before_volume: kind == Kind::Recording, meter_active: record.meter_active, meter_error: record.meter_error.clone(),
            })
        }).collect();
        nodes.sort_by(|a, b| a.id.cmp(&b.id));
        Snapshot { generation: generation.into(), available: true, error: self.binding_errors.values().min().cloned(), routing, nodes }
    }
}

fn display_label(value: &str) -> Option<String> {
    let value = label(value);
    (!value.trim().is_empty()).then_some(value)
}

fn label(value: &str) -> String { value.chars().filter(|c| !c.is_control()).take(256).collect() }
fn properties(dict: &DictRef) -> HashMap<String, String> { dict.iter().map(|(k, v)| (k.into(), v.into())).collect() }

struct NodeBinding {
    _listener: pw::node::NodeListener,
    node: pw::node::Node,
    meter: Option<Meter>,
    meter_retry: Option<Instant>,
    meter_delay: u64,
}

enum Binding {
    Node(NodeBinding),
    Client { _listener: pw::client::ClientListener, _client: pw::client::Client },
    Link { _listener: pw::link::LinkListener, _link: pw::link::Link },
    Metadata { _listener: pw::metadata::MetadataListener, metadata: pw::metadata::Metadata, writable: bool },
}

fn set_props(node: &pw::node::Node, values: Vec<(u32, Value)>) -> Result<()> {
    let pod = Value::Object(Object { type_: pw::spa::sys::SPA_TYPE_OBJECT_Props, id: ParamType::Props.as_raw(),
        properties: values.into_iter().map(|(key, value)| Property { key, flags: PropertyFlags::empty(), value }).collect() });
    let bytes = PodSerializer::serialize(Cursor::new(Vec::new()), &pod)?.0.into_inner();
    node.set_param(ParamType::Props, 0, Pod::from_bytes(&bytes).context("audio control properties")?);
    Ok(())
}

fn apply(command: &Command, viewer: u64, generation: &str, state: &mut State, bindings: &HashMap<u32, Binding>) -> Result<()> {
    command.validate().map_err(anyhow::Error::msg)?;
    let id = match command {
        Command::Volume { id, .. } | Command::Mute { id, .. } | Command::Target { id, .. } | Command::Default { id } => id,
        Command::Subscribe { .. } => bail!("subscriptions are managed by the server"),
    };
    let (global, record) = state.nodes.iter().find(|(_, record)| record.kind().is_some() && record.id(generation) == *id).context("Audio object is gone or belongs to an earlier connection.")?;
    let Some(Binding::Node(binding)) = bindings.get(global) else { bail!("Audio object is unavailable."); };
    let metadata = bindings.values().find_map(|binding| match binding { Binding::Metadata { metadata, writable: true, .. } => Some(metadata), _ => None });
    match command {
        Command::Volume { value, .. } => {
            ensure!(record.writable() && record.props.volume_write, "Volume is not writable on this audio object.");
            set_props(&binding.node, record.props.volume(*value))?;
        }
        Command::Mute { value, .. } => {
            ensure!(record.writable() && record.props.mute_write, "Mute is not writable on this audio object.");
            set_props(&binding.node, vec![(pw::spa::sys::SPA_PROP_mute, Value::Bool(*value))])?;
        }
        Command::Target { target, .. } => {
            ensure!(record.access.contains(Access::M), "Routing is not writable on this audio object.");
            let kind = record.kind().and_then(Kind::target_kind).context("Only application streams can be moved.")?;
            let target = target.as_ref().map(|target| state.nodes.values().find(|record| record.kind() == Some(kind) && record.id(generation) == *target)
                .map(|record| record.serial.as_str()).context("The audio target is gone or incompatible.")).transpose()?;
            let metadata = metadata.context("WirePlumber routing is unavailable.")?;
            metadata.set_property(*global, "target.object", Some("Spa:Id"), target);
            state.pending.insert(metadata.upcast_ref().id(), viewer);
        }
        Command::Default { .. } => {
            ensure!(record.access.contains(Access::M), "Routing is not writable on this audio object.");
            let key = match record.kind() { Some(Kind::Output) => "default.configured.audio.sink", Some(Kind::Input) => "default.configured.audio.source", _ => bail!("Only session endpoints can be defaults.") };
            let name = record.properties.get("node.name").context("Audio endpoint has no routing name.")?;
            let metadata = metadata.context("WirePlumber defaults are unavailable.")?;
            metadata.set_property(0, key, Some("Spa:String:JSON"), Some(&serde_json::json!({"name": name}).to_string()));
            state.pending.insert(metadata.upcast_ref().id(), viewer);
        }
        Command::Subscribe { .. } => unreachable!(),
    }
    state.pending.insert(binding.node.upcast_ref().id(), viewer);
    let key = match command {
        Command::Default { .. } => format!("default:{:?}", record.kind()),
        Command::Target { .. } => format!("target:{id}"),
        Command::Volume { .. } => format!("volume:{id}"),
        Command::Mute { .. } => format!("mute:{id}"),
        Command::Subscribe { .. } => unreachable!(),
    };
    state.controls.insert(key, (viewer, command.clone(), Instant::now()));
    Ok(())
}

fn connect(remote: &Path, requests: Rc<Receiver<Request>>, audience: Audience, shared_epoch: Option<Arc<bw_core::audio::Epoch>>, events: tokio::sync::mpsc::Sender<Event>, stopping: Arc<AtomicBool>) -> Result<()> {
    ensure!(remote.is_absolute(), "private mixer socket must be absolute");
    static GENERATION: AtomicU64 = AtomicU64::new(0);
    let generation = format!("{}-{}", SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos(), GENERATION.fetch_add(1, Ordering::Relaxed));
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_fd_rc(UnixStream::connect(remote)?.into(), Some(pw::properties::properties! { "application.name" => "Browser Wayland mixer", "application.id" => "browser-wayland-mixer" }))?;
    let registry = core.get_registry_rc()?;
    let state: Rc<RefCell<State>> = Rc::default();
    let bindings: Rc<RefCell<HashMap<u32, Binding>>> = Rc::default();
    let registry_ref = registry.clone();
    let objects = bindings.clone();
    let added = state.clone();
    let removed = state.clone();
    let removed_objects = bindings.clone();
    let _registry_listener = registry.add_listener_local().global(move |global| {
        let id = global.id;
        let result = (|| -> Result<Option<Binding>> {
            Ok(Some(match global.type_ {
                pw::types::ObjectType::Node => {
                    let initial = global.props.as_ref().map(|p| properties(p)).unwrap_or_default();
                    let serial = initial.get("object.serial").context("audio node has no serial")?.clone();
                    ensure!(serial.parse::<u64>().is_ok(), "audio node serial is invalid");
                    let node: pw::node::Node = registry_ref.bind(global)?;
                    added.borrow_mut().nodes.insert(id, Record { serial, properties: initial, state: "creating".into(), access: global.permissions,
                        props_write: false, props: Props::default(), meter_active: false, meter_error: None });
                    let info_state = added.clone();
                    let param_state = added.clone();
                    let listener = node.add_listener_local().info(move |info| {
                        if let Some(record) = info_state.borrow_mut().nodes.get_mut(&id) {
                            if let Some(props) = info.props() { record.properties.extend(properties(props)); }
                            record.state = match info.state() {
                                pw::node::NodeState::Error(error) => format!("error: {}", label(error)),
                                pw::node::NodeState::Creating => "creating".into(), pw::node::NodeState::Suspended => "suspended".into(),
                                pw::node::NodeState::Idle => "idle".into(), pw::node::NodeState::Running => "running".into(),
                            };
                            if info.change_mask().contains(pw::node::NodeChangeMask::PARAMS) { record.props_write = info.params().iter().any(|p| p.id() == ParamType::Props && p.flags().contains(ParamInfoFlags::WRITE)); }
                        }
                    }).param(move |_, kind, _, _, pod| {
                        if kind != ParamType::Props { return; }
                        if let Some(record) = param_state.borrow_mut().nodes.get_mut(&id) {
                            if let Some(pod) = pod { record.props.read(pod); }
                        }
                    }).register();
                    node.subscribe_params(&[ParamType::Props]);
                    Binding::Node(NodeBinding { _listener: listener, node, meter: None, meter_retry: None, meter_delay: 1 })
                }
                pw::types::ObjectType::Client => {
                    let client: pw::client::Client = registry_ref.bind(global)?;
                    let state = added.clone();
                    let listener = client.add_listener_local().info(move |info| {
                        if let Some(props) = info.props() { state.borrow_mut().clients.entry(id).or_default().extend(properties(props)); }
                    }).register();
                    Binding::Client { _listener: listener, _client: client }
                }
                pw::types::ObjectType::Link => {
                    let link: pw::link::Link = registry_ref.bind(global)?;
                    let state = added.clone();
                    let listener = link.add_listener_local().info(move |info| {
                        let mut state = state.borrow_mut();
                        if matches!(info.state(), pw::link::LinkState::Active | pw::link::LinkState::Paused) {
                            state.links.insert(id, (info.output_node_id(), info.input_node_id()));
                        } else { state.links.remove(&id); }
                    }).register();
                    Binding::Link { _listener: listener, _link: link }
                }
                pw::types::ObjectType::Metadata if global.props.as_ref().is_some_and(|p| p.get("metadata.name") == Some("default")) => {
                    let metadata: pw::metadata::Metadata = registry_ref.bind(global)?;
                    let state = added.clone();
                    let listener = metadata.add_listener_local().property(move |subject, key, _, value| {
                        if subject != 0 { return 0; }
                        let kind = match key { Some("default.audio.sink") => Kind::Output, Some("default.audio.source") => Kind::Input,
                            None => { state.borrow_mut().defaults.clear(); return 0; }, _ => return 0 };
                        let name = value.and_then(|v| serde_json::from_str::<serde_json::Value>(v).ok()).and_then(|v| v["name"].as_str().map(String::from));
                        if let Some(name) = name { state.borrow_mut().defaults.insert(kind, name); } else { state.borrow_mut().defaults.remove(&kind); }
                        0
                    }).register();
                    Binding::Metadata { _listener: listener, metadata, writable: global.permissions.contains(Access::W | Access::X) }
                }
                _ => return Ok(None),
            }))
        })();
        match result { Ok(Some(binding)) => { objects.borrow_mut().insert(id, binding); }, Ok(None) => {},
            Err(error) => { added.borrow_mut().binding_errors.insert(id, format!("Audio object {id} unavailable: {}", label(&error.to_string()))); } }
    }).global_remove(move |id| {
        let binding = removed_objects.borrow_mut().remove(&id);
        let mut state = removed.borrow_mut();
        match binding.as_ref() {
            Some(Binding::Node(binding)) => { state.pending.remove(&binding.node.upcast_ref().id()); }
            Some(Binding::Metadata { metadata, .. }) => { state.pending.remove(&metadata.upcast_ref().id()); state.defaults.clear(); }
            _ => {}
        }
        state.binding_errors.remove(&id);
        state.nodes.remove(&id); state.clients.remove(&id); state.links.remove(&id);
        state.links.retain(|_, (output, input)| *output != id && *input != id);
    }).register();
    let errors = state.clone();
    let ready = Rc::new(Cell::new(false));
    let completed = ready.clone();
    let pending = Cell::new(core.sync(0)?);
    let discovery = Cell::new(true);
    let sync_core = core.clone();
    let sync_errors = state.clone();
    let command_sync = Rc::new(Cell::new(None));
    let completed_commands = command_sync.clone();
    let command_state = state.clone();
    let _core_listener = core.add_listener_local().done(move |id, seq| {
        if id != 0 { return; }
        if completed_commands.get() == Some(seq) {
            command_state.borrow_mut().pending.clear();
            completed_commands.set(None);
        }
        if seq != pending.get() { return; }
        if discovery.replace(false) {
            match sync_core.sync(0) { Ok(seq) => pending.set(seq), Err(error) => sync_errors.borrow_mut().disconnected = Some(error.to_string()) }
        } else { completed.set(true); }
    }).error(move |id, _, _, message| {
        let mut state = errors.borrow_mut();
        if id == 0 { state.disconnected = Some(label(message)); }
        else if let Some(viewer) = state.pending.get(&id).copied() { state.error(viewer, label(message)); }
    }).register();
    let current = state.clone();
    let stop_loop = mainloop.clone();
    let previous = RefCell::new(None);
    let started = Instant::now();
    let command_started = Cell::new(Instant::now());
    let timer = mainloop.loop_().add_timer(move |_| {
        if stopping.load(Ordering::Relaxed) || events.is_closed() || current.borrow().disconnected.is_some() { stop_loop.quit(); return; }
        if !ready.get() {
            if started.elapsed() >= Duration::from_secs(8) { current.borrow_mut().disconnected = Some("Audio graph discovery timed out.".into()); stop_loop.quit(); }
            return;
        }
        if command_sync.get().is_some() && command_started.get().elapsed() >= Duration::from_secs(8) {
            current.borrow_mut().disconnected = Some("Audio command acknowledgement timed out.".into());
            stop_loop.quit(); return;
        }
        let mut commands = Vec::new();
        for _ in 0..if command_sync.get().is_none() { 64 } else { 0 } {
            match requests.try_recv() {
                Ok(Request::Audience { .. }) => {},
                Ok(Request::Command { viewer, epoch, command }) => commands.push((viewer, epoch, command)),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => { stop_loop.quit(); return; }
            }
        }
        let mut volumes = HashSet::new();
        commands.reverse();
        commands.retain(|(viewer, epoch, command)| match command {
            Command::Volume { id, .. } => volumes.insert((*viewer, *epoch, id.clone())),
            _ => true,
        });
        commands.reverse();
        let mut objects = bindings.borrow_mut();
        let mut state = current.borrow_mut();
        for (viewer, requested_epoch, command) in commands {
            let authority = audience.lock().unwrap_or_else(|p| p.into_inner());
            let (_, controller, epoch) = *authority;
            // This shared atomic load admits one dispatch. A handoff invalidates all
            // commands still queued, even while the helper's input pipe is delayed.
            let current_epoch = shared_epoch.as_ref().map_or(epoch, |value| value.load());
            let result = if controller != Some(viewer) || requested_epoch != epoch || requested_epoch != current_epoch { Err(anyhow::anyhow!("Audio control permission changed.")) }
                else { apply(&command, viewer, &generation, &mut state, &objects) };
            if let Err(error) = result { state.error(viewer, error); }
        }
        if !state.pending.is_empty() && command_sync.get().is_none() {
            match core.sync(0) {
                Ok(seq) => { command_sync.set(Some(seq)); command_started.set(Instant::now()); },
                Err(error) => state.disconnected = Some(error.to_string()),
            }
        }
        let subscribed = audience.lock().unwrap_or_else(|p| p.into_inner()).0;
        let mut levels = Vec::new();
        for (id, record) in &mut state.nodes {
            let Some(Binding::Node(binding)) = objects.get_mut(id) else { continue; };
            let kind = record.kind();
            if !subscribed || kind.is_none() {
                binding.meter.take(); binding.meter_retry = None; binding.meter_delay = 1;
                record.meter_active = false; record.meter_error = None;
                continue;
            }
            if let Some(error) = binding.meter.as_ref().and_then(Meter::error) {
                binding.meter.take();
                binding.meter_retry = Some(Instant::now() + Duration::from_secs(binding.meter_delay));
                binding.meter_delay = (binding.meter_delay * 2).min(30);
                record.meter_error = Some(error);
            }
            if binding.meter.is_none() && binding.meter_retry.is_none_or(|at| Instant::now() >= at) {
                binding.meter_retry = Some(Instant::now() + Duration::from_secs(binding.meter_delay));
                binding.meter_delay = (binding.meter_delay * 2).min(30);
                match Meter::new(core.clone(), &record.serial, kind.unwrap()) {
                    Ok(meter) => { binding.meter = Some(meter); record.meter_error = None; }
                    Err(error) => record.meter_error = Some(label(&error.to_string())),
                }
            }
            record.meter_active = binding.meter.as_ref().is_some_and(Meter::active);
            if let Some(meter) = &binding.meter {
                if meter.active() { binding.meter_delay = 1; }
                levels.push(Level { id: record.id(&generation), peak: meter.take_peak() });
            }
        }
        let routing = objects.values().any(|binding| matches!(binding, Binding::Metadata { writable: true, .. }));
        let snapshot = state.snapshot(&generation, routing);
        let mut unconfirmed = Vec::new();
        state.controls.retain(|_, (viewer, command, since)| {
            let id = match command {
                Command::Volume { id, .. } | Command::Mute { id, .. } | Command::Target { id, .. } | Command::Default { id } => id,
                Command::Subscribe { .. } => return false,
            };
            let node = snapshot.nodes.iter().find(|node| node.id == *id);
            let confirmed = node.is_some_and(|node| match command {
                Command::Volume { value, .. } => node.volume.is_some_and(|actual| (actual - *value).abs() < 0.1),
                Command::Mute { value, .. } => node.mute == Some(*value),
                Command::Default { .. } => node.is_default,
                Command::Target { target, .. } => {
                    let target = target.as_ref().or_else(|| snapshot.nodes.iter().find(|n| Some(n.kind) == node.kind.target_kind() && n.is_default).map(|n| &n.id));
                    target.is_some_and(|target| node.targets.contains(target))
                }
                Command::Subscribe { .. } => false,
            });
            if confirmed { return false; }
            if node.is_none() { return false; }
            if since.elapsed() >= Duration::from_secs(3) {
                unconfirmed.push(*viewer); return false;
            }
            true
        });
        for viewer in unconfirmed { state.error(viewer, "Audio change was not confirmed by the session. The object may be unavailable or its policy may reject the change."); }
        if previous.borrow().as_ref() != Some(&snapshot) && events.try_send(Event::State(snapshot.clone())).is_ok() { *previous.borrow_mut() = Some(snapshot); }
        if subscribed { let _ = events.try_send(Event::Levels(levels)); }
        state.errors.retain(|viewer, message| events.try_send(Event::Error { viewer: *viewer, message: message.clone() }).is_err());
    });
    timer.update_timer(Some(Duration::from_millis(100)), Some(Duration::from_millis(100))).into_result()?;
    mainloop.run();
    let reason = state.borrow_mut().disconnected.take();
    if let Some(reason) = reason { bail!("{reason}"); }
    Ok(())
}

/// Call only inside the private audio helper, whose environment selects owned client configuration.
pub fn run(remote: PathBuf, requests: Requests, events: tokio::sync::mpsc::Sender<Event>, stopping: Arc<AtomicBool>) {
    pw::init();
    let audience = requests.audience;
    let shared_epoch = requests.epoch;
    let requests = Rc::new(requests.queue);
    while !stopping.load(Ordering::Relaxed) && !events.is_closed() {
        match connect(&remote, requests.clone(), audience.clone(), shared_epoch.clone(), events.clone(), stopping.clone()) {
            Ok(()) => return,
            Err(error) => {
                let snapshot = Snapshot { error: Some(format!("Session mixer unavailable: {error}")), ..Snapshot::default() };
                let mut sent = false;
                for _ in 0..10 {
                    if !sent { sent = events.try_send(Event::State(snapshot.clone())).is_ok(); }
                    if stopping.load(Ordering::Relaxed) || events.is_closed() { return; }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_properties_preserve_channel_controls() {
        let mut props = Props::default();
        for values in [
            vec![(pw::spa::sys::SPA_PROP_channelVolumes, Value::ValueArray(ValueArray::Float(vec![1.0, 0.5])))],
            vec![(pw::spa::sys::SPA_PROP_volume, Value::Float(0.5))],
            vec![(pw::spa::sys::SPA_PROP_mute, Value::Bool(true))],
            vec![],
        ] {
            let value = Value::Object(Object { type_: pw::spa::sys::SPA_TYPE_OBJECT_Props, id: ParamType::Props.as_raw(),
                properties: values.into_iter().map(|(key, value)| Property { key, value, flags: PropertyFlags::empty() }).collect() });
            let bytes = PodSerializer::serialize(Cursor::new(Vec::new()), &value).unwrap().0.into_inner();
            props.read(Pod::from_bytes(&bytes).unwrap()).unwrap();
        }
        assert!(props.volume_write && props.mute_write);
        assert_eq!(props.mute, Some(true));
        assert_eq!(props.channels, Some(vec![1.0, 0.5]));
        assert!((props.percent().unwrap() - 0.5_f32.cbrt() * 100.0).abs() < 0.001);
        assert!(matches!(&props.volume(50.0)[0].1, Value::ValueArray(ValueArray::Float(v)) if v == &[0.125, 0.0625]));
    }

    #[test]
    fn revocation_bypasses_a_full_command_queue() {
        let (control, requests) = channel();
        for _ in 0..64 {
            control.send(Request::Command { viewer: 1, epoch: 1, command: Command::Mute { id: "old:1".into(), value: true } }).unwrap();
        }
        assert!(matches!(control.send(Request::Command { viewer: 1, epoch: 1, command: Command::Mute { id: "old:1".into(), value: true } }), Err(TrySendError::Full(_))));
        control.send(Request::Audience { subscribed: true, controller: Some(2), epoch: 2 }).unwrap();
        assert_eq!(*requests.audience.lock().unwrap(), (true, Some(2), 2));
        assert!(matches!(requests.queue.try_recv(), Ok(Request::Command { viewer: 1, epoch: 1, .. })));
    }
}
