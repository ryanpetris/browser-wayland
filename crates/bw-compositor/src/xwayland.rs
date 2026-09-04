//! Rootless Xwayland: X11 windows become ordinary windows in the space, and we are their window manager.

use std::{os::fd::OwnedFd, process::Stdio};

use smithay::{
    delegate_xwayland_shell,
    desktop::Window,
    input::pointer::Focus,
    reexports::wayland_protocols::xdg::shell::server::xdg_toplevel,
    utils::{Logical, Rectangle},
    wayland::{
        selection::{
            SelectionTarget,
            data_device::{
                clear_data_device_selection, current_data_device_selection_userdata, request_data_device_client_selection,
                set_data_device_selection,
            },
            primary_selection::{
                clear_primary_selection, current_primary_selection_userdata, request_primary_client_selection, set_primary_selection,
            },
        },
        xwayland_shell::{XWaylandShellHandler, XWaylandShellState},
    },
    xwayland::{
        X11Surface, X11Wm, XWayland, XWaylandEvent, XwmHandler,
        xwm::{Reorder, ResizeEdge, XwmId},
    },
};

use crate::{State, grabs};

/// Geometry to go back to after maximize/fullscreen.
type Restore = std::cell::RefCell<Option<Rectangle<i32, Logical>>>;

impl State {
    /// Launch Xwayland; `x11_display` is set once it is ready and the window manager is attached.
    pub fn start_xwayland(&mut self) {
        // No abstract socket: it has no permissions at all, and X clients get the whole display.
        let (xwayland, client) = match XWayland::spawn(&self.dh, None, std::iter::empty::<(String, String)>(), false, Stdio::null(), Stdio::null(), |_| {}) {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!("no Xwayland ({e}); X11 clients won't work");
                return;
            }
        };
        let handle = self.handle.clone();
        let res = self.handle.insert_source(xwayland, move |event, _, state| {
            state.xwayland_pending = false;
            match event {
                XWaylandEvent::Ready { x11_socket, display_number } => match X11Wm::start_wm(handle.clone(), x11_socket, client.clone()) {
                    Ok(wm) => {
                        // Only this user may connect to the display.
                        let sock = format!("/tmp/.X11-unix/X{display_number}");
                        if let Err(e) = std::fs::set_permissions(&sock, std::os::unix::fs::PermissionsExt::from_mode(0o600)) {
                            tracing::warn!("chmod {sock}: {e}");
                        }
                        tracing::info!(display = display_number, "xwayland ready");
                        state.xwm = Some(wm);
                        state.x11_display = Some(display_number);
                    }
                    Err(e) => tracing::warn!("xwayland window manager failed: {e}"),
                },
                XWaylandEvent::Error => tracing::warn!("xwayland failed to start"),
            }
        });
        match res {
            Ok(_) => self.xwayland_pending = true,
            Err(e) => tracing::warn!("xwayland event source: {e}"),
        }
    }

    fn window_for_x11(&self, surface: &X11Surface) -> Option<Window> {
        self.space.elements().find(|w| w.x11_surface() == Some(surface)).cloned()
    }

    /// Move/resize an X11 window and tell X about it (so its own coordinates stay right).
    fn place_x11(&mut self, window: &Window, surface: &X11Surface, rect: Rectangle<i32, Logical>) {
        self.space.map_element(window.clone(), rect.loc, false);
        let _ = surface.configure(rect);
        self.dirty = true;
    }
}

