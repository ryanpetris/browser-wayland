use std::{os::fd::AsFd, sync::Arc, time::{Duration, Instant}};

use anyhow::{Context, Result};
use elsewhere_core::{Frame, FrameBuffer, Submit};

/// How long after the last change the refine frame follows.
const REFINE_AFTER: Duration = Duration::from_millis(150);
use smithay::{
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    wayland::compositor::SurfaceData,
    reexports::wayland_protocols::xdg::shell::server::xdg_toplevel,
    desktop::Window,
    utils::{Logical, Point},
    backend::allocator::Buffer as _,
    backend::renderer::{ImportAll, ImportMem, element::{AsRenderElements, Id, Kind, memory::MemoryRenderBufferRenderElement, surface::{WaylandSurfaceRenderElement, render_elements_from_surface_tree}}, gles::GlesRenderer},
    desktop::{
        WindowSurface, layer_map_for_output,
        utils::{OutputPresentationFeedback, send_frames_surface_tree},
    },
    reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback,
    wayland::presentation::Refresh,
    utils::Scale,
    wayland::shell::wlr_layer::Layer,
    input::pointer::CursorImageStatus,
};

use crate::{State, gpu::{Gpu, Target, Targets, read_pixels}};

pub const CLEAR: [f32; 4] = [0.12, 0.12, 0.14, 1.0];

smithay::backend::renderer::element::render_elements! {
    /// What the output is made of: the clients' surfaces and our title bars.
    pub OutputElement<R> where R: ImportAll + ImportMem;
    Surface = WaylandSurfaceRenderElement<R>,
    Bar = MemoryRenderBufferRenderElement<R>,
}

/// Per-swapchain-slot id so the encoder can cache its import.
pub(crate) struct SlotId(pub u32);

impl State {
    /// A mapped window is fullscreen: it covers the panels (Top layer), only the Overlay layer stays above.
    pub fn fullscreen_window_mapped(&self) -> bool {
        self.space.elements().any(|w| match w.underlying_surface() {
            // a toplevel that unmapped with a null buffer stays in the space until destroyed, with no committed state
            WindowSurface::Wayland(t) => t.with_committed_state(|s| s.is_some_and(|s| s.states.contains(xdg_toplevel::State::Fullscreen))),
            WindowSurface::X11(x) => x.is_fullscreen(),
        })
    }

    /// Everything on the output, front to back, at `scale` physical pixels per logical pixel: the overlay
    /// (and, unless a window is fullscreen, top) layers, the windows with their popups and title bars,
    /// then the bottom and background layers.
    pub fn output_elements(&mut self, scale: f64) -> Vec<OutputElement<GlesRenderer>> {
        let s = Scale::from(scale);
        let output_loc = self.space.output_geometry(&self.output).map(|g| g.loc).unwrap_or_default();
        let fullscreen = self.fullscreen_window_mapped();
        // the layers first (their map borrows the output), the windows and their bars between them after
        let (mut out, bottom) = {
            let layers = layer_map_for_output(&self.output);
            let mut layer_elements = |pick: fn(Layer) -> bool| -> Vec<OutputElement<GlesRenderer>> {
                layers
                    .layers()
                    .rev()
                    .filter(|l| pick(l.layer()))
                    .filter_map(|l| layers.layer_geometry(l).map(|g| (g.loc, l)))
                    .flat_map(|(loc, l)| l.render_elements::<WaylandSurfaceRenderElement<GlesRenderer>>(&mut self.gpu.renderer, loc.to_physical_precise_round(scale), s, 1.0))
                    .map(Into::into)
                    .collect()
            };
            let top = layer_elements(if fullscreen { |l| l == Layer::Overlay } else { |l| matches!(l, Layer::Top | Layer::Overlay) });
            (top, layer_elements(|l| matches!(l, Layer::Bottom | Layer::Background)))
        };
        let windows: Vec<(Window, Point<i32, Logical>)> = self.space.elements().rev().filter_map(|w| Some((w.clone(), self.space.element_location(w)?))).collect();
        for (w, loc) in windows {
            // the space places the geometry; render_elements wants the surface origin
            let origin = loc - w.geometry().loc - output_loc;
            out.extend(w.render_elements::<WaylandSurfaceRenderElement<GlesRenderer>>(&mut self.gpu.renderer, origin.to_physical_precise_round(scale), s, 1.0).into_iter().map(Into::into));
            out.extend(self.bar_element(&w, scale).map(Into::into)); // under the window's popups, which may open upward
        }
        out.extend(bottom);
        // a client's drag icon rides the pointer, over everything
        if let Some((icon, offset)) = &self.dnd_icon {
            let loc = (self.pointer_location + offset.to_f64()).to_physical_precise_round(scale);
            let icon: Vec<OutputElement<GlesRenderer>> = render_elements_from_surface_tree::<_, WaylandSurfaceRenderElement<GlesRenderer>>(&mut self.gpu.renderer, icon, loc, s, 1.0, Kind::Cursor).into_iter().map(Into::into).collect();
            out.splice(0..0, icon);
        }
        out
    }

