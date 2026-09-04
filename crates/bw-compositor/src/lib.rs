//! Headless Wayland compositor: composites into dmabufs handed to a [`FrameSink`]
//! and takes input as [`Command`]s. Everything runs on one thread with one calloop loop.

mod cursor;
mod desktop;
mod foreign_toplevel;
mod gpu;
mod grabs;
mod handlers;
mod input;
pub(crate) mod render;
mod xwayland;

use std::{
    path::PathBuf,
    sync::Arc,
    thread::JoinHandle,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use bw_core::{Command, Event, FrameSink, OutputGeometry, WindowInfo};
use smithay::{
    backend::renderer::{ImportDma, damage::OutputDamageTracker},
    desktop::{PopupManager, Space, Window, WindowSurface, layer_map_for_output},
    input::{Seat, SeatState, pointer::CursorImageStatus},
    output::{Mode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::{
        calloop::{
            EventLoop, Interest, LoopHandle, Mode as IoMode, PostAction, channel,
            generic::Generic,
            timer::{TimeoutAction, Timer},
        },
        wayland_protocols_wlr::foreign_toplevel::v1::server::zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1,
        wayland_server::{
            Display, DisplayHandle,
            backend::{ClientData, ClientId, DisconnectReason},
        },
    },
    utils::{Clock, IsAlive, Logical, Monotonic, Point, Rectangle, SERIAL_COUNTER, Transform},
    wayland::{
        compositor::{CompositorClientState, CompositorState},
        dmabuf::{DmabufFeedback, DmabufFeedbackBuilder, DmabufState},
        drm_syncobj::{DrmSyncobjState, supports_syncobj_eventfd},
        fractional_scale::FractionalScaleManagerState,
        output::OutputManagerState,
        pointer_constraints::PointerConstraintsState,
        relative_pointer::RelativePointerManagerState,
        selection::{data_device::DataDeviceState, primary_selection::PrimarySelectionState},
        shell::{
            wlr_layer::WlrLayerShellState,
            xdg::{XdgShellState, decoration::XdgDecorationState},
        },
        shm::ShmState,
        socket::ListeningSocketSource,
        viewporter::ViewporterState,
        xwayland_shell::XWaylandShellState,
    },
    xwayland::X11Wm,
};

pub struct Config {
    pub render_node: PathBuf,
    pub socket_name: String,
    /// Output size until a viewer connects and resizes it.
    pub initial: OutputGeometry,
    /// Shell command started when the first viewer connects, with WAYLAND_DISPLAY, DISPLAY,
    /// BW_WIDTH/BW_HEIGHT (logical output size) and `exec_env` set.
    pub exec: Option<String>,
    pub exec_env: Vec<(String, String)>,
    /// Every new window is fullscreened (for running a nested desktop).
    pub kiosk: bool,
}

pub struct CompositorHandle {
    pub commands: channel::Sender<Command>,
    pub socket_name: String,
    /// X11 display number, if Xwayland came up.
    pub x11_display: Option<u32>,
    pub join: JoinHandle<()>,
}

/// Start the compositor on its own thread. Returns once the Wayland socket exists.
pub fn spawn(cfg: Config, sink: Box<dyn FrameSink>, events: tokio::sync::mpsc::UnboundedSender<Event>) -> Result<CompositorHandle> {
    let (commands, rx) = channel::channel();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let join = std::thread::Builder::new().name("compositor".into()).spawn(move || match State::new(cfg, sink, events) {
        Ok((mut event_loop, mut state)) => {
            // Give Xwayland a moment to come up so --exec children can get DISPLAY.
            state.start_xwayland();
            let deadline = Instant::now() + Duration::from_secs(5);
            while state.xwayland_pending && Instant::now() < deadline {
                let _ = event_loop.dispatch(Some(Duration::from_millis(50)), &mut state);
                let _ = state.dh.flush_clients();
            }
            let _ = ready_tx.send(Ok((state.socket_name.clone(), state.x11_display)));
            state.run(&mut event_loop, rx);
        }
        Err(e) => {
            let _ = ready_tx.send(Err(e));
        }
    })?;
    let (socket_name, x11_display) = ready_rx.recv().context("compositor thread died")??;
    Ok(CompositorHandle { commands, socket_name, x11_display, join })
}

pub struct State {
    pub handle: LoopHandle<'static, State>,
    pub dh: DisplayHandle,
    pub clock: Clock<Monotonic>,
    pub socket_name: String,
    pub running: bool,
    pub exec: Option<String>,
    pub exec_env: Vec<(String, String)>,
    pub kiosk: bool,

    pub gpu: gpu::Gpu,
    pub output: Output,
    pub geometry: OutputGeometry,
    pub damage_tracker: OutputDamageTracker,
    pub dmabuf_state: DmabufState,
    pub syncobj_state: Option<DrmSyncobjState>,
    pub dmabuf_feedback: DmabufFeedback,
    pub sink: Box<dyn FrameSink>,
    pub events: tokio::sync::mpsc::UnboundedSender<Event>,
    pub frame_seq: u64,
    pub frame_interval: Duration,
    pub last_render: Instant,
    /// Something changed since the last render.
    pub dirty: bool,
    /// Next render redraws everything (age 0), e.g. after a viewer connects.
    pub force_full_frame: bool,
    pub viewer_connected: bool,

    pub space: Space<Window>,
    pub popups: PopupManager,
    /// Minimized windows (unmapped from the space) with where to put them back.
    pub minimized: Vec<(Window, Point<i32, Logical>)>,
    pub foreign: foreign_toplevel::ForeignToplevels,
    /// The window `focus_window` last activated.
    pub active: Option<Window>,
    /// What the viewer was last told (desktop API).
    pub last_windows: Vec<WindowInfo>,

    pub seat_state: SeatState<State>,
    pub seat: Seat<State>,
    pub pointer_location: Point<f64, Logical>,
    pub pressed_buttons: std::collections::HashSet<u32>,
    /// A client holds an active pointer lock (mirrored to the browser).
    pub pointer_locked: bool,
    /// The browser gave up its lock: don't re-activate a client's lock until the next click.
    pub lock_suppressed: bool,
    pub cursor_status: CursorImageStatus,
    pub cursor: cursor::CursorTheme,

    pub compositor_state: CompositorState,
    pub shm_state: ShmState,
    pub xdg_shell_state: XdgShellState,
    pub xdg_decoration_state: XdgDecorationState,
    pub layer_shell_state: WlrLayerShellState,
    pub output_manager_state: OutputManagerState,
    pub data_device_state: DataDeviceState,
    pub primary_selection_state: PrimarySelectionState,
    pub viewporter_state: ViewporterState,
    pub fractional_scale_state: FractionalScaleManagerState,
    pub relative_pointer_state: RelativePointerManagerState,
    pub pointer_constraints_state: PointerConstraintsState,
    pub xwayland_shell_state: XWaylandShellState,
    pub xwm: Option<X11Wm>,
    pub x11_display: Option<u32>,
    pub xwayland_pending: bool,
}

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}
impl ClientData for ClientState {
    fn initialized(&self, _: ClientId) {}
    fn disconnected(&self, _: ClientId, _: DisconnectReason) {}
}

fn mode_for(geo: &OutputGeometry) -> Mode {
    Mode { size: (geo.width_px as i32, geo.height_px as i32).into(), refresh: geo.refresh_mhz }
}

impl State {
    fn new(cfg: Config, sink: Box<dyn FrameSink>, events: tokio::sync::mpsc::UnboundedSender<Event>) -> Result<(EventLoop<'static, State>, State)> {
        let event_loop: EventLoop<'static, State> = EventLoop::try_new()?;
        let handle = event_loop.handle();
        let display: Display<State> = Display::new()?;
        let dh = display.handle();

        let mut sink = sink;
        let gpu = gpu::Gpu::new(&cfg.render_node, &cfg.initial, &sink.accepted_formats())?;
        sink.output_changed(cfg.initial, gpu.fourcc as u32, u64::from(gpu.modifier));

        let output = Output::new(
            "BROWSER-1".into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "browser-wayland".into(),
                model: "WebSocket".into(),
            },
        );
        let _global = output.create_global::<State>(&dh);
        let mode = mode_for(&cfg.initial);
        output.change_current_state(Some(mode), Some(Transform::Normal), Some(Scale::Fractional(cfg.initial.scale)), Some((0, 0).into()));
        output.set_preferred(mode);
        let mut space = Space::default();
        space.map_output(&output, (0, 0));

        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(&dh, "browser");
        seat.add_keyboard(Default::default(), 200, 25)?;
        seat.add_pointer();

        let mut dmabuf_state = DmabufState::new();
        let dmabuf_feedback = DmabufFeedbackBuilder::new(gpu.node.dev_id(), gpu.renderer.dmabuf_formats()).build()?;
        let _dmabuf_global = dmabuf_state.create_global_with_default_feedback::<State>(&dh, &dmabuf_feedback);

        let socket = ListeningSocketSource::with_name(&cfg.socket_name)?;
        let socket_name = socket.socket_name().to_string_lossy().into_owned();
        handle.insert_source(socket, |stream, _, state| {
            let _ = state.dh.insert_client(stream, Arc::new(ClientState::default()));
        })?;
        handle.insert_source(Generic::new(display, Interest::READ, IoMode::Level), |_, display, state| {
            // Safety: the display is never dropped from inside the callback.
            unsafe { display.get_mut().dispatch_clients(state).unwrap() };
            Ok(PostAction::Continue)
        })?;

        // Explicit sync: Vulkan clients (GTK4 by default) put no implicit fences on their dmabufs.
        let syncobj_state = supports_syncobj_eventfd(&gpu.drm).then(|| DrmSyncobjState::new::<State>(&dh, gpu.drm.clone()));
        dh.create_global::<State, ZwlrForeignToplevelManagerV1, ()>(foreign_toplevel::VERSION, ());

        let mut state = State {
            handle,
            clock: Clock::new(),
            socket_name,
            running: true,
            exec: cfg.exec,
            exec_env: cfg.exec_env,
            kiosk: cfg.kiosk,
            damage_tracker: OutputDamageTracker::from_output(&output),
            output,
            geometry: cfg.initial,
            gpu,
            dmabuf_state,
            syncobj_state,
            dmabuf_feedback,
            sink,
            events,
            frame_seq: 0,
            frame_interval: Duration::from_nanos(1_000_000_000_000 / cfg.initial.refresh_mhz as u64),
            last_render: Instant::now(),
            dirty: true,
            force_full_frame: true,
            viewer_connected: false,
            space,
            popups: PopupManager::default(),
            minimized: Vec::new(),
            foreign: Default::default(),
            active: None,
            last_windows: Vec::new(),
            seat_state,
            seat,
            pointer_location: (0.0, 0.0).into(),
            pressed_buttons: Default::default(),
            pointer_locked: false,
            lock_suppressed: false,
            cursor_status: CursorImageStatus::default_named(),
            cursor: cursor::CursorTheme::load(),
            compositor_state: CompositorState::new::<State>(&dh),
            shm_state: ShmState::new::<State>(&dh, vec![]),
            xdg_shell_state: XdgShellState::new::<State>(&dh),
            xdg_decoration_state: XdgDecorationState::new::<State>(&dh),
            layer_shell_state: WlrLayerShellState::new::<State>(&dh),
            output_manager_state: OutputManagerState::new_with_xdg_output::<State>(&dh),
            data_device_state: DataDeviceState::new::<State>(&dh),
            primary_selection_state: PrimarySelectionState::new::<State>(&dh),
            viewporter_state: ViewporterState::new::<State>(&dh),
            fractional_scale_state: FractionalScaleManagerState::new::<State>(&dh),
            relative_pointer_state: RelativePointerManagerState::new::<State>(&dh),
            pointer_constraints_state: PointerConstraintsState::new::<State>(&dh),
            xwayland_shell_state: XWaylandShellState::new::<State>(&dh),
            xwm: None,
            x11_display: None,
            xwayland_pending: false,
            dh,
        };
        state.export_cursor(); // the default arrow, before any client sets one
        Ok((event_loop, state))
    }

    /// `--exec` runs once the first viewer has told us its size, so a nested desktop can match it.
    fn spawn_exec_once(&mut self, geo: OutputGeometry) {
        if let Some(cmd) = self.exec.take() {
            self.spawn_client(&cmd, geo);
        }
    }

    /// Run a shell command as a client of this compositor: WAYLAND_DISPLAY, DISPLAY,
    /// BW_WIDTH/BW_HEIGHT (logical size of `geo`) and the toolkit backends set.
    pub fn spawn_client(&self, cmd: &str, geo: OutputGeometry) {
        let mut command = std::process::Command::new("sh");
        command
            .arg("-c")
            .arg(cmd)
            .env("WAYLAND_DISPLAY", &self.socket_name)
            .env("BW_WIDTH", ((geo.width_px as f64 / geo.scale).round() as u32).to_string())
            .env("BW_HEIGHT", ((geo.height_px as f64 / geo.scale).round() as u32).to_string())
            .env_remove("WAYLAND_SOCKET")
            .env("GDK_BACKEND", "wayland")
            // GTK 4.22's default Vulkan renderer intermittently draws hairline slivers from the window corner
            // (seen with gnome-text-editor and mutter-devkit, never with its GL renderer). Drop when GTK fixes it.
            .env("GSK_RENDERER", "ngl")
            .env("QT_QPA_PLATFORM", "wayland")
            .env("SDL_VIDEODRIVER", "wayland")
            .env("MOZ_ENABLE_WAYLAND", "1")
            .envs(self.exec_env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        match self.x11_display {
            Some(d) => command.env("DISPLAY", format!(":{d}")),
            None => command.env_remove("DISPLAY"),
        };
        match command.spawn() {
            Ok(mut child) => {
                let cmd = cmd.to_string();
                std::thread::spawn(move || tracing::info!(cmd, status = ?child.wait(), "client exited"));
            }
            Err(e) => tracing::error!("spawn {cmd:?}: {e}"),
        }
    }

    fn run(mut self, event_loop: &mut EventLoop<'static, State>, rx: channel::Channel<Command>) {
        let handle = event_loop.handle();
        handle
            .insert_source(rx, |ev, _, state| match ev {
                channel::Event::Msg(cmd) => state.handle_command(cmd),
                channel::Event::Closed => state.running = false,
            })
            .unwrap();
        let interval = self.frame_interval;
        handle
            .insert_source(Timer::from_duration(interval), move |_, _, state| {
                state.tick();
                TimeoutAction::ToDuration(interval)
            })
            .unwrap();
        let signal = event_loop.get_signal();
        event_loop
            .run(None, &mut self, |state| {
                state.space.refresh();
                state.minimized.retain(|(w, _)| w.alive());
                if state.active.as_ref().is_some_and(|w| !w.alive()) {
                    state.active = None;
                }
                state.popups.cleanup();
                state.refresh_foreign_toplevels();
                state.refresh_windows();
                if state.pointer_locked {
                    // constraints can go away without any input (client destroyed it, focus left)
                    let pointer = state.seat.get_pointer().unwrap();
                    state.sync_pointer_lock(&pointer);
                }
                // Render right after input/commits when a frame period has passed, instead of waiting for the timer.
                if state.dirty && state.last_render.elapsed() >= state.frame_interval {
                    state.tick();
                }
                let _ = state.dh.flush_clients();
                if !state.running {
                    signal.stop();
                }
            })
            .unwrap();
    }

    /// Hide a window until a taskbar (or its own client) asks for it back; focus moves to the top-most window left.
    pub fn minimize(&mut self, window: &Window) {
        let Some(loc) = self.space.element_location(window) else { return };
        self.space.unmap_elem(window);
        self.minimized.push((window.clone(), loc));
        window.set_activated(false); // out of the space, so focus_window can't reach it any more
        if let Some(t) = window.toplevel() {
            t.send_pending_configure();
        }
        let next = self.space.elements().rev().find(|w| w.x11_surface().is_none_or(|x| !x.is_override_redirect())).cloned();
        self.focus_window(next.as_ref(), SERIAL_COUNTER.next_serial());
        self.dirty = true;
    }

    pub fn unminimize(&mut self, window: &Window) {
        if let Some(i) = self.minimized.iter().position(|(w, _)| w == window) {
            let (window, loc) = self.minimized.remove(i);
            self.space.map_element(window, loc, false);
            self.relayout(); // the output or the panels may have changed meanwhile
        }
    }

    /// The output minus the panels' exclusive zones: where windows go.
    pub fn work_area(&self) -> Rectangle<i32, Logical> {
        layer_map_for_output(&self.output).non_exclusive_zone()
    }

    /// Keep at least a corner of a window at `loc` reachable in the work area.
    pub fn clamp_to_output(&self, loc: Point<i32, Logical>) -> Point<i32, Logical> {
        let work = self.work_area();
        let max = |lo: i32, len: i32| (lo + len - 64).max(lo);
        Point::from((loc.x.clamp(work.loc.x, max(work.loc.x, work.size.w)), loc.y.clamp(work.loc.y, max(work.loc.y, work.size.h))))
    }

    /// Apply a new output size/scale from the viewer.
    pub fn resize(&mut self, geo: OutputGeometry) {
        self.spawn_exec_once(geo);
        if geo == self.geometry {
            return;
        }
        let mode = mode_for(&geo);
        self.output.change_current_state(Some(mode), None, Some(Scale::Fractional(geo.scale)), None);
        self.output.set_preferred(mode);
        self.gpu.swapchain.resize(geo.width_px, geo.height_px);
        self.geometry = geo;
        layer_map_for_output(&self.output).arrange();
        self.relayout();
        self.sink.output_changed(geo, self.gpu.fourcc as u32, u64::from(self.gpu.modifier));
        self.force_full_frame = true;
        self.dirty = true;
    }

    /// Re-arrange the panels; re-fit the windows if that moved the work area.
    /// (`arrange()` only reports panel size changes, not a panel coming or going.)
    pub fn arrange_layers(&mut self) {
        let mut layers = layer_map_for_output(&self.output);
        let before = layers.non_exclusive_zone();
        layers.arrange();
        let changed = layers.non_exclusive_zone() != before;
        drop(layers);
        if changed {
            self.relayout();
        }
    }

    /// Re-fit every window after the output or the panels' exclusive zones changed.
    pub fn relayout(&mut self) {
        let output = self.space.output_geometry(&self.output).unwrap_or_default();
        let work = self.work_area();
        let scale = self.geometry.scale;
        for window in self.space.elements().cloned().collect::<Vec<_>>() {
            // maximized windows fill the work area, fullscreen ones the whole output
            let filled = match window.underlying_surface() {
                WindowSurface::Wayland(toplevel) => {
                    let rect = toplevel.with_pending_state(|s| {
                        use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State as S;
                        s.bounds = Some(work.size);
                        let rect = if s.states.contains(S::Fullscreen) {
                            Some(output)
                        } else if s.states.contains(S::Maximized) {
                            Some(work)
                        } else {
                            None
                        };
                        if let Some(r) = rect {
                            s.size = Some(r.size);
                        }
                        rect
                    });
                    toplevel.send_pending_configure();
                    rect
                }
                WindowSurface::X11(x11) => {
                    let rect = if x11.is_fullscreen() {
                        Some(output)
                    } else if x11.is_maximized() {
                        Some(work)
                    } else {
                        None
                    };
                    if let Some(r) = rect {
                        let _ = x11.configure(r);
                    }
                    rect
                }
            };
            // map_element raises: re-map every window in this back-to-front order to keep the stacking
            let loc = match (filled, self.space.element_location(&window)) {
                (Some(rect), _) => rect.loc,
                (None, Some(loc)) => {
                    let clamped = self.clamp_to_output(loc); // keep a corner of every floating window reachable
                    if let (true, WindowSurface::X11(x11)) = (clamped != loc, window.underlying_surface()) {
                        let _ = x11.configure(Rectangle::new(clamped, window.geometry().size));
                    }
                    clamped
                }
                (None, None) => continue,
            };
            self.space.map_element(window.clone(), loc, false);
            window.with_surfaces(|_, states| {
                smithay::wayland::fractional_scale::with_fractional_scale(states, |f| f.set_preferred_scale(scale));
            });
        }
        self.dirty = true;
    }
}
