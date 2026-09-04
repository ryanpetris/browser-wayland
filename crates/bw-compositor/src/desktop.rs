//! The desktop API: the window list the viewer and `/api` see, and the control requests they send.

use std::{
    cell::Cell,
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context, Result};
use bw_core::{ControlMsg, ControlOp, Event, Snapshot, SnapshotError, WindowInfo};
use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            Bind, ExportMem, Offscreen, TextureMapping,
            damage::OutputDamageTracker,
            element::{AsRenderElements, RenderElement, surface::WaylandSurfaceRenderElement},
            gles::{GlesRenderer, GlesTexture},
        },
    },
    desktop::{Window, WindowSurface, PopupManager},
    reexports::{wayland_protocols::xdg::shell::server::xdg_toplevel::State as XdgState, wayland_server::Resource},
    utils::{Buffer, Physical, Rectangle, SERIAL_COUNTER, Scale, Size, Transform},
    wayland::{compositor::with_states, shell::xdg::XdgToplevelSurfaceData},
};

use crate::State;

struct WindowId(u64);
/// Last commit, ms on the compositor clock.
struct LastCommit(Cell<u64>);

/// Stable for the window's life, never reused.
pub fn window_id(window: &Window) -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    window.user_data().insert_if_missing(|| WindowId(NEXT.fetch_add(1, Ordering::Relaxed)));
    window.user_data().get::<WindowId>().unwrap().0
}

impl State {
    /// Whole seconds: a window redrawing at 60 fps must not mean 60 window lists a second.
    pub fn touch_window(&self, window: &Window) {
        window.user_data().insert_if_missing(|| LastCommit(Cell::new(0)));
        window.user_data().get::<LastCommit>().unwrap().0.set(self.clock.now().as_micros() / 1_000_000 * 1000);
    }

    pub fn title_app_id(window: &Window) -> (String, String) {
        match window.underlying_surface() {
            WindowSurface::Wayland(t) => with_states(t.wl_surface(), |s| {
                let d = s.data_map.get::<XdgToplevelSurfaceData>().unwrap().lock().unwrap();
                (d.title.clone().unwrap_or_default(), d.app_id.clone().unwrap_or_default())
            }),
            WindowSurface::X11(x) => (x.title(), x.class()),
        }
    }

    fn window_info(&self, window: &Window, z: Option<u32>) -> WindowInfo {
        let (title, app_id) = Self::title_app_id(window);
        let geo = self.space.element_geometry(window).unwrap_or_else(|| {
            // minimized: where it comes back
            let loc = self.minimized.iter().find(|(w, ..)| w == window).map(|(_, l, _)| *l).unwrap_or_default();
            Rectangle::new(loc, window.geometry().size)
        });
        let mut popups = Vec::new();
        let (x11, pid, maximized, fullscreen) = match window.underlying_surface() {
            WindowSurface::Wayland(t) => {
                let st = t.current_state().states;
                let pid = t.wl_surface().client().and_then(|c| c.get_credentials(&self.dh).ok()).map(|c| c.pid as u32);
                // a popup's location is relative to the parent's geometry, like our x/y
                popups.extend(PopupManager::popups_for_surface(t.wl_surface()).map(|(p, loc)| {
                    let g = p.geometry();
                    (loc.x, loc.y, g.size.w, g.size.h)
                }));
                (false, pid, st.contains(XdgState::Maximized), st.contains(XdgState::Fullscreen))
            }
            WindowSurface::X11(x) => (true, x.pid(), x.is_maximized(), x.is_fullscreen()),
        };
        WindowInfo {
            id: window_id(window),
            title,
            app_id,
            x11,
            pid,
            x: geo.loc.x,
            y: geo.loc.y,
            w: geo.size.w,
            h: geo.size.h,
            geo_x: window.geometry().loc.x,
            geo_y: window.geometry().loc.y,
            popups,
            z,
            maximized,
            fullscreen,
            minimized: z.is_none(),
            focused: self.active.as_ref() == Some(window),
            updated_ms: window.user_data().get::<LastCommit>().map_or(0, |c| c.0.get()),
        }
    }

    /// Every window bottom to top, then the minimized ones. Menus and tooltips (X11 override-redirect) aren't windows.
    pub fn windows(&self) -> Vec<WindowInfo> {
        let mut list: Vec<WindowInfo> = self
            .space
            .elements()
            .filter(|w| w.x11_surface().is_none_or(|x| !x.is_override_redirect()))
            .enumerate()
            .map(|(z, w)| self.window_info(w, Some(z as u32)))
            .collect();
        list.extend(self.minimized.iter().map(|(w, ..)| self.window_info(w, None)));
        list
    }

    /// Send the list to the viewer when it changed. Called once per loop iteration.
    pub fn refresh_windows(&mut self) {
        let list = self.windows();
        if list != self.last_windows {
            let _ = self.events.send(Event::Windows(list.clone()));
            self.last_windows = list;
        }
    }

    pub fn window_by_id(&self, id: u64) -> Option<Window> {
        self.space
            .elements()
            .chain(self.minimized.iter().map(|(w, ..)| w))
            .find(|w| w.user_data().get::<WindowId>().is_some_and(|i| i.0 == id))
            .cloned()
    }

