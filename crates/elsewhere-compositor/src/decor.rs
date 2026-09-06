//! Server-side decorations: a title bar with the title and close, maximize and minimize buttons above
//! windows that don't draw their own (see `elsewhere_core::decoration`). The bar is compositor chrome above
//! the window's geometry: the API, elements, snapshots and window streams keep meaning the client's
//! area. Drawn on the CPU into a bitmap at the output's resolution, once per change of title, focus,
//! state or width; dragged, resized (a band around the frame) and double-clicked like any title bar.

use std::cell::RefCell;

use ab_glyph::{Font, FontVec, PxScale, ScaleFont};
use smithay::{
    reexports::{
        wayland_protocols_misc::server_decoration::server::org_kde_kwin_server_decoration::{Mode as KdeMode, OrgKdeKwinServerDecoration},
        wayland_server::{WEnum, protocol::wl_surface::WlSurface},
    },
    wayland::{
        compositor::with_states,
        shell::kde::decoration::{KdeDecorationHandler, KdeDecorationState},
    },
};
use elsewhere_core::decoration::{BAR, BUTTON, Button, buttons};
use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            element::{
                Kind,
                memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
            },
            gles::GlesRenderer,
        },
    },
    desktop::{Window, WindowSurface, WindowSurfaceType},
    input::pointer::CursorIcon,
    reexports::wayland_protocols::xdg::{
        decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode,
        shell::server::xdg_toplevel::{ResizeEdge, State as XdgState},
    },
    utils::{Buffer, Logical, Point, Rectangle, Size, Transform},
};

use crate::State;

/// The invisible resize band around a decorated window, in logical px.
const EDGE: i32 = 6;

/// What the pointer is over, of a window's decorations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    Bar,
    Button(Button),
    Edge(ResizeEdge),
}

/// What a window walk found under the pointer.
pub enum Under {
    Surface(WlSurface, Point<f64, Logical>),
    Decoration(Window, Hit),
}

/// The bar's bitmap and what it was drawn for: title, focused, maximized, size in pixels.
#[derive(Default)]
struct Cache(RefCell<Option<(String, bool, bool, Size<i32, Buffer>, MemoryRenderBuffer)>>);

/// What a surface asked for through KDE's server-decoration protocol, the one GTK (3 and 4) and
/// Firefox use to say they draw their own; xdg-decoration is the other way to say it.
#[derive(Default)]
struct KdeRequest(RefCell<Option<KdeMode>>);

fn kde_client_side(surface: &WlSurface) -> bool {
    with_states(surface, |s| s.data_map.get::<KdeRequest>().is_some_and(|r| matches!(*r.0.borrow(), Some(KdeMode::Client | KdeMode::None))))
}

impl KdeDecorationHandler for State {
    fn kde_decoration_state(&self) -> &KdeDecorationState {
        &self.kde_decoration_state
    }
    fn request_mode(&mut self, surface: &WlSurface, decoration: &OrgKdeKwinServerDecoration, mode: WEnum<KdeMode>) {
        if let WEnum::Value(mode) = mode {
            with_states(surface, |s| {
                s.data_map.insert_if_missing(KdeRequest::default);
                *s.data_map.get::<KdeRequest>().unwrap().0.borrow_mut() = Some(mode);
            });
            decoration.mode(mode);
            self.decorations_changed();
        }
    }
    fn release(&mut self, _decoration: &OrgKdeKwinServerDecoration, surface: &WlSurface) {
        with_states(surface, |s| {
            if let Some(r) = s.data_map.get::<KdeRequest>() {
                *r.0.borrow_mut() = None;
            }
        });
    }
}

/// A sans-serif face from the system, for the titles. None leaves the bars without text.
pub fn load_font() -> Option<FontVec> {
    let mut db = fontdb::Database::new();
    db.load_system_fonts();
    let families = ["Noto Sans", "DejaVu Sans", "Cantarell", "Liberation Sans", "Ubuntu", "Roboto"].map(fontdb::Family::Name);
    let families = [&families[..], &[fontdb::Family::SansSerif]].concat();
    let query = fontdb::Query { families: &families, weight: fontdb::Weight::SEMIBOLD, ..Default::default() };
    let id = db.query(&query)?;
    let face = db.face(id)?;
    tracing::info!(font = %face.post_script_name, "window decorations");
    db.with_face_data(id, |data, index| FontVec::try_from_vec_and_index(data.to_vec(), index).ok())?
}