impl XwmHandler for State {
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        self.xwm.as_mut().unwrap()
    }
    fn new_window(&mut self, _xwm: XwmId, _window: X11Surface) {}
    fn new_override_redirect_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn map_window_request(&mut self, _xwm: XwmId, window: X11Surface) {
        let _ = window.set_mapped(true);
        let win = Window::new_x11_window(window.clone());
        if self.kiosk {
            let _ = window.set_fullscreen(true);
            let geo = self.space.output_geometry(&self.output).unwrap_or_default();
            self.place_x11(&win, &window, geo);
        } else {
            let n = self.space.elements().count() as i32 % 10;
            let mut rect = window.geometry();
            rect.loc = self.work_area().loc + smithay::utils::Point::from((40 + 30 * n, 40 + 30 * n));
            self.place_x11(&win, &window, rect);
        }
        win.set_activated(true);
        self.active = Some(win); // mapped activated: that is what the desktop API reports as focused
    }

    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
        // menus, tooltips: they know where they want to be
        let loc = window.geometry().loc;
        self.space.map_element(Window::new_x11_window(window), loc, false);
        self.dirty = true;
    }

    fn unmapped_window(&mut self, _xwm: XwmId, window: X11Surface) {
        if let Some(win) = self.window_for_x11(&window) {
            self.space.unmap_elem(&win);
        }
        self.minimized.retain(|(w, ..)| w.x11_surface() != Some(&window));
        self.dirty = true;
    }

    fn destroyed_window(&mut self, _xwm: XwmId, _window: X11Surface) {
        self.dirty = true;
    }

    fn configure_request(&mut self, _xwm: XwmId, window: X11Surface, _x: Option<i32>, _y: Option<i32>, w: Option<u32>, h: Option<u32>, _reorder: Option<Reorder>) {
        // clients may pick their size, not their position
        let mut geo = window.geometry();
        if let Some(w) = w {
            geo.size.w = w as i32;
        }
        if let Some(h) = h {
            geo.size.h = h as i32;
        }
        let _ = window.configure(geo);
    }

    fn configure_notify(&mut self, _xwm: XwmId, window: X11Surface, geometry: Rectangle<i32, Logical>, _above: Option<u32>) {
        if let Some(win) = self.window_for_x11(&window) {
            // map_element puts the window on top, so only remap when it actually moved
            if self.space.element_location(&win) != Some(geometry.loc) {
                self.space.map_element(win, geometry.loc, false);
            }
            self.dirty = true;
        }
    }

    /// Xwayland died after startup: its windows are gone, drop them from the space.
    /// `xwm` stays set: Smithay's shell hooks and in-flight selection transfers still call `xwm_state`.
    fn disconnected(&mut self, _xwm: XwmId) {
        tracing::warn!("xwayland exited");
        for w in self.space.elements().filter(|w| w.x11_surface().is_some()).cloned().collect::<Vec<_>>() {
            self.space.unmap_elem(&w);
        }
        self.minimized.retain(|(w, ..)| w.x11_surface().is_none());
        self.x11_display = None;
        self.dirty = true;
    }

    fn minimize_request(&mut self, _xwm: XwmId, window: X11Surface) {
        if let Some(win) = self.window_for_x11(&window) {
            self.minimize(&win);
        }
    }
    fn unminimize_request(&mut self, _xwm: XwmId, window: X11Surface) {
        if let Some(win) = self.minimized.iter().map(|(w, ..)| w).find(|w| w.x11_surface() == Some(&window)).cloned() {
            self.unminimize(&win);
        }
    }
    fn maximize_request(&mut self, _xwm: XwmId, window: X11Surface) {
        self.fill_x11(window, |w| w.set_maximized(true));
    }
    fn unmaximize_request(&mut self, _xwm: XwmId, window: X11Surface) {
        self.unfill_x11(window, |w| w.set_maximized(false));
    }
    fn fullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        self.fill_x11(window, |w| w.set_fullscreen(true));
    }
    fn unfullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        self.unfill_x11(window, |w| w.set_fullscreen(false));
    }

    fn resize_request(&mut self, _xwm: XwmId, window: X11Surface, _button: u32, edges: ResizeEdge) {
        let pointer = self.seat.get_pointer().unwrap();
        let (Some(start_data), Some(win)) = (self.x11_grab_start(&window), self.window_for_x11(&window)) else { return };
        let mut initial_rect = win.geometry();
        initial_rect.loc = self.space.element_location(&win).unwrap();
        let edges = match edges {
            ResizeEdge::Top => xdg_toplevel::ResizeEdge::Top,
            ResizeEdge::Bottom => xdg_toplevel::ResizeEdge::Bottom,
            ResizeEdge::Left => xdg_toplevel::ResizeEdge::Left,
            ResizeEdge::TopLeft => xdg_toplevel::ResizeEdge::TopLeft,
            ResizeEdge::BottomLeft => xdg_toplevel::ResizeEdge::BottomLeft,
            ResizeEdge::Right => xdg_toplevel::ResizeEdge::Right,
            ResizeEdge::TopRight => xdg_toplevel::ResizeEdge::TopRight,
            ResizeEdge::BottomRight => xdg_toplevel::ResizeEdge::BottomRight,
        };
        let grab = grabs::ResizeGrab { start_data, window: win, edges, initial_rect, last_size: initial_rect.size };
        pointer.set_grab(self, grab, smithay::utils::SERIAL_COUNTER.next_serial(), Focus::Clear);
    }

    // --- clipboard / primary selection, X11 side ---

    /// X11 clients may read a Wayland-owned selection only while an X11 window has keyboard focus.
    fn allow_selection_access(&mut self, _xwm: XwmId, _selection: SelectionTarget) -> bool {
        let Some(keyboard) = self.seat.get_keyboard() else { return false };
        keyboard.current_focus().and_then(|s| self.window_for(&s)).is_some_and(|w| w.x11_surface().is_some())
    }

    /// An X11 client wants data from a selection owned by a Wayland client.
    fn send_selection(&mut self, _xwm: XwmId, selection: SelectionTarget, mime_type: String, fd: OwnedFd) {
        let res = match selection {
            SelectionTarget::Clipboard => request_data_device_client_selection(&self.seat, mime_type, fd).map_err(|e| format!("{e:?}")),
            SelectionTarget::Primary => request_primary_client_selection(&self.seat, mime_type, fd).map_err(|e| format!("{e:?}")),
        };
        if let Err(e) = res {
            tracing::warn!("selection transfer to X11: {e}");
        }
    }

    /// An X11 client took a selection: offer it to Wayland clients.
    fn new_selection(&mut self, _xwm: XwmId, selection: SelectionTarget, mime_types: Vec<String>) {
        match selection {
            SelectionTarget::Clipboard => set_data_device_selection(&self.dh, &self.seat, mime_types, ()),
            SelectionTarget::Primary => set_primary_selection(&self.dh, &self.seat, mime_types, ()),
        }
    }

    fn cleared_selection(&mut self, _xwm: XwmId, selection: SelectionTarget) {
        match selection {
            SelectionTarget::Clipboard => {
                if current_data_device_selection_userdata(&self.seat).is_some() {
                    clear_data_device_selection(&self.dh, &self.seat);
                }
            }
            SelectionTarget::Primary => {
                if current_primary_selection_userdata(&self.seat).is_some() {
                    clear_primary_selection(&self.dh, &self.seat);
                }
            }
        }
    }

    fn move_request(&mut self, _xwm: XwmId, window: X11Surface, _button: u32) {
        let pointer = self.seat.get_pointer().unwrap();
        let (Some(start_data), Some(win)) = (self.x11_grab_start(&window), self.window_for_x11(&window)) else { return };
        let initial_location = self.space.element_location(&win).unwrap();
        let grab = grabs::MoveGrab { start_data, window: win, initial_location };
        pointer.set_grab(self, grab, smithay::utils::SERIAL_COUNTER.next_serial(), Focus::Clear);
    }
}

