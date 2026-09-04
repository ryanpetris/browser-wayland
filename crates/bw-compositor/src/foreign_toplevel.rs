//! wlr-foreign-toplevel-management: the window list for taskbars (waybar, xfce4-panel).
//! Each taskbar binds a manager; every window gets one handle per manager.

use std::collections::HashMap;

use smithay::{
    reexports::wayland_server::protocol::wl_output::WlOutput,
    desktop::{Window, WindowSurface},
    output::Output,
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel::State as XdgState,
        wayland_protocols_wlr::foreign_toplevel::v1::server::{
            zwlr_foreign_toplevel_handle_v1::{self as handle, ZwlrForeignToplevelHandleV1},
            zwlr_foreign_toplevel_manager_v1::{self as manager, ZwlrForeignToplevelManagerV1},
        },
        wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, backend::ClientId},
    },
    utils::SERIAL_COUNTER,
};

use crate::State;

/// v3 adds the `parent` event; we don't track window parents, so stay at 2.
pub const VERSION: u32 = 2;

#[derive(Default)]
pub struct ForeignToplevels {
    managers: Vec<ZwlrForeignToplevelManagerV1>,
    windows: HashMap<Window, Entry>,
}

/// What the taskbars know about a window; diffed against the current state every loop iteration.
#[derive(Default, PartialEq)]
struct Info {
    title: String,
    app_id: String,
    /// `handle::State` values as the protocol's u32 array
    states: Vec<u8>,
}

struct Entry {
    info: Info,
    handles: Vec<ZwlrForeignToplevelHandleV1>,
}

fn new_handle(dh: &DisplayHandle, output: &Output, manager: &ZwlrForeignToplevelManagerV1, window: &Window, info: &Info) -> Option<ZwlrForeignToplevelHandleV1> {
    let client = manager.client()?;
    let h = client.create_resource::<ZwlrForeignToplevelHandleV1, Window, State>(dh, manager.version(), window.clone()).ok()?;
    manager.toplevel(&h);
    h.title(info.title.clone());
    h.app_id(info.app_id.clone());
    for o in output.client_outputs(&client) {
        h.output_enter(&o);
    }
    h.state(info.states.clone());
    h.done();
    Some(h)
}

impl ForeignToplevels {
    /// A client bound `wl_output` after its handles were made: tell it the windows are on that output.
    pub fn output_bound(&self, wl_output: &WlOutput) {
        for h in self.windows.values().flat_map(|e| &e.handles).filter(|h| h.client().is_some_and(|c| Some(c) == wl_output.client())) {
            h.output_enter(wl_output);
            h.done();
        }
    }
}

impl State {
    fn toplevel_info(&self, window: &Window) -> Info {
        let (maximized, fullscreen, activated) = match window.underlying_surface() {
            WindowSurface::Wayland(t) => {
                let st = t.current_state().states;
                (st.contains(XdgState::Maximized), st.contains(XdgState::Fullscreen), st.contains(XdgState::Activated))
            }
            WindowSurface::X11(x) => (x.is_maximized(), x.is_fullscreen(), x.is_activated()),
        };
        let minimized = self.minimized.iter().any(|(w, ..)| w == window);
        let states = [
            (maximized, handle::State::Maximized),
            (fullscreen, handle::State::Fullscreen),
            (activated, handle::State::Activated),
            (minimized, handle::State::Minimized),
        ]
        .into_iter()
        .filter(|(on, _)| *on)
        .flat_map(|(_, s)| (s as u32).to_ne_bytes())
        .collect();
        let (title, app_id) = State::title_app_id(window);
        Info { title, app_id, states }
    }