pub fn maximized(window: &Window) -> bool {
    match window.underlying_surface() {
        WindowSurface::Wayland(t) => t.with_committed_state(|s| s.is_some_and(|s| s.states.contains(XdgState::Maximized))),
        WindowSurface::X11(x) => x.is_maximized(),
    }
}

pub fn resize_cursor(edge: ResizeEdge) -> CursorIcon {
    match edge {
        ResizeEdge::Top => CursorIcon::NResize,
        ResizeEdge::Bottom => CursorIcon::SResize,
        ResizeEdge::Left => CursorIcon::WResize,
        ResizeEdge::Right => CursorIcon::EResize,
        ResizeEdge::TopLeft => CursorIcon::NwResize,
        ResizeEdge::TopRight => CursorIcon::NeResize,
        ResizeEdge::BottomLeft => CursorIcon::SwResize,
        _ => CursorIcon::SeResize,
    }
}

impl State {
    /// Windows we draw a bar for: X11 ones that don't refuse decorations, Wayland ones that didn't say
    /// they draw their own (through xdg-decoration or KDE's protocol); never fullscreen ones. Read from
    /// the pending xdg state: what was asked for, before the client acks it, so the room a maximized
    /// window gets is right on the way out of fullscreen.
    pub fn decorated(&self, window: &Window) -> bool {
        match window.underlying_surface() {
            WindowSurface::X11(x) => !x.is_override_redirect() && !x.is_decorated() && !x.is_fullscreen(),
            WindowSurface::Wayland(t) => {
                t.with_pending_state(|s| s.decoration_mode != Some(Mode::ClientSide) && !s.states.contains(XdgState::Fullscreen)) && !kde_client_side(t.wl_surface())
            }
        }
    }

    /// A window's decorations came or went: a maximized one gains or loses the bar's room, and the
    /// pointer may now rest on a bar (or a client) without having moved.
    pub fn decorations_changed(&mut self) {
        self.relayout();
        self.pointer_motion(self.pointer_location);
    }

    /// The bar's height above `window`'s geometry: `BAR`, or 0 when the client decorates itself.
    pub fn bar_height(&self, window: &Window) -> i32 {
        if self.decorated(window) { BAR } else { 0 }
    }

    /// The bar above a decorated, mapped window, in output coordinates.
    pub fn bar(&self, window: &Window) -> Option<Rectangle<i32, Logical>> {
        if !self.decorated(window) {
            return None;
        }
        let geo = self.space.element_geometry(window)?;
        Some(Rectangle::new((geo.loc.x, geo.loc.y - BAR).into(), (geo.size.w, BAR).into()))
    }

    /// What is under `pos` among the windows, top-most first: a client surface (a window's own, its
    /// popups and its resize handles included) or one of our decorations; either hides everything below.
    pub fn window_under(&self, pos: Point<f64, Logical>) -> Option<Under> {
        for window in self.space.elements().rev() {
            let Some(loc) = self.space.element_location(window) else { continue };
            // the space places the geometry; surface_under wants the point from the surface's own origin,
            // which sits a shadow margin up and left of it for client-side decorated windows
            let origin = loc - window.geometry().loc;
            if let Some((surface, p)) = window.surface_under(pos - origin.to_f64(), WindowSurfaceType::ALL) {
                return Some(Under::Surface(surface, (p + origin).to_f64()));
            }
            if let Some(hit) = self.decoration_hit(window, pos) {
                return Some(Under::Decoration(window.clone(), hit));
            }
        }
        None
    }

    /// The decoration under `pos`, unless a client surface is.
    pub fn decoration_under(&self, pos: Point<f64, Logical>) -> Option<(Window, Hit)> {
        match self.window_under(pos)? {
            Under::Decoration(window, hit) => Some((window, hit)),
            Under::Surface(..) => None,
        }
    }

