//! Wayland protocol handlers and their delegate macros.

use std::{cell::RefCell, os::unix::io::OwnedFd};

use smithay::{
    backend::{
        allocator::dmabuf::Dmabuf,
        renderer::ImportDma,
    },
    delegate_compositor, delegate_data_device, delegate_dmabuf, delegate_fractional_scale, delegate_layer_shell,
    delegate_output, delegate_pointer_constraints, delegate_primary_selection, delegate_relative_pointer, delegate_seat,
    delegate_shm, delegate_drm_syncobj, delegate_viewporter, delegate_xdg_decoration, delegate_xdg_shell,
    desktop::{
        LayerSurface, PopupKind, Window, WindowSurfaceType, find_popup_root_surface, get_popup_toplevel_coords,
        layer_map_for_output,
    },
    input::{
        Seat, SeatHandler, SeatState,
        pointer::{CursorImageStatus, Focus, GrabStartData, PointerHandle},
    },
    reexports::{
        calloop::Interest,
        wayland_protocols::xdg::{
            decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode,
            shell::server::xdg_toplevel::{self, ResizeEdge},
        },
        wayland_server::{
            Client, Resource,
            protocol::{wl_buffer::WlBuffer, wl_output::WlOutput, wl_seat::WlSeat, wl_surface::WlSurface},
        },
    },
    utils::{Logical, Point, Rectangle, SERIAL_COUNTER, Serial},
    wayland::{
        buffer::BufferHandler,
        seat::WaylandFocus,
        compositor::{
            BufferAssignment, CompositorClientState, CompositorHandler, CompositorState, SurfaceAttributes, add_blocker,
            add_pre_commit_hook, get_parent, is_sync_subsurface, with_states,
        },
        dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier, get_dmabuf},
        drm_syncobj::{DrmSyncobjCachedState, DrmSyncobjHandler, DrmSyncobjState},
        fractional_scale::{FractionalScaleHandler, with_fractional_scale},
        output::OutputHandler,
        pointer_constraints::PointerConstraintsHandler,
        selection::{
            SelectionHandler, SelectionSource, SelectionTarget,
            data_device::{ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler, set_data_device_focus},
            primary_selection::{PrimarySelectionHandler, PrimarySelectionState, set_primary_focus},
        },
        shell::{
            wlr_layer::{
                KeyboardInteractivity, Layer, LayerSurface as WlrLayerSurface, LayerSurfaceData, WlrLayerShellHandler,
                WlrLayerShellState,
            },
            xdg::{
                PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
                decoration::XdgDecorationHandler,
            },
        },
        shm::{ShmHandler, ShmState},
        viewporter::ViewporterState,
    },
};

use smithay::xwayland::XWaylandClientData;

use crate::{ClientState, State, grabs};

impl State {
    pub fn window_for(&self, surface: &WlSurface) -> Option<Window> {
        self.space.elements().find(|w| w.wl_surface().is_some_and(|s| *s == *surface)).cloned()
    }

    /// The pointer grab this request belongs to, if the requesting client owns the focused surface.
    fn grab_start(&self, seat: &Seat<State>, surface: &WlSurface, serial: Serial) -> Option<GrabStartData<State>> {
        let pointer = seat.get_pointer()?;
        if !pointer.has_grab(serial) {
            return None;
        }
        let start = pointer.grab_start_data()?;
        let (focus, _) = start.focus.as_ref()?;
        focus.id().same_client_as(&surface.id()).then_some(start)
    }

    fn unconstrain_popup(&self, popup: &PopupSurface) {
        let kind = PopupKind::Xdg(popup.clone());
        let Ok(root) = find_popup_root_surface(&kind) else { return };
        // the popup's parent is a window or a layer surface (panel menus)
        let parent_loc = match self.window_for(&root) {
            Some(window) => self.space.element_geometry(&window).map(|g| g.loc),
            None => {
                let layers = layer_map_for_output(&self.output);
                layers.layer_for_surface(&root, WindowSurfaceType::TOPLEVEL).and_then(|l| layers.layer_geometry(l)).map(|g| g.loc)
            }
        };
        let (Some(mut target), Some(parent_loc)) = (self.space.output_geometry(&self.output), parent_loc) else { return };
        target.loc -= get_popup_toplevel_coords(&kind);
        target.loc -= parent_loc;
        popup.with_pending_state(|s| s.geometry = s.positioner.get_unconstrained_geometry(target));
    }
}

impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }
    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        if let Some(x) = client.get_data::<XWaylandClientData>() {
            return &x.compositor_state; // Xwayland is registered by Smithay with its own client data
        }
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn new_surface(&mut self, surface: &WlSurface) {
        add_pre_commit_hook::<Self, _>(surface, |state, _dh, surface| {
            // Don't sample a client's dmabuf before its GPU work has finished: wait for the explicit-sync
            // acquire point when the client gave one, else for the dmabuf's implicit fences.
            let (dmabuf, acquire) = with_states(surface, |data| {
                let dmabuf = match data.cached_state.get::<SurfaceAttributes>().pending().buffer.as_ref() {
                    Some(BufferAssignment::NewBuffer(b)) => get_dmabuf(b).cloned().ok(),
                    _ => None,
                };
                (dmabuf, data.cached_state.get::<DrmSyncobjCachedState>().pending().acquire_point.clone())
            });
            let Some(dmabuf) = dmabuf else { return };
            let Some(client) = surface.client() else { return };
            let cleared = move |state: &mut State| {
                let dh = state.dh.clone();
                state.client_compositor_state(&client).blocker_cleared(state, &dh);
            };
            match acquire.and_then(|point| point.generate_blocker().ok()) {
                Some((blocker, source)) => {
                    if state.handle.insert_source(source, move |_, _, state| { cleared(state); Ok(()) }).is_ok() {
                        add_blocker(surface, blocker);
                    }
                }
                None => {
                    let Ok((blocker, source)) = dmabuf.generate_blocker(Interest::READ) else { return };
                    if state.handle.insert_source(source, move |_, _, state| { cleared(state); Ok(()) }).is_ok() {
                        add_blocker(surface, blocker);
                    }
                }
            }
        });
    }

    fn destroyed(&mut self, surface: &WlSurface) {
        if matches!(&self.cursor_status, CursorImageStatus::Surface(s) if s == surface) {
            self.cursor_status = CursorImageStatus::default_named();
            self.export_cursor();
        }
        self.dirty = true;
    }

    fn commit(&mut self, surface: &WlSurface) {
        smithay::backend::renderer::utils::on_commit_buffer_handler::<Self>(surface);
        if !is_sync_subsurface(surface) {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            if let Some(window) = self.window_for(&root) {
                window.on_commit();
                // wl_surface.offset: the client moved its buffer origin, so move the window with it.
                // map_element always puts the window on top, so only do it for a real move: some
                // clients attach with a zero offset on every frame.
                let delta = with_states(&root, |s| s.cached_state.get::<SurfaceAttributes>().current().buffer_delta.take());
                if let (Some(delta), Some(loc)) = (delta.filter(|d| d.x != 0 || d.y != 0), self.space.element_location(&window)) {
                    self.space.map_element(window, loc + delta, false);
                }
            }
        }
        self.popups.commit(surface);
        if matches!(&self.cursor_status, CursorImageStatus::Surface(s) if s == surface) {
            self.export_cursor(); // client redrew its cursor
            return;
        }
        ensure_initial_configure(surface, self);
        grabs::handle_commit(&mut self.space, surface);
        self.dirty = true;
    }
}

fn ensure_initial_configure(surface: &WlSurface, state: &mut State) {
    if let Some(window) = state.window_for(surface) {
        if let Some(toplevel) = window.toplevel() {
            if !toplevel.is_initial_configure_sent() {
                toplevel.send_configure();
            }
        }
    } else if let Some(PopupKind::Xdg(popup)) = state.popups.find_popup(surface) {
        if !popup.is_initial_configure_sent() {
            let _ = popup.send_configure();
        }
    } else {
        let layer = layer_map_for_output(&state.output).layer_for_surface(surface, WindowSurfaceType::TOPLEVEL).cloned();
        let Some(layer) = layer else { return };
        // arrange first so the configure carries the size the client's anchors/size give it
        state.arrange_layers();
        let configured = with_states(surface, |s| s.data_map.get::<LayerSurfaceData>().unwrap().lock().unwrap().initial_configure_sent);
        if !configured {
            layer.layer_surface().send_configure();
        }
        // launchers take the keyboard while they are up; the panels only ask on demand (handled on click)
        if layer.cached_state().keyboard_interactivity == KeyboardInteractivity::Exclusive && matches!(layer.layer(), Layer::Top | Layer::Overlay) {
            let keyboard = state.seat.get_keyboard().unwrap();
            keyboard.set_focus(state, Some(layer.wl_surface().clone()), SERIAL_COUNTER.next_serial());
        }
    }
}

