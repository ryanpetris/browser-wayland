use std::{os::fd::AsFd, time::{Duration, Instant}};

use anyhow::{Context, Result};
use bw_core::DmabufFrame;
use smithay::{
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    wayland::compositor::SurfaceData,
    reexports::wayland_protocols::xdg::shell::server::xdg_toplevel,
    desktop::Window,
    utils::{Logical, Point},
    backend::allocator::Buffer as _,
    backend::renderer::{Bind, utils::with_renderer_surface_state, element::{AsRenderElements, Id, surface::WaylandSurfaceRenderElement}, gles::GlesRenderer},
    desktop::{
        WindowSurface, layer_map_for_output,
        utils::{OutputPresentationFeedback, send_frames_surface_tree, surface_presentation_feedback_flags_from_states},
    },
    reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback,
    wayland::presentation::Refresh,
    utils::Scale,
    wayland::shell::wlr_layer::Layer,
    input::pointer::CursorImageStatus,
};

use crate::State;

pub const CLEAR: [f32; 4] = [0.12, 0.12, 0.14, 1.0];

/// Per-swapchain-slot id so the encoder can cache its import.
struct SlotId(u32);

impl State {
    /// A mapped window is fullscreen: it covers the panels (Top layer), only the Overlay layer stays above.
    pub fn fullscreen_window_mapped(&self) -> bool {
        self.space.elements().any(|w| match w.underlying_surface() {
            // a toplevel that unmapped with a null buffer keeps its state and stays in the space until destroyed
            WindowSurface::Wayland(t) => {
                t.current_state().states.contains(xdg_toplevel::State::Fullscreen) && with_renderer_surface_state(t.wl_surface(), |s| s.buffer().is_some()).unwrap_or(false)
            }
            WindowSurface::X11(x) => x.is_fullscreen(),
        })
    }

    /// Everything on the output, front to back, at `scale` physical pixels per logical pixel: the overlay
    /// (and, unless a window is fullscreen, top) layers, the windows with their popups, then the bottom
    /// and background layers.
    pub fn output_elements(&mut self, scale: f64) -> Vec<WaylandSurfaceRenderElement<GlesRenderer>> {
        let s = Scale::from(scale);
        let output_loc = self.space.output_geometry(&self.output).map(|g| g.loc).unwrap_or_default();
        let fullscreen = self.fullscreen_window_mapped();
        let layers = layer_map_for_output(&self.output);
        let layer_elements = |renderer: &mut GlesRenderer, pick: fn(Layer) -> bool| -> Vec<WaylandSurfaceRenderElement<GlesRenderer>> {
            layers
                .layers()
                .rev()
                .filter(|l| pick(l.layer()))
                .filter_map(|l| layers.layer_geometry(l).map(|g| (g.loc, l)))
                .flat_map(|(loc, l)| l.render_elements::<WaylandSurfaceRenderElement<GlesRenderer>>(renderer, loc.to_physical_precise_round(scale), s, 1.0))
                .collect()
        };
        let mut out = layer_elements(&mut self.gpu.renderer, if fullscreen { |l| l == Layer::Overlay } else { |l| matches!(l, Layer::Top | Layer::Overlay) });
        let windows: Vec<(Window, Point<i32, Logical>)> = self.space.elements().rev().filter_map(|w| Some((w.clone(), self.space.element_location(w)?))).collect();
        for (w, loc) in windows {
            // the space places the geometry; render_elements wants the surface origin
            let origin = loc - w.geometry().loc - output_loc;
            out.extend(w.render_elements::<WaylandSurfaceRenderElement<GlesRenderer>>(&mut self.gpu.renderer, origin.to_physical_precise_round(scale), s, 1.0));
        }
        out.extend(layer_elements(&mut self.gpu.renderer, |l| matches!(l, Layer::Bottom | Layer::Background)));
        out
    }

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
        if !self.gpu.modifier_verified {
            // The allocator was asked for exactly the negotiated modifier, but GBM can fall back to another
            // (or an implicit one); the encoder would then copy every frame through system memory, silently.
            self.gpu.modifier_verified = true;
            let got = dmabuf.format().modifier;
            if got == self.gpu.modifier {
                tracing::debug!(modifier = ?got, "swapchain buffer modifier matches the encoder's");
            } else {
                // The encoder was told the negotiated modifier and would read these buffers with the wrong
                // layout. Tell it the real one; if it can't take it, the pipeline fails visibly.
                tracing::warn!(?got, negotiated = ?self.gpu.modifier, "swapchain buffer modifier is not the negotiated one; rebuilding the encoder for it");
                self.gpu.modifier = got;
                self.sink.output_changed(self.geometry, self.gpu.fourcc as u32, u64::from(got));
            }
        }
        // The pointer is drawn by the browser, so there are no custom elements.
        let elements = self.output_elements(self.geometry.scale);
        let (sync, damaged, states) = {
            let mut fb = self.gpu.renderer.bind(&mut dmabuf)?;
            let res = self.damage_tracker.render_output(&mut self.gpu.renderer, &mut fb, age, &elements, CLEAR)?;
            (res.sync, res.damage.is_some(), res.states)
        };
        self.last_render = Instant::now();