    /// The bar or the resize band of `window` at `pos`.
    fn decoration_hit(&self, window: &Window, pos: Point<f64, Logical>) -> Option<Hit> {
        let p = pos.to_i32_round::<i32>();
        let bar = self.bar(window)?;
        if bar.contains(p) {
            let x = p.x - bar.loc.x;
            let button = buttons(bar.size.w).into_iter().find(|(_, bx)| (*bx..bx + BUTTON).contains(&x)).map(|(b, _)| b);
            return Some(button.map_or(Hit::Bar, Hit::Button));
        }
        if maximized(window) {
            return None;
        }
        let geo = self.space.element_geometry(window)?;
        let frame = Rectangle::new(bar.loc, (geo.size.w, geo.size.h + BAR).into());
        let band = Rectangle::new(frame.loc - Point::from((EDGE, EDGE)), frame.size + Size::from((2 * EDGE, 2 * EDGE)));
        if !band.contains(p) || frame.contains(p) {
            return None;
        }
        let (left, right) = (p.x < frame.loc.x, p.x >= frame.loc.x + frame.size.w);
        let (top, bottom) = (p.y < frame.loc.y, p.y >= frame.loc.y + frame.size.h);
        Some(Hit::Edge(match (left, right, top, bottom) {
            (true, _, true, _) => ResizeEdge::TopLeft,
            (_, true, true, _) => ResizeEdge::TopRight,
            (true, _, _, true) => ResizeEdge::BottomLeft,
            (_, true, _, true) => ResizeEdge::BottomRight,
            (true, ..) => ResizeEdge::Left,
            (_, true, ..) => ResizeEdge::Right,
            (_, _, true, _) => ResizeEdge::Top,
            _ => ResizeEdge::Bottom,
        }))
    }

    /// The bar's render element for a decorated window, drawn at `scale` physical px per logical px.
    pub fn bar_element(&mut self, window: &Window, scale: f64) -> Option<MemoryRenderBufferRenderElement<GlesRenderer>> {
        let bar = self.bar(window)?;
        let focused = self.active.as_ref() == Some(window);
        let (title, _) = Self::title_app_id(window);
        let maximized = maximized(window);
        let size = bar.size.to_f64().to_physical(scale).to_i32_round::<i32>();
        let size = Size::<i32, Buffer>::from((size.w.max(1), size.h.max(1)));
        window.user_data().insert_if_missing(Cache::default);
        let cache = window.user_data().get::<Cache>().unwrap();
        let mut slot = cache.0.borrow_mut();
        if !slot.as_ref().is_some_and(|(t, f, m, s, _)| *t == title && *f == focused && *m == maximized && *s == size) {
            let rgba = paint(self.font.as_ref(), &title, focused, maximized, bar.size.w, size, scale as f32);
            let buffer = MemoryRenderBuffer::from_slice(&rgba, Fourcc::Abgr8888, size, 1, Transform::Normal, Some(vec![Rectangle::from_size(size)]));
            *slot = Some((title, focused, maximized, size, buffer));
        }
        let buffer = &slot.as_ref().unwrap().4;
        // the whole bitmap (buffer scale 1: its pixels are its logical size) shown at the bar's logical size,
        // which is its pixel size at the output's scale: 1:1
        let src = Rectangle::from_size(Size::<f64, Logical>::from((size.w as f64, size.h as f64)));
        MemoryRenderBufferRenderElement::from_buffer(&mut self.gpu.renderer, bar.loc.to_f64().to_physical(scale), buffer, None, Some(src), Some(bar.size), Kind::Unspecified).ok()
    }
}