impl BufferHandler for State {
    fn buffer_destroyed(&mut self, _buffer: &WlBuffer) {}
}
impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let work = self.work_area();
        surface.with_pending_state(|s| {
            s.bounds = Some(work.size);
            s.decoration_mode = Some(DecorationMode::ClientSide);
            s.capabilities.replace([xdg_toplevel::WmCapabilities::Maximize, xdg_toplevel::WmCapabilities::Fullscreen]);
            if !self.kiosk {
                s.capabilities.set(xdg_toplevel::WmCapabilities::Minimize); // a nested desktop has nowhere to come back from
            }
        });
        if self.kiosk {
            let size = self.space.output_geometry(&self.output).map(|g| g.size).unwrap_or_default();
            surface.with_pending_state(|s| {
                s.states.set(xdg_toplevel::State::Fullscreen);
                s.size = Some(size);
            });
            self.space.map_element(Window::new_wayland_window(surface), (0, 0), true);
            return;
        }
        let n = self.space.elements().count() as i32 % 10;
        self.space.map_element(Window::new_wayland_window(surface), work.loc + Point::from((40 + 30 * n, 40 + 30 * n)), true);
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        self.unconstrain_popup(&surface);
        let _ = self.popups.track_popup(PopupKind::Xdg(surface));
    }

    fn reposition_request(&mut self, surface: PopupSurface, positioner: PositionerState, token: u32) {
        surface.with_pending_state(|s| {
            s.geometry = positioner.get_geometry();
            s.positioner = positioner;
        });
        self.unconstrain_popup(&surface);
        surface.send_repositioned(token);
    }

    fn grab(&mut self, surface: PopupSurface, seat: WlSeat, serial: Serial) {
        use smithay::desktop::{PopupKeyboardGrab, PopupPointerGrab, PopupUngrabStrategy};
        let seat: Seat<State> = Seat::from_resource(&seat).unwrap();
        let kind = PopupKind::Xdg(surface);
        let Some(root) = find_popup_root_surface(&kind).ok() else { return };
        let Ok(mut grab) = self.popups.grab_popup(root, kind, &seat, serial) else { return };
        if let Some(keyboard) = seat.get_keyboard() {
            if keyboard.is_grabbed() && !(keyboard.has_grab(serial) || keyboard.has_grab(grab.previous_serial().unwrap_or(serial))) {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            keyboard.set_focus(self, grab.current_grab(), serial);
            keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
        }
        if let Some(pointer) = seat.get_pointer() {
            if pointer.is_grabbed() && !(pointer.has_grab(serial) || pointer.has_grab(grab.previous_serial().unwrap_or_else(|| grab.serial()))) {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            pointer.set_grab(self, PopupPointerGrab::new(&grab), serial, Focus::Keep);
        }
    }

    fn move_request(&mut self, surface: ToplevelSurface, seat: WlSeat, serial: Serial) {
        let seat = Seat::from_resource(&seat).unwrap();
        let Some(start_data) = self.grab_start(&seat, surface.wl_surface(), serial) else { return };
        let Some(window) = self.window_for(surface.wl_surface()) else { return };
        let initial_location = self.space.element_location(&window).unwrap();
        let grab = grabs::MoveGrab { start_data, window, initial_location };
        seat.get_pointer().unwrap().set_grab(self, grab, serial, Focus::Clear);
    }

    fn resize_request(&mut self, surface: ToplevelSurface, seat: WlSeat, serial: Serial, edges: ResizeEdge) {
        let seat = Seat::from_resource(&seat).unwrap();
        let Some(start_data) = self.grab_start(&seat, surface.wl_surface(), serial) else { return };
        let Some(window) = self.window_for(surface.wl_surface()) else { return };
        let mut initial_rect = window.geometry();
        initial_rect.loc = self.space.element_location(&window).unwrap();
        grabs::ResizeState::with(surface.wl_surface(), |s| *s = grabs::ResizeState::Resizing { edges, initial_rect });
        surface.with_pending_state(|s| s.states.set(xdg_toplevel::State::Resizing));
        surface.send_pending_configure();
        let grab = grabs::ResizeGrab { start_data, window, edges, initial_rect, last_size: initial_rect.size };
        seat.get_pointer().unwrap().set_grab(self, grab, serial, Focus::Clear);
    }

    fn toplevel_destroyed(&mut self, _surface: ToplevelSurface) {
        self.dirty = true;
    }
    fn popup_destroyed(&mut self, _surface: PopupSurface) {
        self.dirty = true;
    }

    fn minimize_request(&mut self, surface: ToplevelSurface) {
        if let Some(window) = self.window_for(surface.wl_surface()) {
            self.minimize(&window);
        }
    }
    fn maximize_request(&mut self, surface: ToplevelSurface) {
        self.fill_output(&surface, xdg_toplevel::State::Maximized);
    }
    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        self.unfill_output(&surface, xdg_toplevel::State::Maximized);
    }
    fn fullscreen_request(&mut self, surface: ToplevelSurface, _output: Option<WlOutput>) {
        self.fill_output(&surface, xdg_toplevel::State::Fullscreen);
    }
    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        self.unfill_output(&surface, xdg_toplevel::State::Fullscreen);
    }
}

