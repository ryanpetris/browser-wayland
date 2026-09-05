//! Wayland protocol handlers and their delegate macros.

use std::{borrow::Cow, cell::RefCell, os::unix::io::OwnedFd};

use smithay::{
    backend::renderer::utils::with_renderer_surface_state,
    input::pointer::CursorImageSurfaceData,
    backend::{
        allocator::dmabuf::Dmabuf,
        input::KeyState,
        renderer::ImportDma,
    },
    delegate_compositor, delegate_data_device, delegate_dmabuf, delegate_fractional_scale, delegate_layer_shell,
    delegate_output, delegate_pointer_constraints, delegate_primary_selection, delegate_relative_pointer, delegate_seat,
    delegate_shm, delegate_drm_syncobj, delegate_viewporter, delegate_xdg_decoration, delegate_xdg_shell,
    desktop::{
        LayerSurface, PopupKind, Window, WindowSurface, WindowSurfaceType, find_popup_root_surface, get_popup_toplevel_coords,
        layer_map_for_output,
    },
    input::{
        Seat, SeatHandler, SeatState,
        keyboard::{KeyboardTarget, KeysymHandle, ModifiersState},
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
    utils::{IsAlive, Logical, Point, Rectangle, SERIAL_COUNTER, Serial},
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

use smithay::xwayland::{X11Surface, XWaylandClientData};

use crate::{ClientState, State, grabs};

/// What the keyboard focuses. An X11 window is focused as itself rather than as its surface: Smithay's
/// target for it also sets the X input focus, without which X11 clients see no FocusIn and Chromium,
/// for one, opens no menus.
#[derive(Clone, Debug, PartialEq)]
pub enum KeyboardFocus {
    Wayland(WlSurface),
    X11(X11Surface),
}

impl KeyboardFocus {
    pub fn of(window: &Window) -> Self {
        match window.underlying_surface() {
            WindowSurface::Wayland(t) => Self::Wayland(t.wl_surface().clone()),
            WindowSurface::X11(x) => Self::X11(x.clone()),
        }
    }
}

impl From<WlSurface> for KeyboardFocus {
    fn from(s: WlSurface) -> Self {
        Self::Wayland(s)
    }
}

impl From<PopupKind> for KeyboardFocus {
    fn from(p: PopupKind) -> Self {
        Self::Wayland(p.wl_surface().clone())
    }
}

/// Popup grabs point the pointer at the keyboard's focus; theirs is always an xdg surface.
impl From<KeyboardFocus> for WlSurface {
    fn from(f: KeyboardFocus) -> Self {
        match f {
            KeyboardFocus::Wayland(s) => s,
            KeyboardFocus::X11(_) => unreachable!("popup grab roots are xdg surfaces"),
        }
    }
}

impl IsAlive for KeyboardFocus {
    fn alive(&self) -> bool {
        match self {
            Self::Wayland(s) => s.alive(),
            Self::X11(x) => x.alive(),
        }
    }
}

impl WaylandFocus for KeyboardFocus {
    fn wl_surface(&self) -> Option<Cow<'_, WlSurface>> {
        match self {
            Self::Wayland(s) => Some(Cow::Borrowed(s)),
            Self::X11(x) => x.wl_surface().map(Cow::Owned),
        }
    }
}

impl KeyboardTarget<State> for KeyboardFocus {
    fn enter(&self, seat: &Seat<State>, data: &mut State, keys: Vec<KeysymHandle<'_>>, serial: Serial) {
        match self {
            Self::Wayland(s) => KeyboardTarget::enter(s, seat, data, keys, serial),
            Self::X11(x) => KeyboardTarget::enter(x, seat, data, keys, serial),
        }
    }
    fn leave(&self, seat: &Seat<State>, data: &mut State, serial: Serial) {
        match self {
            Self::Wayland(s) => KeyboardTarget::leave(s, seat, data, serial),
            Self::X11(x) => KeyboardTarget::leave(x, seat, data, serial),
        }
    }
    fn key(&self, seat: &Seat<State>, data: &mut State, key: KeysymHandle<'_>, state: KeyState, serial: Serial, time: u32) {
        match self {
            Self::Wayland(s) => KeyboardTarget::key(s, seat, data, key, state, serial, time),
            Self::X11(x) => KeyboardTarget::key(x, seat, data, key, state, serial, time),
        }
    }
    fn modifiers(&self, seat: &Seat<State>, data: &mut State, modifiers: ModifiersState, serial: Serial) {
        match self {
            Self::Wayland(s) => KeyboardTarget::modifiers(s, seat, data, modifiers, serial),
            Self::X11(x) => KeyboardTarget::modifiers(x, seat, data, modifiers, serial),
        }
    }
}

impl State {
    pub fn window_for(&self, surface: &WlSurface) -> Option<Window> {
        self.space.elements().find(|w| w.wl_surface().is_some_and(|s| *s == *surface)).cloned()
    }

    /// The toplevels whose xdg parent chain (set_parent, or xdg-foreign) leads to this window, bottom to top.
    pub fn transients_of(&self, window: &Window) -> Vec<Window> {
        let mut roots: Vec<WlSurface> = window.wl_surface().map(|s| s.into_owned()).into_iter().collect();
        let mut found = Vec::new();
        while !roots.is_empty() {
            let next: Vec<Window> = self.space.elements().filter(|w| !found.contains(*w) && w.toplevel().and_then(|t| t.parent()).is_some_and(|p| roots.contains(&p))).cloned().collect();
            roots = next.iter().filter_map(|w| w.wl_surface().map(|s| s.into_owned())).collect();
            found.extend(next);
        }
        let order = |w: &Window| self.space.elements().position(|e| e == w);
        found.sort_by_key(order);
        found
    }

    /// Put a window with an xdg parent in the middle of that parent.
    fn center_on_parent(&mut self, window: &Window) {
        let Some(parent) = window.toplevel().and_then(|t| t.parent()).and_then(|p| self.window_for(&p)) else { return };
        let (Some(pg), true) = (self.space.element_geometry(&parent), self.space.element_location(window).is_some()) else { return }; // not a minimized child
        let size = window.geometry().size;
        let loc = self.clamp_to_output(window, pg.loc + Point::from(((pg.size.w - size.w) / 2, (pg.size.h - size.h) / 2)));
        self.space.map_element(window.clone(), loc, false);
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

    pub(crate) fn unconstrain_popup(&self, popup: &PopupSurface) {
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
        self.surfaces.push(surface.downgrade());
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
            let minimized = || self.minimized.iter().map(|(w, ..)| w).find(|w| w.wl_surface().is_some_and(|s| *s == root)).cloned();
            if let Some(window) = self.window_for(&root).or_else(minimized) {
                window.on_commit();
                self.touch_window(&window);
                let has_buffer = with_renderer_surface_state(&root, |s| s.buffer().is_some()).unwrap_or(false);
                // a dialog opens over its parent, not in the cascade
                if has_buffer && window.user_data().insert_if_missing(|| FirstBuffer) {
                    self.center_on_parent(&window);
                }
                // A new window takes the keyboard once it has something to show (its first buffer), so typing
                // goes to it without a click, unless a launcher holds an exclusive grab. Once per window.
                if self.active.as_ref() == Some(&window)
                    && has_buffer
                    && !self.exclusive_layer_focused()
                    && window.user_data().insert_if_missing(|| InitialFocus)
                {
                    self.focus_window(Some(&window), SERIAL_COUNTER.next_serial());
                }
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
        // a menu redrawing counts as its window updating (thumbnails)
        if let Some(window) = self.popups.find_popup(surface).and_then(|p| find_popup_root_surface(&p).ok()).and_then(|root| self.window_for(&root)) {
            self.touch_window(&window);
        }
        if matches!(&self.cursor_status, CursorImageStatus::Surface(s) if s == surface) {
            // GTK 4 changes the image on the same surface with wl_surface.offset; the hotspot moves with it
            let delta = with_states(surface, |s| s.cached_state.get::<SurfaceAttributes>().current().buffer_delta.take());
            if let Some(d) = delta.filter(|d| d.x != 0 || d.y != 0) {
                with_states(surface, |s| {
                    if let Some(data) = s.data_map.get::<CursorImageSurfaceData>() {
                        data.lock().unwrap().hotspot -= d;
                    }
                });
            }
            self.export_cursor(); // client redrew its cursor
            return;
        }
        ensure_initial_configure(surface, self);
        grabs::handle_commit(&mut self.space, surface);
        self.dirty = true;
    }
}

/// Marker: this window has had its one initial keyboard focus.
struct InitialFocus;
/// Marker: this window has shown its first buffer (placement happens once).
struct FirstBuffer;

impl State {
    /// A Top or Overlay layer surface with exclusive keyboard interactivity (a launcher) holds the keyboard.
    fn exclusive_layer_focused(&self) -> bool {
        let Some(focus) = self.seat.get_keyboard().and_then(|k| k.current_focus()) else { return false };
        let Some(focus) = focus.wl_surface() else { return false };
        let layers = layer_map_for_output(&self.output);
        layers
            .layer_for_surface(&focus, WindowSurfaceType::TOPLEVEL)
            .is_some_and(|l| l.cached_state().keyboard_interactivity == KeyboardInteractivity::Exclusive && matches!(l.layer(), Layer::Top | Layer::Overlay))
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
            keyboard.set_focus(state, Some(layer.wl_surface().clone().into()), SERIAL_COUNTER.next_serial());
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
            s.decoration_mode = Some(DecorationMode::ServerSide); // ours unless the client asks to draw its own
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
            let window = Window::new_wayland_window(surface);
            self.space.map_element(window.clone(), (0, 0), true);
            self.active = Some(window);
            return;
        }
        let n = self.space.elements().count() as i32 % 10;
        let window = Window::new_wayland_window(surface);
        // room for a title bar above (the client's answer on decorations comes with its first commit)
        self.space.map_element(window.clone(), work.loc + Point::from((40 + 30 * n, 40 + bw_core::decoration::BAR + 30 * n)), true);
        self.active = Some(window); // mapped activated: that is what the desktop API reports as focused
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
        let Ok(mut grab) = self.popups.grab_popup(root.into(), kind, &seat, serial) else { return };
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
/// Where (and how big) a window was before it filled the output.
type RestoreLocation = RefCell<Option<Rectangle<i32, Logical>>>;

impl State {
    /// Fullscreen covers the whole output, maximized only the work area.
    /// Where `window` goes fullscreen (the output) or maximized (the work area, under its title bar if we draw one).
    pub fn fill_rect(&self, window: &Window, fullscreen: bool) -> Rectangle<i32, Logical> {
        if fullscreen {
            return self.space.output_geometry(&self.output).unwrap_or_default();
        }
        let mut work = self.work_area();
        let bar = self.bar_height(window);
        work.loc.y += bar;
        work.size.h -= bar;
        work
    }

    pub(crate) fn fill_output(&mut self, surface: &ToplevelSurface, what: xdg_toplevel::State) {
        // fullscreen wins over maximized when both are set
        let fullscreen = what == xdg_toplevel::State::Fullscreen || surface.with_pending_state(|s| s.states.contains(xdg_toplevel::State::Fullscreen));
        let Some(window) = self.window_for(surface.wl_surface()) else { return };
        let geo = self.fill_rect(&window, fullscreen);
        window.user_data().insert_if_missing(RestoreLocation::default);
        let restore = window.user_data().get::<RestoreLocation>().unwrap();
        if restore.borrow().is_none() {
            *restore.borrow_mut() = self.space.element_location(&window).map(|loc| Rectangle::new(loc, window.geometry().size));
        }
        surface.with_pending_state(|s| {
            s.states.set(what);
            s.size = Some(geo.size);
        });
        self.space.map_element(window, geo.loc, false); // raise, but focus is focus_window's job
        surface.send_pending_configure();
    }
    pub(crate) fn unfill_output(&mut self, surface: &ToplevelSurface, what: xdg_toplevel::State) {
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
        // the size it had, said explicitly: a client that only ever takes what it is told keeps the big one otherwise
        let Some(window) = self.window_for(surface.wl_surface()) else { return };
        let saved = window.user_data().get::<RestoreLocation>().and_then(|r| r.borrow_mut().take());
        surface.with_pending_state(|s| s.size = saved.map(|r| r.size));
        if let Some(rect) = saved {
            let loc = self.clamp_to_output(&window, rect.loc);
            self.space.map_element(window, loc, false);
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
        // a launcher that held the keyboard is gone: the active window gets it back
        if self.seat.get_keyboard().and_then(|k| k.current_focus()).is_some_and(|f| f.wl_surface().as_deref() == Some(surface.wl_surface())) {
            let active = self.active.clone();
            self.focus_window(active.as_ref(), SERIAL_COUNTER.next_serial());
        }
    }
}
delegate_layer_shell!(State);

/// We draw the decorations unless the client wants to (GTK, Qt, browsers); what it asks for, it gets.
impl XdgDecorationHandler for State {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|s| s.decoration_mode = Some(DecorationMode::ServerSide));
    }
    fn request_mode(&mut self, toplevel: ToplevelSurface, mode: DecorationMode) {
        toplevel.with_pending_state(|s| s.decoration_mode = Some(mode));
        if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
        self.decorations_changed();
    }
    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        self.request_mode(toplevel, DecorationMode::ServerSide);
    }
}

impl SeatHandler for State {
    type KeyboardFocus = KeyboardFocus;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<State> {
        &mut self.seat_state
    }
    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        // A cursor surface is never in the space, so nothing else tells its client which output (and
        // scale) it is on; without this GTK uploads 1× cursors whatever the output scale.
        if let CursorImageStatus::Surface(old) = &self.cursor_status
            && !matches!(&image, CursorImageStatus::Surface(s) if s == old)
        {
            self.output.leave(old);
        }
        if let CursorImageStatus::Surface(s) = &image {
            self.output.enter(s);
            with_states(s, |states| with_fractional_scale(states, |f| f.set_preferred_scale(self.geometry.scale)));
        }
        self.cursor_status = image;
        self.export_cursor();
    }
    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&KeyboardFocus>) {
        let client = focused.and_then(|f| f.wl_surface()).and_then(|s| self.dh.get_client(s.id()).ok());
        set_data_device_focus(&self.dh, seat, client.clone());
        set_primary_focus(&self.dh, seat, client);
    }
}

