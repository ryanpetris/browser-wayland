//! One window as its own video stream: rendered from its buffers (popups included) into targets of its
//! own (a dmabuf swapchain, or a texture read back without a GPU) and handed to its own encoder, at the
//! output's scale, only when it changed.

use std::{os::fd::AsFd, time::{Duration, Instant}};

use anyhow::{Context, Result};
use bw_core::{Frame, FrameBuffer, FrameSink, OutputGeometry};
use smithay::{
    backend::{
        allocator::Buffer as _,
        renderer::{damage::OutputDamageTracker, element::{AsRenderElements, surface::WaylandSurfaceRenderElement}, gles::GlesRenderer},
    },
    desktop::Window,
    utils::{Physical, Point, Scale, Size, Transform},
};

use crate::{State, gpu::{Target, Targets, read_pixels}, render::{CLEAR, SlotId}};

/// An interactive resize commits a new size every frame; the encoder is rebuilt once it stops.
const SETTLE: Duration = Duration::from_millis(150);

pub struct WindowStream {
    pub key: u64,
    pub window: Window,
    targets: Targets,
    tracker: OutputDamageTracker,
    sink: Box<dyn FrameSink>,
    size: Size<i32, Physical>,
    scale: f64,
    /// A size other than `size`, and since when.
    settling: Option<(Size<i32, Physical>, Instant)>,
    /// What the buffers really are (GBM may fall back); the sink is told when it differs.
    modifier: u64,
    /// The last frame wasn't handed over (no free buffer, or the sink refused it): render again, whole.
    pub pending: bool,
    seq: u64,
}

impl State {
    pub fn start_window_stream(&mut self, key: u64, id: u64, sink: Box<dyn FrameSink>) {
        let Some(window) = self.window_by_id(id) else { return }; // the session ends on its own: nothing ever arrives
        let targets = self.gpu.targets(2, 2); // resized to the window before the first frame
        let tracker = OutputDamageTracker::new((2, 2), 1.0, Transform::Normal);
        let modifier = u64::from(self.gpu.modifier);
        self.window_streams.push(WindowStream { key, window, targets, tracker, sink, size: Size::default(), scale: 0.0, settling: None, modifier, pending: true, seq: 0 });
        self.dirty = true;
    }

    pub fn stop_window_stream(&mut self, key: u64) {
        self.window_streams.retain(|s| s.key != key); // dropping the sink stops its pipeline
    }

    /// Called after the output's frame: every stream whose window changed gets a frame (every stream, if `force`).
    pub fn render_window_streams(&mut self, force: bool) {
        // gone from the desktop (closed, or an X11 window withdrawn while its handle lives on): stream over
        let present = self.full_stack();
        self.window_streams.retain(|s| present.contains(&s.window));
        for i in 0..self.window_streams.len() {
            if let Err(e) = self.render_window_stream(i, force) {
                tracing::warn!(key = self.window_streams[i].key, "window stream: {e:#}");
                self.window_streams[i].pending = true; // the update it missed is retried whole
            }
        }
    }

