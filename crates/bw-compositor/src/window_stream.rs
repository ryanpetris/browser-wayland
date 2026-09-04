//! One window as its own video stream: rendered from its buffers (popups included) into a private
//! dmabuf swapchain and handed to its own encoder, at the output's scale, only when it changed.

use std::os::fd::AsFd;

use anyhow::{Context, Result};
use bw_core::{DmabufFrame, FrameSink, OutputGeometry};
use smithay::{
    backend::renderer::{Bind, damage::OutputDamageTracker, element::{AsRenderElements, surface::WaylandSurfaceRenderElement}, gles::GlesRenderer},
    desktop::Window,
    utils::{IsAlive, Physical, Point, Scale, Size, Transform},
};

use crate::{State, gpu::DmabufSwapchain, render::CLEAR};

pub struct WindowStream {
    pub key: u64,
    pub window: Window,
    swapchain: DmabufSwapchain,
    tracker: OutputDamageTracker,
    sink: Box<dyn FrameSink>,
    size: Size<i32, Physical>,
    seq: u64,
}

/// Swapchain slot id (see `render.rs`), counted per stream.
struct SlotId(u32);

impl State {
    pub fn start_window_stream(&mut self, key: u64, id: u64, sink: Box<dyn FrameSink>) {
        let Some(window) = self.window_by_id(id) else { return }; // the session ends on its own: nothing ever arrives
        let swapchain = self.gpu.swapchain(2, 2); // resized to the window before the first frame
        let tracker = OutputDamageTracker::new((2, 2), 1.0, Transform::Normal);
        self.window_streams.push(WindowStream { key, window, swapchain, tracker, sink, size: Size::default(), seq: 0 });
        self.dirty = true;
    }

    pub fn stop_window_stream(&mut self, key: u64) {
        self.window_streams.retain(|s| s.key != key); // dropping the sink stops its pipeline
    }

    /// Called after the output's frame: every stream whose window changed gets a frame (every stream, if `force`).
    pub fn render_window_streams(&mut self, force: bool) {
        self.window_streams.retain(|s| s.window.alive());
        for i in 0..self.window_streams.len() {
            if let Err(e) = self.render_window_stream(i, force) {
                tracing::warn!(key = self.window_streams[i].key, "window stream: {e:#}");
            }
        }
    }

    fn render_window_stream(&mut self, i: usize, force: bool) -> Result<()> {
        let scale = self.geometry.scale;
        let (fourcc, modifier) = (self.gpu.fourcc as u32, u64::from(self.gpu.modifier));
        let now = self.clock.now();
        let Self { window_streams, gpu, .. } = self;
        let s = &mut window_streams[i];
        let geo = s.window.geometry();
        // even sizes for 4:2:0 encoders, like the output
        let size = geo.size.to_f64().to_physical(scale).to_i32_round::<i32>();
        let size = Size::<i32, Physical>::from(((size.w & !1).max(2), (size.h & !1).max(2)));
        if size != s.size {
            s.size = size;
            s.swapchain.resize(size.w as u32, size.h as u32);
            s.tracker = OutputDamageTracker::new(size, scale, Transform::Normal);
            s.sink.output_changed(OutputGeometry { width_px: size.w as u32, height_px: size.h as u32, scale, refresh_mhz: 60_000 }, fourcc, modifier);
        }
        let Some(slot) = s.swapchain.acquire()? else { return Ok(()) }; // the encoder holds every buffer: next time
        let age = if force { 0 } else { slot.age() as usize };
        let mut dmabuf = (*slot).clone();
        // the geometry's corner at the origin, as for snapshots
        let loc = Point::from((-geo.loc.x, -geo.loc.y)).to_f64().to_physical(scale).to_i32_round();
        let elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = s.window.render_elements(&mut gpu.renderer, loc, Scale::from(scale), 1.0);
        let (sync, damaged) = {
            let mut fb = gpu.renderer.bind(&mut dmabuf)?;
            let res = s.tracker.render_output(&mut gpu.renderer, &mut fb, age, &elements, CLEAR)?;
            (res.sync, res.damage.is_some())
        };
        while sync.wait().is_err() {}
        if !damaged {
            return Ok(());
        }
        s.swapchain.submitted(&slot);
        slot.userdata().insert_if_missing_threadsafe(|| SlotId(s.seq as u32));
        let slot_id = slot.userdata().get::<SlotId>().unwrap().0;
        s.seq += 1;
        let fd = dmabuf.handles().next().context("dmabuf has no plane")?.as_fd().try_clone_to_owned().context("dup dmabuf fd")?;
        s.sink
            .submit(DmabufFrame {
                fd,
                width: size.w as u32,
                height: size.h as u32,
                fourcc,
                modifier,
                stride: dmabuf.strides().next().unwrap(),
                offset: dmabuf.offsets().next().unwrap(),
                slot_id,
                pts: now.into(),
                seq: s.seq,
                lease: Box::new(slot),
            })
            .map_err(|e| anyhow::anyhow!("{e}"))
    }
}
