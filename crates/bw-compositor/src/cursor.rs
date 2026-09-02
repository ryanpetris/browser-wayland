//! Pointer images are not composited; they are exported to the browser, which draws its own cursor.

use std::collections::HashMap;

use bw_core::{CursorImage, Event};
use smithay::{
    backend::renderer::utils::with_renderer_surface_state,
    input::pointer::{CursorIcon, CursorImageStatus, CursorImageSurfaceData},
    reexports::wayland_server::protocol::{wl_shm, wl_surface::WlSurface},
    wayland::{compositor::with_states, shm::with_buffer_contents},
};

use crate::State;

/// Named cursors from the user's Xcursor theme, cached per icon.
pub struct CursorTheme {
    theme: xcursor::CursorTheme,
    size: u32,
    cache: HashMap<CursorIcon, Option<xcursor::parser::Image>>,
}

impl CursorTheme {
    pub fn load() -> Self {
        let name = std::env::var("XCURSOR_THEME").unwrap_or_else(|_| "default".into());
        let size = std::env::var("XCURSOR_SIZE").ok().and_then(|s| s.parse().ok()).unwrap_or(24);
        Self { theme: xcursor::CursorTheme::load(&name), size, cache: HashMap::new() }
    }

    // ponytail: always the 1x image; a HiDPI browser gets a slightly soft cursor. Send per-dpr sizes if it bothers anyone.
    fn image(&mut self, icon: CursorIcon) -> Option<CursorImage> {
        let (theme, size) = (&self.theme, self.size);
        self.cache
            .entry(icon)
            .or_insert_with(|| {
                let names = std::iter::once(icon.name()).chain(icon.alt_names().iter().copied());
                let path = names.filter_map(|n| theme.load_icon(n)).next()?;
                let images = xcursor::parser::parse_xcursor(&std::fs::read(path).ok()?)?;
                images.into_iter().min_by_key(|i| (i.size as i32 - size as i32).abs())
            })
            .as_ref()
            .map(|img| CursorImage {
                width: img.width,
                height: img.height,
                hot_x: img.xhot as i32,
                hot_y: img.yhot as i32,
                rgba: unpremultiply(img.pixels_rgba.chunks_exact(4).map(|p| (p[0], p[1], p[2], p[3]))),
            })
    }
}

/// Straight-alpha RGBA from premultiplied (r, g, b, a) pixels.
fn unpremultiply(pixels: impl Iterator<Item = (u8, u8, u8, u8)>) -> Vec<u8> {
    let mut out = Vec::new();
    for (r, g, b, a) in pixels {
        let un = |c: u8| if a == 0 { 0 } else { (c as u32 * 255 / a as u32).min(255) as u8 };
        out.extend([un(r), un(g), un(b), a]);
    }
    out
}

/// The client's cursor surface (a wl_shm buffer) as straight RGBA.
fn surface_cursor(surface: &WlSurface) -> Option<CursorImage> {
    let hotspot = with_states(surface, |s| s.data_map.get::<CursorImageSurfaceData>().map(|d| d.lock().unwrap().hotspot))?;
    let buffer = with_renderer_surface_state(surface, |s| s.buffer().cloned())??;
    with_buffer_contents(&buffer, |ptr, len, data| {
        // Safety: shm buffer memory of `len` bytes, valid for the duration of the closure.
        let src = unsafe { std::slice::from_raw_parts(ptr, len) };
        let (w, h, stride) = (data.width as usize, data.height as usize, data.stride as usize);
        let opaque = data.format == wl_shm::Format::Xrgb8888;
        let rows = (0..h).flat_map(|y| src[data.offset as usize + y * stride..][..w * 4].chunks_exact(4));
        // wl_shm [AX]RGB8888 is little-endian: bytes are B, G, R, A.
        let rgba = unpremultiply(rows.map(|p| (p[2], p[1], p[0], if opaque { 255 } else { p[3] })));
        CursorImage { width: w as u32, height: h as u32, hot_x: hotspot.x, hot_y: hotspot.y, rgba }
    })
    .ok()
}

impl State {
    /// Send the current pointer image to the viewer.
    pub fn export_cursor(&mut self) {
        let image = match &self.cursor_status {
            CursorImageStatus::Hidden => None,
            CursorImageStatus::Named(icon) => self.cursor.image(*icon),
            CursorImageStatus::Surface(surface) => surface_cursor(surface),
        };
        let _ = self.events.send(Event::Cursor(image));
    }
}