/// The bar as straight RGBA: background, the buttons' line art, the title. `logical_w` is the bar's
/// width as hit-testing knows it; `size` its pixels.
fn paint(font: Option<&FontVec>, title: &str, focused: bool, maximized: bool, logical_w: i32, size: Size<i32, Buffer>, scale: f32) -> Vec<u8> {
    let (bg, fg) = if focused { ([0x2b, 0x2b, 0x30], [0xe4, 0xe4, 0xe7]) } else { ([0x1e, 0x1e, 0x22], [0x8b, 0x8b, 0x93]) };
    let (w, h) = (size.w, size.h);
    let (wu, hu) = (w as usize, h as usize);
    let mut px = vec![0u8; wu * hu * 4];
    for p in px.chunks_exact_mut(4) {
        p.copy_from_slice(&[bg[0], bg[1], bg[2], 255]);
    }
    let mut blend = |x: i32, y: i32, cov: f32| {
        if x < 0 || y < 0 || x >= w || y >= h {
            return;
        }
        let i = (y as usize * wu + x as usize) * 4;
        for c in 0..3 {
            px[i + c] = (px[i + c] as f32 * (1.0 - cov) + fg[c] as f32 * cov).round() as u8;
        }
    };
    // buttons: `thick` px strokes on a box `r` around each button's centre
    let thick = (1.5 * scale).round().max(1.0) as i32;
    let line = |blend: &mut dyn FnMut(i32, i32, f32), (x0, y0): (f32, f32), (x1, y1): (f32, f32)| {
        let n = ((x1 - x0).abs().max((y1 - y0).abs()) * 2.0).ceil().max(1.0) as i32;
        for i in 0..=n {
            let t = i as f32 / n as f32;
            let (x, y) = ((x0 + (x1 - x0) * t).round() as i32, (y0 + (y1 - y0) * t).round() as i32);
            for dx in 0..thick {
                for dy in 0..thick {
                    blend(x + dx, y + dy, 1.0);
                }
            }
        }
    };
    let r = 5.0 * scale;
    for (button, bx) in buttons(logical_w) {
        let (cx, cy) = ((bx as f32 + BUTTON as f32 / 2.0) * scale - thick as f32 / 2.0, h as f32 / 2.0 - thick as f32 / 2.0);
        match button {
            Button::Close => {
                line(&mut blend, (cx - r, cy - r), (cx + r, cy + r));
                line(&mut blend, (cx - r, cy + r), (cx + r, cy - r));
            }
            Button::Maximize => {
                let (o, s) = if maximized { (2.0 * scale, r - 2.0 * scale) } else { (0.0, r) };
                let (l, t, rr, b) = (cx - s - o, cy - s + o, cx + s - o, cy + s + o);
                line(&mut blend, (l, t), (rr, t));
                line(&mut blend, (rr, t), (rr, b));
                line(&mut blend, (rr, b), (l, b));
                line(&mut blend, (l, b), (l, t));
                if maximized {
                    // the window behind, offset up and right
                    line(&mut blend, (l + 2.0 * o, t - 2.0 * o), (rr + 2.0 * o, t - 2.0 * o));
                    line(&mut blend, (rr + 2.0 * o, t - 2.0 * o), (rr + 2.0 * o, b - 2.0 * o));
                }
            }
            Button::Minimize => line(&mut blend, (cx - r, cy + r * 0.8), (cx + r, cy + r * 0.8)),
        }
    }
    if let Some(font) = font {
        let scaled = font.as_scaled(PxScale::from(14.0 * scale));
        let advance = |c: char| scaled.h_advance(scaled.scaled_glyph(c).id);
        let width = |s: &str| s.chars().map(advance).sum::<f32>();
        let (x0, max_w) = (12.0 * scale, w as f32 - (3 * BUTTON) as f32 * scale - 20.0 * scale);
        let mut text = title.to_string();
        if width(&text) > max_w {
            while !text.is_empty() && width(&text) + advance('…') > max_w {
                text.pop();
            }
            text.push('…');
        }
        let baseline = (h as f32 - (scaled.ascent() - scaled.descent())) / 2.0 + scaled.ascent();
        let mut x = x0;
        for c in text.chars() {
            let mut glyph = scaled.scaled_glyph(c);
            glyph.position = ab_glyph::point(x, baseline);
            x += scaled.h_advance(glyph.id);
            if let Some(outlined) = scaled.outline_glyph(glyph) {
                let min = outlined.px_bounds().min;
                outlined.draw(|gx, gy, cov| blend(min.x as i32 + gx as i32, min.y as i32 + gy as i32, cov));
            }
        }
    }
    px
}