/// Where a window was before it got maximized/fullscreened.
type RestoreLocation = RefCell<Option<Point<i32, Logical>>>;

impl State {
    /// Fullscreen covers the whole output, maximized only the work area.
    pub fn fill_rect(&self, fullscreen: bool) -> Rectangle<i32, Logical> {
        if fullscreen { self.space.output_geometry(&self.output).unwrap_or_default() } else { self.work_area() }
    }

    fn fill_output(&mut self, surface: &ToplevelSurface, what: xdg_toplevel::State) {
        let geo = self.fill_rect(what == xdg_toplevel::State::Fullscreen);
        let Some(window) = self.window_for(surface.wl_surface()) else { return };
        window.user_data().insert_if_missing(RestoreLocation::default);
        let restore = window.user_data().get::<RestoreLocation>().unwrap();
        if restore.borrow().is_none() {
            *restore.borrow_mut() = self.space.element_location(&window);
        }
        surface.with_pending_state(|s| {
            s.states.set(what);
            s.size = Some(geo.size);
        });
        self.space.map_element(window, geo.loc, true);
        surface.send_pending_configure();
    }
    fn unfill_output(&mut self, surface: &ToplevelSurface, what: xdg_toplevel::State) {
        let other = match what {
            xdg_toplevel::State::Maximized => xdg_toplevel::State::Fullscreen,
            _ => xdg_toplevel::State::Maximized,
        };
        let still_filled = surface.with_pending_state(|s| {
            s.states.unset(what);
            s.states.contains(other)
        });
        if still_filled {
            return self.fill_output(surface, other); // e.g. unfullscreen back to maximized: re-fit to the work area
        }
        surface.with_pending_state(|s| s.size = None);
        if let Some(window) = self.window_for(surface.wl_surface()) {
            let saved = window.user_data().get::<RestoreLocation>().and_then(|r| r.borrow_mut().take());
            if let Some(loc) = saved {
                let loc = self.clamp_to_output(loc);
                self.space.map_element(window, loc, true);
            }
        }
        surface.send_pending_configure();
    }
}

impl WlrLayerShellHandler for State {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(&mut self, surface: WlrLayerSurface, _output: Option<WlOutput>, _layer: Layer, namespace: String) {
        // single output: whatever the client asked for, it gets ours
        let _ = layer_map_for_output(&self.output).map_layer(&LayerSurface::new(surface, namespace));
    }

    fn new_popup(&mut self, _parent: WlrLayerSurface, popup: PopupSurface) {
        self.unconstrain_popup(&popup);
    }

    fn layer_destroyed(&mut self, surface: WlrLayerSurface) {
        let mut layers = layer_map_for_output(&self.output);
        let layer = layers.layers().find(|l| l.layer_surface() == &surface).cloned();
        if let Some(layer) = layer {
            layers.unmap_layer(&layer); // re-arranges by itself
        }
        drop(layers);
        self.relayout();
    }
}
delegate_layer_shell!(State);

