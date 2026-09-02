//! Named cursors from the user's Xcursor theme, cached per (icon, scale).

use std::collections::HashMap;

use smithay::{
    backend::{allocator::Fourcc, renderer::element::memory::MemoryRenderBuffer},
    input::pointer::CursorIcon,
    utils::{Logical, Point, Transform},
};

pub struct CursorTheme {
    theme: xcursor::CursorTheme,
    size: u32,
    cache: HashMap<(CursorIcon, i32), Option<(MemoryRenderBuffer, Point<i32, Logical>)>>,
}

impl CursorTheme {
    pub fn load() -> Self {
        let name = std::env::var("XCURSOR_THEME").unwrap_or_else(|_| "default".into());
        let size = std::env::var("XCURSOR_SIZE").ok().and_then(|s| s.parse().ok()).unwrap_or(24);
        Self { theme: xcursor::CursorTheme::load(&name), size, cache: HashMap::new() }
    }

    /// Buffer plus logical hotspot, or `None` if the theme has no such cursor.
    pub fn get(&mut self, icon: CursorIcon, scale: f64) -> Option<&(MemoryRenderBuffer, Point<i32, Logical>)> {
        let s = scale.round().max(1.0) as i32;
        let (theme, size) = (&self.theme, self.size);
        self.cache
            .entry((icon, s))
            .or_insert_with(|| {
                let names = std::iter::once(icon.name()).chain(icon.alt_names().iter().copied());
                let path = names.filter_map(|n| theme.load_icon(n)).next()?;
                let images = xcursor::parser::parse_xcursor(&std::fs::read(path).ok()?)?;
                let target = (size as i32) * s;
                let img = images.iter().min_by_key(|i| (i.size as i32 - target).abs())?;
                let buffer = MemoryRenderBuffer::from_slice(
                    &img.pixels_rgba,
                    Fourcc::Abgr8888,
                    (img.width as i32, img.height as i32),
                    s,
                    Transform::Normal,
                    None,
                );
                Some((buffer, (img.xhot as i32 / s, img.yhot as i32 / s).into()))
            })
            .as_ref()
    }
}