    pub fn tick(&mut self) {
        // The picture settled: one more frame, unchanged, for the encoders to sharpen what motion left rough.
        let refine = !self.dirty && !self.force_full_frame && !self.viewer_sinks.is_empty() && self.refine_due.is_some_and(|t| Instant::now() >= t);
        if refine {
            tracing::debug!("refine frame: the picture settled");
            self.refine_due = None;
            self.force_full_frame = true;
        }
        if !(self.dirty || self.force_full_frame || self.window_streams.iter().any(|s| s.pending)) {
            return;
        }
        let force = self.force_full_frame; // render_frame clears it
        if let Err(e) = self.render_frame(refine) {
            tracing::warn!("render failed: {e:#}");
            self.force_full_frame = true; // the damage tracker advanced: the retry draws everything
        }
        self.render_window_streams(force && !refine); // a refine is the desktop's; the windows didn't change
    }

    fn render_frame(&mut self, refine: bool) -> Result<()> {
        // No free slot means the encoder still holds every buffer: skip this tick, stay dirty.
        let Some((mut target, age)) = self.gpu.targets.acquire(&mut self.gpu.renderer, self.gpu.fourcc)? else {
            tracing::debug!("no free swapchain slot: every frame is still with an encoder");
            return Ok(());
        };
        let age = if self.force_full_frame { 0 } else { age };

        if let (false, Target::Slot { dmabuf, .. }) = (self.gpu.modifier_verified, &target) {
            // The allocator was asked for exactly the negotiated modifier, but GBM can fall back to another
            // (or an implicit one); the encoder would then copy every frame through system memory, silently.
            self.gpu.modifier_verified = true;
            let got = dmabuf.format().modifier;
            if got == self.gpu.modifier {
                tracing::debug!(modifier = ?got, "swapchain buffer modifier matches the encoder's");
            } else {
                // The encoder was told the negotiated modifier and would read these buffers with the wrong
                // layout. Tell it the real one; if it can't take it, the pipeline fails visibly.
                tracing::warn!(?got, negotiated = ?self.gpu.modifier, "swapchain buffer modifier is not the negotiated one; rebuilding the encoders for it");
                self.gpu.modifier = got;
                for (_, sink) in &mut self.viewer_sinks {
                    sink.output_changed(self.geometry, self.gpu.fourcc as u32, u64::from(got));
                }
            }
        }
        // The pointer is drawn by the browser, so there are no custom elements.
        let elements = self.output_elements(self.geometry.scale);
        let size = (self.geometry.width_px as i32, self.geometry.height_px as i32).into();
        let texture = matches!(target, Target::Texture);
        let (sync, damaged, states, pixels) = {
            let Gpu { renderer, targets, fourcc, .. } = &mut self.gpu;
            let mut fb = targets.bind(renderer, &mut target)?;
            let res = self.damage_tracker.render_output(renderer, &mut fb, age, &elements, CLEAR)?;
            // no GPU: the pixels come back through the CPU now, while the framebuffer is bound
            let pixels = if texture && res.damage.is_some() && !self.viewer_sinks.is_empty() { Some(read_pixels(renderer, &fb, size, *fourcc)?) } else { None };
            (res.sync, res.damage.is_some(), res.states, pixels)
        };
        self.last_render = Instant::now();

        let now = self.clock.now();
        // wp_presentation: clients that asked learn when this frame went out (or that it didn't). Only
        // surfaces that were actually drawn (not occluded, not a panel hidden under a fullscreen window).
        let mut feedback = OutputPresentationFeedback::new(&self.output);
        let drawn = |s: &WlSurface, _: &SurfaceData| states.element_was_presented(Id::from_wayland_resource(s)).then(|| self.output.clone());
        // everything is composited into the swapchain: never zero-copy, no vsync, no hardware clock
        let flags = |_: &WlSurface, _: &SurfaceData| wp_presentation_feedback::Kind::empty();
        for window in self.space.elements() {
            window.take_presentation_feedback(&mut feedback, drawn, flags);
        }
        for layer in layer_map_for_output(&self.output).layers() {
            layer.take_presentation_feedback(&mut feedback, drawn, flags);
        }
        for window in self.space.elements() {
            window.send_frame(&self.output, now, Some(Duration::ZERO), |_, _| Some(self.output.clone()));
            if let Some(feedback) = &self.dmabuf_feedback {
                window.send_dmabuf_feedback(&self.output, |_, _| Some(self.output.clone()), |_, _| feedback);
            }
        }
        for layer in layer_map_for_output(&self.output).layers() {
            layer.send_frame(&self.output, now, Some(Duration::ZERO), |_, _| Some(self.output.clone()));
            if let Some(feedback) = &self.dmabuf_feedback {
                layer.send_dmabuf_feedback(&self.output, |_, _| Some(self.output.clone()), |_, _| feedback);
            }
        }
        if let CursorImageStatus::Surface(s) = &self.cursor_status {
            send_frames_surface_tree(s, &self.output, now, Some(Duration::ZERO), |_, _| Some(self.output.clone()));
        }
        if let Some((icon, _)) = &self.dnd_icon {
            send_frames_surface_tree(icon, &self.output, now, Some(Duration::ZERO), |_, _| Some(self.output.clone()));
        }

        // ponytail: CPU wait for the GPU; export the fence instead if it ever shows up in profiles.
        // Done before any early return: the next commit releases client buffers, so our reads must be over.
        while sync.wait().is_err() {} // Interrupted: wait again
        if !damaged || self.viewer_sinks.is_empty() {
            self.dirty = false;
            self.force_full_frame = false;
            feedback.discarded(); // rendered, but nothing went out
            return Ok(()); // nothing new to show (or nobody watching): don't encode
        }

        self.frame_seq += 1;
        // one frame, every viewer's encoder: a dmabuf goes to each with its own dup of the fd and a share
        // of the lease, so the slot is free again when the last of them is done with it; memory is copied
        let buffer: Box<dyn Fn() -> Result<FrameBuffer>> = match target {
            Target::Slot { slot, dmabuf } => {
                if let Targets::Dmabuf(s) = &mut self.gpu.targets {
                    s.submitted(&slot);
                }
                slot.userdata().insert_if_missing_threadsafe(|| SlotId(self.frame_seq as u32));
                let slot_id = slot.userdata().get::<SlotId>().unwrap().0;
                let (lease, modifier) = (Arc::new(slot), u64::from(self.gpu.modifier));
                Box::new(move || {
                    let fd = dmabuf.handles().next().context("dmabuf has no plane")?.as_fd().try_clone_to_owned().context("dup dmabuf fd")?;
                    Ok(FrameBuffer::Dmabuf { fd, modifier, stride: dmabuf.strides().next().unwrap(), offset: dmabuf.offsets().next().unwrap(), slot_id, lease: Box::new(lease.clone()) })
                })
            }
            Target::Texture => {
                let (data, stride) = (elsewhere_core::Bytes::from(pixels.unwrap()), self.geometry.width_px * 4); // read back above: damaged, with sinks
                Box::new(move || Ok(FrameBuffer::Memory { data: data.clone(), stride }))
            }
        };
        let now = self.clock.now(); // after the GPU wait: when the frame really left
        let (mut encoded, mut failed) = (0, false);
        for (key, sink) in &mut self.viewer_sinks {
            let submitted = buffer().and_then(|buffer| {
                sink.submit(Frame { width: self.geometry.width_px, height: self.geometry.height_px, fourcc: self.gpu.fourcc as u32, pts: now.into(), seq: self.frame_seq, refine, buffer })
                    .map_err(|e| anyhow::anyhow!("{e}"))
            });
            match submitted {
                Ok(Submit::Encoded) => encoded += 1,
                Ok(Submit::Held) => failed = true, // a rate cap: the next frame is the whole picture again
                Err(e) => {
                    failed = true;
                    tracing::warn!(key, "frame not encoded: {e}");
                }
            }
        }
        if encoded > 0 {
            self.dirty = false;
            self.refine_due = if refine { None } else { Some(Instant::now() + REFINE_AFTER) };
            // an encoder has it: the closest thing to "presented" without a display; no MSC, so seq 0
            feedback.presented(now, Refresh::Fixed(self.frame_interval), 0, wp_presentation_feedback::Kind::empty());
        } else {
            feedback.discarded();
        }
        // An encoder that got nothing gets the next frame whole: the damage tracker already advanced.
        self.force_full_frame = failed || encoded == 0;
        Ok(())
    }
}