        let now = self.clock.now();
        // wp_presentation: clients that asked learn when this frame went out (or that it didn't). Only
        // surfaces that were actually drawn (not occluded, not a panel hidden under a fullscreen window).
        let mut feedback = OutputPresentationFeedback::new(&self.output);
        let drawn = |s: &WlSurface, _: &SurfaceData| states.element_was_presented(Id::from_wayland_resource(s)).then(|| self.output.clone());
        let flags = |s: &WlSurface, _: &SurfaceData| surface_presentation_feedback_flags_from_states(s, &states);
        for window in self.space.elements() {
            window.take_presentation_feedback(&mut feedback, drawn, flags);
        }
        for layer in layer_map_for_output(&self.output).layers() {
            layer.take_presentation_feedback(&mut feedback, drawn, flags);
        }
        for window in self.space.elements() {
            window.send_frame(&self.output, now, Some(Duration::ZERO), |_, _| Some(self.output.clone()));
            window.send_dmabuf_feedback(&self.output, |_, _| Some(self.output.clone()), |_, _| &self.dmabuf_feedback);
        }
        for layer in layer_map_for_output(&self.output).layers() {
            layer.send_frame(&self.output, now, Some(Duration::ZERO), |_, _| Some(self.output.clone()));
            layer.send_dmabuf_feedback(&self.output, |_, _| Some(self.output.clone()), |_, _| &self.dmabuf_feedback);
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
            feedback.discarded(); // rendered, but nothing went out
            return Ok(()); // nothing new to show (or nobody watching): don't encode
        }

        self.gpu.swapchain.submitted(&slot);
        slot.userdata().insert_if_missing_threadsafe(|| SlotId(self.frame_seq as u32));
        let slot_id = slot.userdata().get::<SlotId>().unwrap().0;
        self.frame_seq += 1;

        let fd = dmabuf.handles().next().context("dmabuf has no plane").and_then(|fd| fd.as_fd().try_clone_to_owned().context("dup dmabuf fd"));
        let fd = match fd {
            Ok(fd) => fd,
            Err(e) => {
                feedback.discarded();
                return Err(e);
            }
        };
        let now = self.clock.now(); // after the GPU wait: when the frame really left
        let submitted = self.sink.submit(DmabufFrame {
            fd,
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
                // the encoder has it: the closest thing to "presented" without a display; no MSC, so seq 0
                feedback.presented(now, Refresh::Fixed(self.frame_interval), 0, wp_presentation_feedback::Kind::empty());
            }
            Err(e) => {
                // The damage tracker already advanced, so the retry must redraw everything.
                tracing::warn!("frame not encoded: {e}");
                self.force_full_frame = true;
                feedback.discarded();
            }
        }
        Ok(())
    }
}
