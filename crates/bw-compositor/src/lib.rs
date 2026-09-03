//! Headless Wayland compositor: composites into dmabufs handed to a [`FrameSink`]
//! and takes input as [`Command`]s. Everything runs on one thread with one calloop loop.

mod cursor;
mod gpu;
mod grabs;
mod handlers;
mod input;
mod render;
mod xwayland;

use std::{
    path::PathBuf,
    sync::Arc,
    thread::JoinHandle,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use bw_core::{Command, Event, FrameSink, OutputGeometry};
use smithay::{
    backend::renderer::{ImportDma, damage::OutputDamageTracker},
    desktop::{PopupManager, Space, Window, WindowSurface},
    input::{Seat, SeatState, pointer::CursorImageStatus},
    output::{Mode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::{
        calloop::{
            EventLoop, Interest, LoopHandle, Mode as IoMode, PostAction, channel,
            generic::Generic,
            timer::{TimeoutAction, Timer},
        },
        wayland_server::{
            Display, DisplayHandle,
            backend::{ClientData, ClientId, DisconnectReason},
        },
    },
    utils::{Clock, Logical, Monotonic, Point, Rectangle, Transform},
    wayland::{
        compositor::{CompositorClientState, CompositorState},
        dmabuf::{DmabufFeedback, DmabufFeedbackBuilder, DmabufState},
        fractional_scale::FractionalScaleManagerState,
        output::OutputManagerState,
        pointer_constraints::PointerConstraintsState,
        relative_pointer::RelativePointerManagerState,
        selection::{data_device::DataDeviceState, primary_selection::PrimarySelectionState},
        shell::xdg::{XdgShellState, decoration::XdgDecorationState},
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
            while state.x11_display.is_none() && Instant::now() < deadline {
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

    pub gpu: gpu::Gpu,
    pub output: Output,
    pub geometry: OutputGeometry,
    pub damage_tracker: OutputDamageTracker,
    pub dmabuf_state: DmabufState,
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

        let mut state = State {
            handle,
            clock: Clock::new(),
            socket_name,
            running: true,
            damage_tracker: OutputDamageTracker::from_output(&output),
            output,
            geometry: cfg.initial,
            gpu,
            dmabuf_state,
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
            dh,
        };
        state.export_cursor(); // the default arrow, before any client sets one
        Ok((event_loop, state))
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
                state.popups.cleanup();
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

    /// Apply a new output size/scale from the viewer.
    pub fn resize(&mut self, geo: OutputGeometry) {
        if geo == self.geometry {
            return;
        }
        let mode = mode_for(&geo);
        self.output.change_current_state(Some(mode), None, Some(Scale::Fractional(geo.scale)), None);
        self.output.set_preferred(mode);
        self.gpu.swapchain.resize(geo.width_px, geo.height_px);
        self.geometry = geo;
        let size = self.space.output_geometry(&self.output).map(|g| g.size).unwrap_or_default();
        for window in self.space.elements().cloned().collect::<Vec<_>>() {
            let filled = match window.underlying_surface() {
                WindowSurface::Wayland(toplevel) => {
                    let filled = toplevel.with_pending_state(|s| {
                        use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State as S;
                        s.bounds = Some(size);
                        let filled = s.states.contains(S::Maximized) || s.states.contains(S::Fullscreen);
                        if filled {
                            s.size = Some(size);
                        }
                        filled
                    });
                    toplevel.send_pending_configure();
                    filled
                }
                WindowSurface::X11(x11) => {
                    let filled = x11.is_maximized() || x11.is_fullscreen();
                    if filled {
                        let _ = x11.configure(Rectangle::new((0, 0).into(), size));
                    }
                    filled
                }
            };
            // keep a corner of every floating window reachable
            if let (false, Some(loc)) = (filled, self.space.element_location(&window)) {
                let clamped = Point::from((loc.x.clamp(0, (size.w - 64).max(0)), loc.y.clamp(0, (size.h - 64).max(0))));
                if clamped != loc {
                    self.space.map_element(window.clone(), clamped, false);
                    if let WindowSurface::X11(x11) = window.underlying_surface() {
                        let _ = x11.configure(Rectangle::new(clamped, window.geometry().size));
                    }
                }
            }
            window.with_surfaces(|_, states| {
                smithay::wayland::fractional_scale::with_fractional_scale(states, |f| f.set_preferred_scale(geo.scale));
            });
        }
        self.sink.output_changed(geo, self.gpu.fourcc as u32, u64::from(self.gpu.modifier));
        self.force_full_frame = true;
        self.dirty = true;
    }
}