    /// A request from the viewer page or `/api/control`. Unknown ids and impossible requests are ignored.
    pub fn control(&mut self, msg: ControlMsg) {
        if let ControlOp::Spawn { cmd } = &msg.op {
            return self.spawn_client(cmd, self.geometry);
        }
        let Some(window) = self.window_by_id(msg.id) else { return };
        let info = self.window_info(&window, None);
        let floating = self.space.element_location(&window).is_some() && !info.maximized && !info.fullscreen;
        match msg.op {
            ControlOp::Activate => {
                self.unminimize(&window);
                self.focus_window(Some(&window), SERIAL_COUNTER.next_serial());
            }
            ControlOp::Close => match window.underlying_surface() {
                WindowSurface::Wayland(t) => t.send_close(),
                WindowSurface::X11(x) => {
                    let _ = x.close();
                }
            },
            ControlOp::Minimize => self.minimize(&window),
            ControlOp::Unminimize => self.unminimize(&window),
            ControlOp::Maximize => self.fill(&window, XdgState::Maximized, true),
            ControlOp::Unmaximize => self.fill(&window, XdgState::Maximized, false),
            ControlOp::Fullscreen => self.fill(&window, XdgState::Fullscreen, true),
            ControlOp::Unfullscreen => self.fill(&window, XdgState::Fullscreen, false),
            ControlOp::Move { x, y } if floating => {
                self.space.map_element(window.clone(), (x, y), false);
                if let WindowSurface::X11(x11) = window.underlying_surface() {
                    let _ = x11.configure(Rectangle::new((x, y).into(), window.geometry().size));
                }
            }
            ControlOp::Resize { w, h } if floating => {
                let size = (w.max(1), h.max(1)).into();
                match window.underlying_surface() {
                    WindowSurface::Wayland(t) => {
                        t.with_pending_state(|s| s.size = Some(size));
                        t.send_pending_configure();
                    }
                    WindowSurface::X11(x11) => {
                        let loc = self.space.element_location(&window).unwrap_or_default();
                        let _ = x11.configure(Rectangle::new(loc, size));
                    }
                }
            }
            ControlOp::Move { .. } | ControlOp::Resize { .. } | ControlOp::Spawn { .. } => {}
        }
        self.dirty = true;
    }

    /// One window (its xdg geometry, popups included, transparent where it doesn't paint) or the whole
    /// output, at `scale` × the output scale. Renders offscreen; the stream is untouched.
    pub fn snapshot(&mut self, id: Option<u64>, scale: f64) -> Result<Snapshot, SnapshotError> {
        let result = match id {
            Some(id) => {
                let window = self.window_by_id(id).ok_or(SnapshotError::NoSuchWindow)?;
                let scale = scale * self.geometry.scale;
                let geo = window.geometry();
                let size = geo.size.to_f64().to_physical(scale).to_i32_round();
                let loc = smithay::utils::Point::<i32, smithay::utils::Logical>::from((-geo.loc.x, -geo.loc.y)).to_f64().to_physical(scale).to_i32_round();
                let elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = window.render_elements(&mut self.gpu.renderer, loc, Scale::from(scale), 1.0);
                readback(&mut self.gpu.renderer, &elements, size, scale, [0.0; 4])
            }
            None => {
                // the mode is the stream's size, so scale 1 gives exactly what the viewer sees
                let mode = self.output.current_mode().map(|m| m.size).unwrap_or_default();
                let size = Size::from(((mode.w as f64 * scale).round() as i32, (mode.h as f64 * scale).round() as i32));
                let elements = self.output_elements(scale * self.geometry.scale);
                readback(&mut self.gpu.renderer, &elements, size, scale * self.geometry.scale, crate::render::CLEAR)
            }
        };
        result.map_err(|e| {
            tracing::warn!(?id, "snapshot failed: {e:#}");
            SnapshotError::Render(format!("{e:#}"))
        })
    }
}

fn readback<E: RenderElement<GlesRenderer>>(renderer: &mut GlesRenderer, elements: &[E], size: Size<i32, Physical>, scale: f64, clear: [f32; 4]) -> Result<Snapshot> {
    // 64 Mpx is 256 MiB of RGBA before the PNG; anything bigger is a mistake or an attack
    anyhow::ensure!(size.w > 0 && size.h > 0 && (size.w as u64) * (size.h as u64) <= 64 << 20, "size {}x{} out of range", size.w, size.h);
    let mut texture: GlesTexture = renderer.create_buffer(Fourcc::Abgr8888, Size::<i32, Buffer>::from((size.w, size.h))).context("create texture")?;
    let mut fb = renderer.bind(&mut texture).context("bind texture")?;
    OutputDamageTracker::new(size, scale, Transform::Normal).render_output(renderer, &mut fb, 0, elements, clear).context("render")?;
    let mapping = renderer.copy_framebuffer(&fb, Rectangle::from_size(Size::from((size.w, size.h))), Fourcc::Abgr8888).context("copy framebuffer")?;
    let data = renderer.map_texture(&mapping).context("map texture")?;
    let (w, h) = (size.w as usize, size.h as usize);
    let stride = w * 4;
    let mut rgba = vec![0u8; stride * h];
    // `flipped()` means row 0 is the top (GLES renders that way); otherwise GL's rows start at the bottom
    for y in 0..h {
        let src = if mapping.flipped() { y } else { h - 1 - y };
        rgba[y * stride..(y + 1) * stride].copy_from_slice(&data[src * stride..(src + 1) * stride]);
    }
    // GL gives premultiplied alpha; PNG wants straight
    for px in rgba.chunks_exact_mut(4) {
        let a = px[3] as u32;
        if a != 0 && a != 255 {
            for c in &mut px[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    Ok(Snapshot { width: size.w as u32, height: size.h as u32, rgba })
}
