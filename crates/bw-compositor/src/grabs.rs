//! Interactive move and resize, driven by xdg_toplevel requests.

use std::cell::RefCell;

use smithay::{
    desktop::{Space, Window, WindowSurface},
    input::{
        pointer::{
            AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent, GesturePinchBeginEvent, GesturePinchEndEvent,
            GesturePinchUpdateEvent, GestureSwipeBeginEvent, GestureSwipeEndEvent, GestureSwipeUpdateEvent, GrabStartData,
            MotionEvent, PointerGrab, PointerInnerHandle, RelativeMotionEvent,
        },
        touch::{DownEvent, GrabStartData as TouchGrabStartData, MotionEvent as TouchMotionEvent, OrientationEvent, ShapeEvent, TouchGrab, TouchInnerHandle, UpEvent},
    },
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel::{self, ResizeEdge},
        wayland_server::protocol::wl_surface::WlSurface,
    },
    utils::{Logical, Point, Rectangle, Serial, Size},
    wayland::{compositor::with_states, seat::WaylandFocus, shell::xdg::SurfaceCachedState},
};

use crate::State;

/// Everything a grab forwards unchanged.
macro_rules! forward {
    () => {
        fn relative_motion(&mut self, data: &mut State, handle: &mut PointerInnerHandle<'_, State>, focus: Option<(WlSurface, Point<f64, Logical>)>, event: &RelativeMotionEvent) {
            handle.relative_motion(data, focus, event);
        }
        fn axis(&mut self, data: &mut State, handle: &mut PointerInnerHandle<'_, State>, details: AxisFrame) {
            handle.axis(data, details)
        }
        fn frame(&mut self, data: &mut State, handle: &mut PointerInnerHandle<'_, State>) {
            handle.frame(data)
        }
        fn gesture_swipe_begin(&mut self, data: &mut State, handle: &mut PointerInnerHandle<'_, State>, event: &GestureSwipeBeginEvent) {
            handle.gesture_swipe_begin(data, event)
        }
        fn gesture_swipe_update(&mut self, data: &mut State, handle: &mut PointerInnerHandle<'_, State>, event: &GestureSwipeUpdateEvent) {
            handle.gesture_swipe_update(data, event)
        }
        fn gesture_swipe_end(&mut self, data: &mut State, handle: &mut PointerInnerHandle<'_, State>, event: &GestureSwipeEndEvent) {
            handle.gesture_swipe_end(data, event)
        }
        fn gesture_pinch_begin(&mut self, data: &mut State, handle: &mut PointerInnerHandle<'_, State>, event: &GesturePinchBeginEvent) {
            handle.gesture_pinch_begin(data, event)
        }
        fn gesture_pinch_update(&mut self, data: &mut State, handle: &mut PointerInnerHandle<'_, State>, event: &GesturePinchUpdateEvent) {
            handle.gesture_pinch_update(data, event)
        }
        fn gesture_pinch_end(&mut self, data: &mut State, handle: &mut PointerInnerHandle<'_, State>, event: &GesturePinchEndEvent) {
            handle.gesture_pinch_end(data, event)
        }
        fn gesture_hold_begin(&mut self, data: &mut State, handle: &mut PointerInnerHandle<'_, State>, event: &GestureHoldBeginEvent) {
            handle.gesture_hold_begin(data, event)
        }
        fn gesture_hold_end(&mut self, data: &mut State, handle: &mut PointerInnerHandle<'_, State>, event: &GestureHoldEndEvent) {
            handle.gesture_hold_end(data, event)
        }
        fn start_data(&self) -> &GrabStartData<State> {
            &self.start_data
        }
        fn unset(&mut self, _data: &mut State) {}
    };
}

pub struct MoveGrab {
    pub start_data: GrabStartData<State>,
    pub window: Window,
    pub initial_location: Point<i32, Logical>,
}

impl MoveGrab {
    /// The window follows the pointer (or finger) from where the grab began.
    fn drag(&self, data: &mut State, to: Point<f64, Logical>) {
        let delta = to - self.start_data.location;
        // the pointer may be anywhere, the window keeps a corner on the desktop
        let location = data.clamp_to_output(&self.window, (self.initial_location.to_f64() + delta).to_i32_round());
        data.space.map_element(self.window.clone(), location, true);
        if let Some(x11) = self.window.x11_surface() {
            let _ = x11.configure(Rectangle::new(location, self.window.geometry().size));
        }
        data.dirty = true;
    }
}