    /// Tell the taskbars what changed. Called once per loop iteration.
    /// ponytail: a full diff over a handful of windows; gate on `dirty` if it ever shows up in a profile.
    pub fn refresh_foreign_toplevels(&mut self) {
        let live: Vec<Window> = self
            .space
            .elements()
            .filter(|w| w.x11_surface().is_none_or(|x| !x.is_override_redirect())) // menus and tooltips aren't windows
            .chain(self.minimized.iter().map(|(w, ..)| w))
            .cloned()
            .collect();
        let gone: Vec<Window> = self.foreign.windows.keys().filter(|w| !live.contains(w)).cloned().collect();
        for w in gone {
            for h in self.foreign.windows.remove(&w).unwrap().handles {
                h.closed();
            }
        }
        for window in live {
            let info = self.toplevel_info(&window);
            match self.foreign.windows.get_mut(&window) {
                None => {
                    let handles = self.foreign.managers.iter().filter_map(|m| new_handle(&self.dh, &self.output, m, &window, &info)).collect();
                    self.foreign.windows.insert(window, Entry { info, handles });
                }
                Some(entry) => {
                    entry.handles.retain(Resource::is_alive); // taskbars come and go
                    if entry.info == info {
                        continue;
                    }
                    for h in &entry.handles {
                        if entry.info.title != info.title {
                            h.title(info.title.clone());
                        }
                        if entry.info.app_id != info.app_id {
                            h.app_id(info.app_id.clone());
                        }
                        if entry.info.states != info.states {
                            h.state(info.states.clone());
                        }
                        h.done();
                    }
                    entry.info = info;
                }
                Some(_) => {}
            }
        }
    }

    /// Maximize/fullscreen (or undo it) from a taskbar, through the same paths the clients' own requests take.
    pub(crate) fn fill(&mut self, window: &Window, what: XdgState, set: bool) {
        let fullscreen = what == XdgState::Fullscreen;
        self.unminimize(window); // the fill paths only know mapped windows; a taskbar (un)maximizing one shows it anyway
        match window.underlying_surface() {
            WindowSurface::Wayland(t) if set => self.fill_output(t, what),
            WindowSurface::Wayland(t) => self.unfill_output(t, what),
            WindowSurface::X11(x) if set => self.fill_x11(x.clone(), |w| if fullscreen { w.set_fullscreen(true) } else { w.set_maximized(true) }),
            WindowSurface::X11(x) => self.unfill_x11(x.clone(), |w| if fullscreen { w.set_fullscreen(false) } else { w.set_maximized(false) }),
        }
    }
}

impl GlobalDispatch<ZwlrForeignToplevelManagerV1, ()> for State {
    fn bind(state: &mut Self, dh: &DisplayHandle, _client: &Client, resource: New<ZwlrForeignToplevelManagerV1>, _: &(), data_init: &mut DataInit<'_, Self>) {
        let manager = data_init.init(resource, ());
        for (window, entry) in state.foreign.windows.iter_mut() {
            entry.handles.extend(new_handle(dh, &state.output, &manager, window, &entry.info));
        }
        state.foreign.managers.push(manager);
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for State {
    fn request(state: &mut Self, _: &Client, manager: &ZwlrForeignToplevelManagerV1, request: manager::Request, _: &(), _: &DisplayHandle, _: &mut DataInit<'_, Self>) {
        if let manager::Request::Stop = request {
            manager.finished();
            state.foreign.managers.retain(|m| m != manager);
        }
    }
    fn destroyed(state: &mut Self, _: ClientId, manager: &ZwlrForeignToplevelManagerV1, _: &()) {
        state.foreign.managers.retain(|m| m != manager);
    }
}

impl Dispatch<ZwlrForeignToplevelHandleV1, Window> for State {
    fn request(state: &mut Self, _: &Client, _: &ZwlrForeignToplevelHandleV1, request: handle::Request, window: &Window, _: &DisplayHandle, _: &mut DataInit<'_, Self>) {
        use handle::Request as R;
        if !state.foreign.windows.contains_key(window) {
            return; // already `closed`: the protocol says the handle is inert
        }
        match request {
            R::Activate { .. } => {
                state.unminimize(window);
                state.focus_window(Some(window), SERIAL_COUNTER.next_serial());
            }
            R::Close => match window.underlying_surface() {
                WindowSurface::Wayland(t) => t.send_close(),
                WindowSurface::X11(x) => {
                    let _ = x.close();
                }
            },
            R::SetMaximized => state.fill(window, XdgState::Maximized, true),
            R::UnsetMaximized => state.fill(window, XdgState::Maximized, false),
            R::SetFullscreen { .. } => state.fill(window, XdgState::Fullscreen, true),
            R::UnsetFullscreen => state.fill(window, XdgState::Fullscreen, false),
            R::SetMinimized => state.minimize(window),
            R::UnsetMinimized => state.unminimize(window),
            R::SetRectangle { .. } | R::Destroy => {} // the rectangle is a minimize-animation hint
            _ => {}
        }
    }
}