impl SelectionHandler for State {
    type SelectionUserData = crate::clipboard::Selection;

    /// A Wayland client took the clipboard/primary selection: offer it to X11 clients too, and read a
    /// text clipboard for the browser and the API.
    fn new_selection(&mut self, ty: SelectionTarget, source: Option<SelectionSource>, _seat: Seat<Self>) {
        let mimes = source.map(|s| s.mime_types());
        if let Some(xwm) = self.xwm.as_mut()
            && let Err(e) = xwm.new_selection(ty, mimes.clone())
        {
            tracing::warn!("xwayland selection: {e:?}");
        }
        if ty == SelectionTarget::Clipboard
            && let Some(mime) = mimes.as_deref().and_then(crate::clipboard::pick_mime)
        {
            self.read_clipboard(mime, false);
        }
    }

    /// A Wayland client wants data from a compositor-owned selection: relayed from an X11 client, or our own.
    fn send_selection(&mut self, ty: SelectionTarget, mime_type: String, fd: OwnedFd, _seat: Seat<Self>, data: &crate::clipboard::Selection) {
        match data {
            crate::clipboard::Selection::Ours(ours) => self.serve_clipboard(ours.clone(), fd),
            crate::clipboard::Selection::X11 => {
                let handle = self.handle.clone();
                if let Some(xwm) = self.xwm.as_mut()
                    && let Err(e) = xwm.send_selection(ty, mime_type, fd, handle)
                {
                    tracing::warn!("xwayland selection transfer: {e:?}");
                }
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

impl OutputHandler for State {
    fn output_bound(&mut self, _output: smithay::output::Output, wl_output: WlOutput) {
        // a panel that binds wl_output after our globals still learns which output its workspace and windows are on
        self.workspaces.output_bound(&wl_output);
        self.foreign.output_bound(&wl_output);
    }
}

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

/// Clients (players, waybar's idle_inhibitor) can declare they inhibit idle. There is no screen to blank,
/// so the state is accepted and nothing else happens.
impl smithay::wayland::idle_inhibit::IdleInhibitHandler for State {
    fn inhibit(&mut self, _surface: WlSurface) {}
    fn uninhibit(&mut self, _surface: WlSurface) {}
}
smithay::delegate_idle_inhibit!(State);
smithay::delegate_presentation!(State);
// cursor-shape can also name a tablet tool's cursor; there are no tablets here.
impl smithay::wayland::tablet_manager::TabletSeatHandler for State {}
smithay::delegate_cursor_shape!(State);
smithay::delegate_single_pixel_buffer!(State);
smithay::delegate_alpha_modifier!(State);

/// xdg-activation: one user, so every token is good, and a request brings the window forward like a
/// click in the window list would.
impl smithay::wayland::xdg_activation::XdgActivationHandler for State {
    fn activation_state(&mut self) -> &mut smithay::wayland::xdg_activation::XdgActivationState {
        &mut self.xdg_activation_state
    }
    fn token_created(&mut self, _token: smithay::wayland::xdg_activation::XdgActivationToken, _data: smithay::wayland::xdg_activation::XdgActivationTokenData) -> bool {
        // tokens nobody used (a launched program that never asked) would otherwise pile up
        self.xdg_activation_state.retain_tokens(|_, d| d.timestamp.elapsed() < std::time::Duration::from_secs(60));
        true
    }
    fn request_activation(&mut self, token: smithay::wayland::xdg_activation::XdgActivationToken, _data: smithay::wayland::xdg_activation::XdgActivationTokenData, surface: WlSurface) {
        self.xdg_activation_state.remove_token(&token);
        let minimized = self.minimized.iter().map(|(w, ..)| w).find(|w| w.wl_surface().is_some_and(|s| *s == surface)).cloned();
        if let Some(window) = self.window_for(&surface).or(minimized) {
            self.unminimize(&window);
            self.focus_window(Some(&window), SERIAL_COUNTER.next_serial());
        }
    }
}
smithay::delegate_xdg_activation!(State);

/// xdg-foreign sets one client's toplevel as the parent of another's (portal dialogs); Smithay records
/// it as the xdg parent, which placement and raising then follow.
impl smithay::wayland::xdg_foreign::XdgForeignHandler for State {
    fn xdg_foreign_state(&mut self) -> &mut smithay::wayland::xdg_foreign::XdgForeignState {
        &mut self.xdg_foreign_state
    }
}
smithay::delegate_xdg_foreign!(State);

/// The icon name lands in the surface's cached state, read with the window list.
impl smithay::wayland::xdg_toplevel_icon::XdgToplevelIconHandler for State {}
smithay::delegate_xdg_toplevel_icon!(State);

// Frame pacing: barriers and timers are released from the frame clock (State::release_barriers);
// the content type is read with the window list.
smithay::delegate_fifo!(State);
smithay::delegate_commit_timing!(State);
smithay::delegate_content_type!(State);