impl PointerGrab<State> for MoveGrab {
    fn motion(&mut self, data: &mut State, handle: &mut PointerInnerHandle<'_, State>, _focus: Option<(WlSurface, Point<f64, Logical>)>, event: &MotionEvent) {
        handle.motion(data, None, event); // no focus while dragging
        self.drag(data, event.location);
    }
    fn button(&mut self, data: &mut State, handle: &mut PointerInnerHandle<'_, State>, event: &ButtonEvent) {
        handle.button(data, event);
        if !handle.current_pressed().contains(&self.start_data.button) {
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }
    forward!();
}

pub struct ResizeGrab {
    pub start_data: GrabStartData<State>,
    pub window: Window,
    pub edges: ResizeEdge,
    pub initial_rect: Rectangle<i32, Logical>,
    pub last_size: Size<i32, Logical>,
}

fn has_left(e: ResizeEdge) -> bool {
    matches!(e, ResizeEdge::Left | ResizeEdge::TopLeft | ResizeEdge::BottomLeft)
}
fn has_right(e: ResizeEdge) -> bool {
    matches!(e, ResizeEdge::Right | ResizeEdge::TopRight | ResizeEdge::BottomRight)
}
fn has_top(e: ResizeEdge) -> bool {
    matches!(e, ResizeEdge::Top | ResizeEdge::TopLeft | ResizeEdge::TopRight)
}
fn has_bottom(e: ResizeEdge) -> bool {
    matches!(e, ResizeEdge::Bottom | ResizeEdge::BottomLeft | ResizeEdge::BottomRight)
}

impl ResizeGrab {
    /// The window's size follows the pointer (or finger) from where the grab began.
    fn drag(&mut self, data: &mut State, to: Point<f64, Logical>) {
        let delta = to - self.start_data.location;
        let (mut w, mut h) = (self.initial_rect.size.w as f64, self.initial_rect.size.h as f64);
        if has_left(self.edges) {
            w -= delta.x;
        } else if has_right(self.edges) {
            w += delta.x;
        }
        if has_top(self.edges) {
            h -= delta.y;
        } else if has_bottom(self.edges) {
            h += delta.y;
        }
        let (min, max) = match self.window.underlying_surface() {
            WindowSurface::Wayland(t) => with_states(t.wl_surface(), |states| {
                let mut guard = states.cached_state.get::<SurfaceCachedState>();
                let d = guard.current();
                (d.min_size, d.max_size)
            }),
            WindowSurface::X11(x) => (x.min_size().unwrap_or_default(), x.max_size().unwrap_or_default()),
        };
        let clamp = |v: f64, lo: i32, hi: i32| (v as i32).clamp(lo.max(1), if hi == 0 { i32::MAX } else { hi });
        self.last_size = (clamp(w, min.w, max.w), clamp(h, min.h, max.h)).into();
        match self.window.underlying_surface() {
            WindowSurface::Wayland(toplevel) => {
                toplevel.with_pending_state(|s| {
                    s.states.set(xdg_toplevel::State::Resizing);
                    s.size = Some(self.last_size);
                });
                toplevel.send_pending_configure();
            }
            WindowSurface::X11(x11) => {
                // anchor the fixed edge: top/left resizes move the origin
                let mut rect = Rectangle::new(self.initial_rect.loc, self.last_size);
                if has_left(self.edges) {
                    rect.loc.x = self.initial_rect.loc.x + (self.initial_rect.size.w - self.last_size.w);
                }
                if has_top(self.edges) {
                    rect.loc.y = self.initial_rect.loc.y + (self.initial_rect.size.h - self.last_size.h);
                }
                data.space.map_element(self.window.clone(), rect.loc, false);
                let _ = x11.configure(rect);
                data.dirty = true;
            }
        }
    }
    /// The resize is over: the client keeps the last size, and its next commit anchors a top/left resize.
    fn finish(&self) {
        if let Some(toplevel) = self.window.toplevel() {
            toplevel.with_pending_state(|s| {
                s.states.unset(xdg_toplevel::State::Resizing);
                s.size = Some(self.last_size);
            });
            toplevel.send_pending_configure();
            ResizeState::with(toplevel.wl_surface(), |st| *st = ResizeState::WaitingForLastCommit { edges: self.edges, initial_rect: self.initial_rect });
        }
    }
}

impl PointerGrab<State> for ResizeGrab {
    fn motion(&mut self, data: &mut State, handle: &mut PointerInnerHandle<'_, State>, _focus: Option<(WlSurface, Point<f64, Logical>)>, event: &MotionEvent) {
        handle.motion(data, None, event);
        self.drag(data, event.location);
    }
    fn button(&mut self, data: &mut State, handle: &mut PointerInnerHandle<'_, State>, event: &ButtonEvent) {
        handle.button(data, event);
        if !handle.current_pressed().contains(&self.start_data.button) {
            handle.unset_grab(self, data, event.serial, event.time, true);
            self.finish();
        }
    }
    forward!();
}

/// The same grabs from a finger on the drawn decorations: the finger that began one moves it and ends it
/// (other fingers, shapes and orientations mean nothing to it); nothing reaches a client meanwhile.
macro_rules! touch_grab {
    ($name:ident, $grab:ty, |$g:ident| $finish:expr) => {
        pub struct $name {
            pub start_data: TouchGrabStartData<State>,
            pub grab: $grab,
        }
        impl TouchGrab<State> for $name {
            fn down(&mut self, _data: &mut State, _handle: &mut TouchInnerHandle<'_, State>, _focus: Option<(WlSurface, Point<f64, Logical>)>, _event: &DownEvent, _seq: Serial) {}
            fn motion(&mut self, data: &mut State, _handle: &mut TouchInnerHandle<'_, State>, _focus: Option<(WlSurface, Point<f64, Logical>)>, event: &TouchMotionEvent, _seq: Serial) {
                if event.slot == self.start_data.slot {
                    self.grab.drag(data, event.location);
                }
            }
            fn up(&mut self, data: &mut State, handle: &mut TouchInnerHandle<'_, State>, event: &UpEvent, _seq: Serial) {
                if event.slot == self.start_data.slot {
                    handle.unset_grab(self, data);
                    let $g = &self.grab;
                    $finish;
                }
            }
            fn cancel(&mut self, data: &mut State, handle: &mut TouchInnerHandle<'_, State>, _seq: Serial) {
                handle.unset_grab(self, data);
                let $g = &self.grab;
                $finish;
            }
            fn frame(&mut self, _data: &mut State, _handle: &mut TouchInnerHandle<'_, State>, _seq: Serial) {}
            fn shape(&mut self, _data: &mut State, _handle: &mut TouchInnerHandle<'_, State>, _event: &ShapeEvent, _seq: Serial) {}
            fn orientation(&mut self, _data: &mut State, _handle: &mut TouchInnerHandle<'_, State>, _event: &OrientationEvent, _seq: Serial) {}
            fn start_data(&self) -> &TouchGrabStartData<State> {
                &self.start_data
            }
            fn unset(&mut self, _data: &mut State) {}
        }
    };
}
touch_grab!(TouchMoveGrab, MoveGrab, |_g| ());
touch_grab!(TouchResizeGrab, ResizeGrab, |g| g.finish());

/// Tracks a resize so top/left resizes can re-anchor the window when the client commits its new size.
#[derive(Default, Clone, Copy)]
pub enum ResizeState {
    #[default]
    Idle,
    Resizing { edges: ResizeEdge, initial_rect: Rectangle<i32, Logical> },
    WaitingForLastCommit { edges: ResizeEdge, initial_rect: Rectangle<i32, Logical> },
}

impl ResizeState {
    pub fn with<T>(surface: &WlSurface, f: impl FnOnce(&mut Self) -> T) -> T {
        with_states(surface, |states| {
            states.data_map.insert_if_missing(RefCell::<Self>::default);
            f(&mut states.data_map.get::<RefCell<Self>>().unwrap().borrow_mut())
        })
    }
    fn take(&mut self) -> Option<(ResizeEdge, Rectangle<i32, Logical>)> {
        match *self {
            Self::Resizing { edges, initial_rect } => Some((edges, initial_rect)),
            Self::WaitingForLastCommit { edges, initial_rect } => {
                *self = Self::Idle;
                Some((edges, initial_rect))
            }
            Self::Idle => None,
        }
    }
}

/// Called on every commit: moves a window being resized from its top/left edges.
pub fn handle_commit(space: &mut Space<Window>, surface: &WlSurface) {
    let Some(window) = space.elements().find(|w| w.wl_surface().is_some_and(|s| *s == *surface)).cloned() else { return };
    let Some(mut loc) = space.element_location(&window) else { return };
    let size = window.geometry().size;
    let Some((edges, initial)) = ResizeState::with(surface, |s| s.take()) else { return };
    if has_left(edges) {
        loc.x = initial.loc.x + (initial.size.w - size.w);
    }
    if has_top(edges) {
        loc.y = initial.loc.y + (initial.size.h - size.h);
    }
    space.map_element(window, loc, false);
}