impl State {
    /// The pointer grab a move/resize request may take over: a left press on that very window.
    fn x11_grab_start(&self, window: &X11Surface) -> Option<smithay::input::pointer::GrabStartData<State>> {
        if window.is_override_redirect() || window.is_maximized() || window.is_fullscreen() {
            return None;
        }
        let start = self.seat.get_pointer()?.grab_start_data()?;
        (start.button == 0x110 && start.focus.as_ref().map(|(s, _)| s.clone()) == window.wl_surface()).then_some(start)
    }

    pub(crate) fn fill_x11(&mut self, window: X11Surface, set: impl Fn(&X11Surface) -> Result<(), smithay::reexports::x11rb::rust_connection::ConnectionError>) {
        let Some(win) = self.window_for_x11(&window) else { return };
        win.user_data().insert_if_missing(Restore::default);
        let restore = win.user_data().get::<Restore>().unwrap();
        if restore.borrow().is_none() {
            let mut r = win.geometry();
            r.loc = self.space.element_location(&win).unwrap_or_default();
            *restore.borrow_mut() = Some(r);
        }
        let _ = set(&window);
        let geo = self.fill_rect(window.is_fullscreen()); // fullscreen wins over maximized when both are set
        self.place_x11(&win, &window, geo);
    }

    pub(crate) fn unfill_x11(&mut self, window: X11Surface, set: impl Fn(&X11Surface) -> Result<(), smithay::reexports::x11rb::rust_connection::ConnectionError>) {
        let _ = set(&window);
        let Some(win) = self.window_for_x11(&window) else { return };
        if window.is_maximized() || window.is_fullscreen() {
            let geo = self.fill_rect(window.is_fullscreen());
            return self.place_x11(&win, &window, geo); // still filled the other way: re-fit to that rect
        }
        let saved = win.user_data().get::<Restore>().and_then(|r| r.borrow_mut().take());
        if let Some(mut rect) = saved {
            rect.loc = self.clamp_to_output(rect.loc); // the output may have shrunk meanwhile
            self.place_x11(&win, &window, rect);
        }
    }
}

impl XWaylandShellHandler for State {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell_state
    }
}
delegate_xwayland_shell!(State);