    fn render_window_stream(&mut self, i: usize, force: bool) -> Result<()> {
        let scale = self.geometry.scale;
        let fourcc = self.gpu.fourcc as u32;
        let now = self.clock.now();
        let Self { window_streams, gpu, .. } = self;
        let s = &mut window_streams[i];
        let geo = s.window.geometry();
        // even sizes for 4:2:0 encoders, rounded up so no row or column of the window is cut off
        let size = geo.size.to_f64().to_physical(scale).to_i32_round::<i32>();
        let size = Size::<i32, Physical>::from(((size.w + 1) & !1, (size.h + 1) & !1));
        if size.w < 16 || size.h < 16 {
            s.pending = false;
            return Ok(()); // unmapped (no buffer, no geometry); the encoder takes nothing this small anyway
        }
        let geometry = |modifier| (OutputGeometry { width_px: size.w as u32, height_px: size.h as u32, scale, refresh_mhz: 60_000 }, fourcc, modifier);
        if size != s.size || scale != s.scale {
            if s.size != Size::default() && size != s.size {
                // not the first size: wait for it to settle
                match s.settling {
                    Some((sz, since)) if sz == size && since.elapsed() >= SETTLE => {}
                    Some((sz, _)) if sz == size => {
                        s.pending = true; // keeps the loop ticking until then
                        return Ok(());
                    }
                    _ => {
                        s.settling = Some((size, Instant::now()));
                        s.pending = true;
                        return Ok(());
                    }
                }
            }
            s.settling = None;
            if size != s.size {
                s.targets.resize(size.w as u32, size.h as u32);
            }
            s.size = size;
            s.scale = scale;
            s.tracker = OutputDamageTracker::new(size, scale, Transform::Normal);
            let (geo, fourcc, modifier) = geometry(s.modifier);
            s.sink.output_changed(geo, fourcc, modifier);
        }
        let Some((mut target, age)) = s.targets.acquire(&mut gpu.renderer, gpu.fourcc)? else {
            s.pending = true; // the encoder holds every buffer: next tick
            return Ok(());
        };
        let age = if force || s.pending { 0 } else { age };
        if let Target::Slot { dmabuf, .. } = &target
            && u64::from(dmabuf.format().modifier) != s.modifier
        {
            // GBM gave another layout than asked (see render.rs); the encoder must import it as it is
            s.modifier = u64::from(dmabuf.format().modifier);
            let (geo, fourcc, modifier) = geometry(s.modifier);
            s.sink.output_changed(geo, fourcc, modifier);
        }
        // the geometry's corner at the origin, as for snapshots
        let loc = Point::from((-geo.loc.x, -geo.loc.y)).to_f64().to_physical(scale).to_i32_round();
        let elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = s.window.render_elements(&mut gpu.renderer, loc, Scale::from(scale), 1.0);
        let texture = matches!(target, Target::Texture);
        let (sync, damaged, pixels) = {
            let mut fb = s.targets.bind(&mut gpu.renderer, &mut target)?;
            let res = s.tracker.render_output(&mut gpu.renderer, &mut fb, age, &elements, CLEAR)?;
            let pixels = if texture && res.damage.is_some() { Some(read_pixels(&mut gpu.renderer, &fb, (size.w, size.h).into(), gpu.fourcc)?) } else { None };
            (res.sync, res.damage.is_some(), pixels)
        };
        while sync.wait().is_err() {}
        if !damaged {
            s.pending = false;
            return Ok(());
        }
        s.seq += 1;
        let buffer = match target {
            Target::Slot { slot, dmabuf } => {
                if let Targets::Dmabuf(sc) = &mut s.targets {
                    sc.submitted(&slot);
                }
                slot.userdata().insert_if_missing_threadsafe(|| SlotId(s.seq as u32));
                let slot_id = slot.userdata().get::<SlotId>().unwrap().0;
                let fd = dmabuf.handles().next().context("dmabuf has no plane")?.as_fd().try_clone_to_owned().context("dup dmabuf fd")?;
                FrameBuffer::Dmabuf { fd, modifier: s.modifier, stride: dmabuf.strides().next().unwrap(), offset: dmabuf.offsets().next().unwrap(), slot_id, lease: Box::new(slot) }
            }
            Target::Texture => FrameBuffer::Memory { data: pixels.unwrap_or_default(), stride: size.w as u32 * 4 },
        };
        let submitted = s.sink.submit(Frame { width: size.w as u32, height: size.h as u32, fourcc, pts: now.into(), seq: s.seq, refine: false, buffer });
        s.pending = !matches!(submitted, Ok(bw_core::Submit::Encoded)); // the tracker advanced: a retry redraws everything
        submitted.map(|_| ()).map_err(|e| anyhow::anyhow!("{e}"))
    }
}
