//! Pointer images are not composited; they are exported to the browser, which draws its own cursor.

use std::collections::HashMap;

use bw_core::{CursorImage, Event};
use smithay::{
    wayland::{compositor::SurfaceAttributes, viewporter::ViewportCachedState},
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
                logical_w: img.width,
                logical_h: img.height,
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
    // GTK 3 scales a HiDPI cursor with wl_surface.set_buffer_scale, GTK 4 with a viewport destination
    let (hotspot, scale, viewport) = with_states(surface, |s| {
        let hotspot = s.data_map.get::<CursorImageSurfaceData>().map(|d| d.lock().unwrap().hotspot)?;
        let scale = s.cached_state.get::<SurfaceAttributes>().current().buffer_scale.max(1) as u32;
        Some((hotspot, scale, s.cached_state.get::<ViewportCachedState>().current().size()))
    })?;
    let buffer = with_renderer_surface_state(surface, |s| s.buffer().cloned())??;
    with_buffer_contents(&buffer, |ptr, len, data| {
        let opaque = match data.format {
            wl_shm::Format::Argb8888 => false,
            wl_shm::Format::Xrgb8888 => true,
            _ => return None,
        };
        let (w, h, stride, off) = (data.width as usize, data.height as usize, data.stride as usize, data.offset as usize);
        if w == 0 || h == 0 || off + (h - 1) * stride + w * 4 > len {
            return None;
        }
        // The client may write this memory at any time, so read through the raw pointer, never a slice.
        // wl_shm [AX]RGB8888 is little-endian: bytes are B, G, R, A.
        let pixels = (0..h).flat_map(|y| (0..w).map(move |x| off + y * stride + x * 4)).map(|i| unsafe {
            let p = ptr.add(i);
            (p.add(2).read_volatile(), p.add(1).read_volatile(), p.read_volatile(), if opaque { 255 } else { p.add(3).read_volatile() })
        });
        let (logical_w, logical_h) = viewport.map_or((w as u32 / scale, h as u32 / scale), |v| (v.w.max(1) as u32, v.h.max(1) as u32));
        tracing::debug!(w, h, logical_w, logical_h, ?hotspot, "client cursor");
        Some(CursorImage { width: w as u32, height: h as u32, hot_x: hotspot.x, hot_y: hotspot.y, logical_w, logical_h, rgba: unpremultiply(pixels) })
    })
    .ok()
    .flatten()
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
