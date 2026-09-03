use std::{os::fd::AsFd, time::{Duration, Instant}};

use anyhow::{Context, Result};
use bw_core::DmabufFrame;
use smithay::{
    backend::renderer::{Bind, element::surface::WaylandSurfaceRenderElement, gles::GlesRenderer},
    desktop::{space::render_output, utils::send_frames_surface_tree},
    input::pointer::CursorImageStatus,
};

use crate::State;

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

        let mut dmabuf = (*slot).clone();
        let (sync, damaged) = {
            let mut fb = self.gpu.renderer.bind(&mut dmabuf)?;
            // The pointer is drawn by the browser, so there are no custom elements.
            let res = render_output::<_, WaylandSurfaceRenderElement<GlesRenderer>, _, _>(
                &self.output, &mut self.gpu.renderer, &mut fb, 1.0, age, [&self.space], &[], &mut self.damage_tracker, CLEAR,
            )?;
            (res.sync, res.damage.is_some())
        };
        self.last_render = Instant::now();

        let now = self.clock.now();
        for window in self.space.elements() {
            window.send_frame(&self.output, now, Some(Duration::ZERO), |_, _| Some(self.output.clone()));
            window.send_dmabuf_feedback(&self.output, |_, _| Some(self.output.clone()), |_, _| &self.dmabuf_feedback);
        }
        if let CursorImageStatus::Surface(s) = &self.cursor_status {
            send_frames_surface_tree(s, &self.output, now, Some(Duration::ZERO), |_, _| Some(self.output.clone()));
        }

        // ponytail: CPU wait for the GPU; export the fence instead if it ever shows up in profiles.
        // Done before any early return: the next commit releases client buffers, so our reads must be over.
        while sync.wait().is_err() {} // Interrupted: wait again
        if !damaged || !self.viewer_connected {
            self.dirty = false;
            self.force_full_frame = false;
            return Ok(()); // nothing new to show (or nobody watching): don't encode
        }
        self.gpu.swapchain.submitted(&slot);
        slot.userdata().insert_if_missing_threadsafe(|| SlotId(self.frame_seq as u32));
        let slot_id = slot.userdata().get::<SlotId>().unwrap().0;
        self.frame_seq += 1;

        let submitted = self.sink.submit(DmabufFrame {
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
        match submitted {
            Ok(()) => {
                self.dirty = false;
                self.force_full_frame = false;
            }
            Err(e) => {
                // The damage tracker already advanced, so the retry must redraw everything.
                tracing::warn!("frame not encoded: {e}");
                self.force_full_frame = true;
            }
        }
        Ok(())
    }
}