impl XdgDecorationHandler for State {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|s| s.decoration_mode = Some(DecorationMode::ClientSide));
    }
    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: DecorationMode) {
        toplevel.with_pending_state(|s| s.decoration_mode = Some(DecorationMode::ClientSide));
        if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
    }
    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        self.request_mode(toplevel, DecorationMode::ClientSide);
    }
}

impl SeatHandler for State {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<State> {
        &mut self.seat_state
    }
    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        self.cursor_status = image;
        self.export_cursor();
    }
    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let client = focused.and_then(|s| self.dh.get_client(s.id()).ok());
        set_data_device_focus(&self.dh, seat, client.clone());
        set_primary_focus(&self.dh, seat, client);
    }
}

impl SelectionHandler for State {
    type SelectionUserData = ();

    /// A Wayland client took the clipboard/primary selection: offer it to X11 clients too.
    fn new_selection(&mut self, ty: SelectionTarget, source: Option<SelectionSource>, _seat: Seat<Self>) {
        if let Some(xwm) = self.xwm.as_mut() {
            if let Err(e) = xwm.new_selection(ty, source.map(|s| s.mime_types())) {
                tracing::warn!("xwayland selection: {e:?}");
            }
        }
    }

    /// A Wayland client wants data from a selection owned by an X11 client.
    fn send_selection(&mut self, ty: SelectionTarget, mime_type: String, fd: OwnedFd, _seat: Seat<Self>, _user_data: &()) {
        let handle = self.handle.clone();
        if let Some(xwm) = self.xwm.as_mut() {
            if let Err(e) = xwm.send_selection(ty, mime_type, fd, handle) {
                tracing::warn!("xwayland selection transfer: {e:?}");
            }
        }
    }
}
impl DataDeviceHandler for State {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}
impl PrimarySelectionHandler for State {
    fn primary_selection_state(&self) -> &PrimarySelectionState {
        &self.primary_selection_state
    }
}
impl ClientDndGrabHandler for State {}
impl ServerDndGrabHandler for State {
    fn send(&mut self, _mime_type: String, _fd: OwnedFd, _seat: Seat<Self>) {}
}

impl OutputHandler for State {}

impl DrmSyncobjHandler for State {
    fn drm_syncobj_state(&mut self) -> Option<&mut DrmSyncobjState> {
        self.syncobj_state.as_mut()
    }
}
delegate_drm_syncobj!(State);

impl DmabufHandler for State {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }
    fn dmabuf_imported(&mut self, _global: &DmabufGlobal, dmabuf: Dmabuf, notifier: ImportNotifier) {
        if self.gpu.renderer.import_dmabuf(&dmabuf, None).is_ok() {
            dmabuf.set_node(self.gpu.node);
            let _ = notifier.successful::<State>();
        } else {
            notifier.failed();
        }
    }
}

impl FractionalScaleHandler for State {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        let scale = self.output.current_scale().fractional_scale();
        with_states(&surface, |states| with_fractional_scale(states, |f| f.set_preferred_scale(scale)));
    }
}

impl PointerConstraintsHandler for State {
    fn new_constraint(&mut self, surface: &WlSurface, pointer: &PointerHandle<Self>) {
        // Activate right away if the pointer is already inside; otherwise on entering it.
        if !self.lock_suppressed && pointer.current_focus().as_ref() == Some(surface) {
            if let Some((_, origin)) = self.surface_under(self.pointer_location) {
                crate::input::activate_lock(surface, pointer, self.pointer_location - origin);
            }
        }
        self.sync_pointer_lock(pointer);
    }
    // ponytail: the browser's own pointer is wherever the user left it, so a warp hint has nothing to move;
    // the next absolute motion resyncs.
    fn cursor_position_hint(&mut self, _surface: &WlSurface, _pointer: &PointerHandle<Self>, _location: Point<f64, Logical>) {}
}

impl AsRef<ViewporterState> for State {
    fn as_ref(&self) -> &ViewporterState {
        &self.viewporter_state
    }
}

delegate_compositor!(State);
delegate_shm!(State);
delegate_xdg_shell!(State);
delegate_xdg_decoration!(State);
delegate_seat!(State);
delegate_data_device!(State);
delegate_primary_selection!(State);
delegate_output!(State);
delegate_dmabuf!(State);
delegate_viewporter!(State);
delegate_fractional_scale!(State);
delegate_relative_pointer!(State);
delegate_pointer_constraints!(State);
