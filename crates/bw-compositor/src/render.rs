use std::{os::fd::AsFd, time::Duration};

use anyhow::{Context, Result};
use bw_core::DmabufFrame;
use smithay::{
    backend::renderer::{
        Bind, ImportAll, ImportMem,
        element::{
            Kind,
            memory::MemoryRenderBufferRenderElement,
            render_elements,
            surface::{WaylandSurfaceRenderElement, render_elements_from_surface_tree},
        },
        gles::GlesRenderer,
    },
    desktop::{space::render_output, utils::send_frames_surface_tree},
    input::pointer::{CursorImageStatus, CursorImageSurfaceData},
    wayland::compositor::with_states,
};

use crate::State;

render_elements! {
    pub CursorRenderElement<R> where R: ImportAll + ImportMem;
    Surface = WaylandSurfaceRenderElement<R>,
    Memory = MemoryRenderBufferRenderElement<R>,
}

const CLEAR: [f32; 4] = [0.12, 0.12, 0.14, 1.0];

/// Per-swapchain-slot id so the encoder can cache its import.
struct SlotId(u32);

impl State {
    pub fn tick(&mut self) {
        if !(self.dirty || self.force_full_frame) {
            return;
        }
        if let Err(e) = self.render_frame() {
            tracing::warn!("render failed: {e:#}");
        }
    }

    fn render_frame(&mut self) -> Result<()> {
        // No free slot means the encoder still holds every buffer: skip this tick, stay dirty.
        let Some(slot) = self.gpu.swapchain.acquire()? else { return Ok(()) };
        let age = if self.force_full_frame { 0 } else { slot.age() as usize };
        let scale = self.output.current_scale().fractional_scale();
        let cursor = self.cursor_elements(scale);

        let mut dmabuf = (*slot).clone();
        let (sync, damaged) = {
            let mut fb = self.gpu.renderer.bind(&mut dmabuf)?;
            let res = render_output::<_, CursorRenderElement<GlesRenderer>, _, _>(
                &self.output, &mut self.gpu.renderer, &mut fb, 1.0, age, [&self.space], &cursor, &mut self.damage_tracker, CLEAR,
            )?;
            (res.sync, res.damage.is_some())
        };
        self.dirty = false;
        self.force_full_frame = false;

        let now = self.clock.now();
        for window in self.space.elements() {
            window.send_frame(&self.output, now, Some(Duration::ZERO), |_, _| Some(self.output.clone()));
            window.send_dmabuf_feedback(&self.output, |_, _| Some(self.output.clone()), |_, _| &self.dmabuf_feedback);
        }
        if let CursorImageStatus::Surface(s) = &self.cursor_status {
            send_frames_surface_tree(s, &self.output, now, Some(Duration::ZERO), |_, _| Some(self.output.clone()));
        }

        if !damaged || !self.viewer_connected {
            return Ok(()); // nothing new to show (or nobody watching): don't encode
        }
        sync.wait().ok(); // ponytail: CPU wait for the GPU; export the fence instead if it ever shows up in profiles
        self.gpu.swapchain.submitted(&slot);
        slot.userdata().insert_if_missing_threadsafe(|| SlotId(self.frame_seq as u32));
        let slot_id = slot.userdata().get::<SlotId>().unwrap().0;
        self.frame_seq += 1;

        self.sink.submit(DmabufFrame {
            fd: dmabuf.handles().next().context("dmabuf has no plane")?.as_fd().try_clone_to_owned()?,
            width: self.geometry.width_px,
            height: self.geometry.height_px,
            fourcc: self.gpu.fourcc as u32,
            modifier: u64::from(self.gpu.modifier),
            stride: dmabuf.strides().next().unwrap(),
            offset: dmabuf.offsets().next().unwrap(),
            slot_id,
            pts: now.into(),
            seq: self.frame_seq,
            lease: Box::new(slot),
        });
        Ok(())
    }

    fn cursor_elements(&mut self, scale: f64) -> Vec<CursorRenderElement<GlesRenderer>> {
        let pos = self.pointer_location;
        match &self.cursor_status {
            CursorImageStatus::Hidden => vec![],
            CursorImageStatus::Surface(surface) => {
                let hotspot = with_states(surface, |states| {
                    states.data_map.get::<CursorImageSurfaceData>().unwrap().lock().unwrap().hotspot
                });
                render_elements_from_surface_tree(
                    &mut self.gpu.renderer,
                    surface,
                    (pos - hotspot.to_f64()).to_physical_precise_round(scale),
                    scale,
                    1.0,
                    Kind::Cursor,
                )
            }
            CursorImageStatus::Named(icon) => {
                let Some((buffer, hotspot)) = self.cursor.get(*icon, scale) else { return vec![] };
                let loc = (pos - hotspot.to_f64()).to_physical(scale);
                MemoryRenderBufferRenderElement::from_buffer(&mut self.gpu.renderer, loc, buffer, None, None, None, Kind::Cursor)
                    .map(|e| vec![CursorRenderElement::Memory(e)])
                    .unwrap_or_default()
            }
        }
    }
}
