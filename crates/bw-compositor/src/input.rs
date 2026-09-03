//! Browser input (`Command`) → seat events.

use bw_core::{AxisSource as Src, Command, Event, OutputGeometry};
use smithay::{
    backend::input::{Axis, AxisSource, ButtonState, KeyState, Keycode},
    desktop::{WindowSurface, WindowSurfaceType},
    input::{
        keyboard::FilterResult,
        pointer::{AxisFrame, ButtonEvent, Focus, GrabStartData, MotionEvent, PointerHandle, RelativeMotionEvent},
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, SERIAL_COUNTER},
    wayland::{pointer_constraints::{PointerConstraint, with_pointer_constraint}, seat::WaylandFocus},
};

use crate::State;

const BTN_LEFT: u32 = 0x110;

impl State {
    pub fn handle_command(&mut self, cmd: Command) {
        match cmd {
            Command::Key { evdev, pressed } => self.key(evdev, pressed),
            Command::ReleaseAllInput => self.release_all(),
            Command::PointerMotionAbsolute { x, y } => self.pointer_motion((x, y).into()),
            Command::PointerMotionRelative { dx, dy } => self.pointer_motion(self.pointer_location + Point::<f64, Logical>::from((dx, dy))),
            Command::PointerButton { button, pressed } => self.pointer_button(button, pressed),
            Command::PointerAxis { source, dx, dy, v120 } => self.pointer_axis(source, dx, dy, v120),
            Command::Resize(geo) => self.resize(geo),
            Command::ViewerConnected => {
                self.viewer_connected = true;
                self.force_full_frame = true;
            }
            Command::ViewerDisconnected => self.viewer_connected = false,
            Command::RequestFullFrame => self.force_full_frame = true,
            Command::ReleasePointerLock => self.release_pointer_lock(),
            Command::Quit => self.running = false,
        }
    }

    fn now(&self) -> u32 {
        self.clock.now().as_millis()
    }

    fn key(&mut self, evdev: u32, pressed: bool) {
        let keyboard = self.seat.get_keyboard().unwrap();
        let keycode = Keycode::new(evdev + 8); // xkb keycodes are evdev + 8
        if pressed == keyboard.pressed_keys().contains(&keycode) {
            return; // browser auto-repeat or a stray release; clients repeat keys themselves
        }
        let state = if pressed { KeyState::Pressed } else { KeyState::Released };
        keyboard.input::<(), _>(self, keycode, state, SERIAL_COUNTER.next_serial(), self.now(), |_, _, _| FilterResult::Forward);
    }

    fn release_all(&mut self) {
        let keyboard = self.seat.get_keyboard().unwrap();
        for key in keyboard.pressed_keys() {
            keyboard.input::<(), _>(self, key, KeyState::Released, SERIAL_COUNTER.next_serial(), self.now(), |_, _, _| FilterResult::Forward);
        }
        let pointer = self.seat.get_pointer().unwrap();
        for button in std::mem::take(&mut self.pressed_buttons) {
            pointer.button(self, &ButtonEvent { button, state: ButtonState::Released, serial: SERIAL_COUNTER.next_serial(), time: self.now() });
        }
        pointer.frame(self);
    }

    fn pointer_motion(&mut self, location: Point<f64, Logical>) {
        let pointer = self.seat.get_pointer().unwrap();
        let delta = location - self.pointer_location;
        let relative = RelativeMotionEvent { delta, delta_unaccel: delta, utime: self.clock.now().as_micros() };
        if self.locked(&pointer) {
            // Locked: the pointer stays put and the client only gets deltas.
            let under = self.surface_under(self.pointer_location);
            pointer.relative_motion(self, under, &relative);
            pointer.frame(self);
            return;
        }
        let geo = self.space.output_geometry(&self.output).unwrap();
        let location = Point::from((
            location.x.clamp(0.0, (geo.size.w - 1) as f64),
            location.y.clamp(0.0, (geo.size.h - 1) as f64),
        ));
        self.pointer_location = location;
        let under = self.surface_under(location);
        pointer.motion(self, under.clone(), &MotionEvent { location, serial: SERIAL_COUNTER.next_serial(), time: self.now() });
        pointer.relative_motion(self, under.clone(), &relative);
        pointer.frame(self);
        // entering a surface with a pending lock activates it (unless the browser just bailed out of one)
        if let (false, Some((surface, origin))) = (self.lock_suppressed, under) {
            activate_lock(&surface, &pointer, location - origin);
        }
        self.sync_pointer_lock(&pointer);
    }

    /// The browser lost its pointer lock: release the client's, and stay unlocked until the next click.
    fn release_pointer_lock(&mut self) {
        let pointer = self.seat.get_pointer().unwrap();
        if let Some(surface) = pointer.current_focus() {
            with_pointer_constraint(&surface, &pointer, |c| {
                if let Some(c) = c.filter(|c| c.is_active()) {
                    c.deactivate();
                }
            });
        }
        self.lock_suppressed = true;
        self.sync_pointer_lock(&pointer);
    }

    fn locked(&self, pointer: &PointerHandle<State>) -> bool {
        pointer.current_focus().is_some_and(|surface| {
            with_pointer_constraint(&surface, pointer, |c| {
                c.is_some_and(|c| c.is_active() && matches!(*c, PointerConstraint::Locked(_)))
            })
        })
    }

    /// Tell the browser when a lock starts or ends so it can mirror it with the Pointer Lock API.
    pub fn sync_pointer_lock(&mut self, pointer: &PointerHandle<State>) {
        let locked = self.locked(pointer);
        if locked != self.pointer_locked {
            self.pointer_locked = locked;
            let _ = self.events.send(Event::PointerLock(locked));
        }
    }

    fn pointer_button(&mut self, button: u32, pressed: bool) {
        let pointer = self.seat.get_pointer().unwrap();
        let keyboard = self.seat.get_keyboard().unwrap();
        let serial = SERIAL_COUNTER.next_serial();
        if pressed {
            self.lock_suppressed = false; // a click is the user gesture the browser needs to lock again
        }
        // Super/Alt + left drag moves any window, decorated or not (X11 apps have no title bar here).
        let mods = keyboard.modifier_state();
        if pressed && button == BTN_LEFT && (mods.logo || mods.alt) && !pointer.is_grabbed() {
            if let Some((window, loc)) = self.space.element_under(self.pointer_location).map(|(w, l)| (w.clone(), l)) {
                self.space.raise_element(&window, true);
                let start_data = GrabStartData { focus: None, button, location: self.pointer_location };
                let grab = crate::grabs::MoveGrab { start_data, window, initial_location: loc };
                self.pressed_buttons.insert(button);
                pointer.set_grab(self, grab, serial, Focus::Clear);
                return;
            }
        }
        if pressed && !pointer.is_grabbed() {
            // click-to-focus and raise
            let clicked = self.space.element_under(self.pointer_location).map(|(w, _)| w.clone());
            for window in self.space.elements() {
                window.set_activated(false);
            }
            if let Some(window) = &clicked {
                self.space.raise_element(window, true);
                if let Some(x11) = window.x11_surface() {
                    if let Some(xwm) = self.xwm.as_mut() {
                        let _ = xwm.raise_window(x11);
                    }
                }
                keyboard.set_focus(self, window.wl_surface().map(|s| s.into_owned()), serial);
            } else {
                keyboard.set_focus(self, None, serial);
            }
            for window in self.space.elements() {
                match window.underlying_surface() {
                    WindowSurface::Wayland(t) => {
                        t.send_pending_configure();
                    }
                    WindowSurface::X11(x) => {
                        let _ = x.set_activated(clicked.as_ref() == Some(window));
                    }
                }
            }
        }
        let state = if pressed { ButtonState::Pressed } else { ButtonState::Released };
        if pressed { self.pressed_buttons.insert(button); } else { self.pressed_buttons.remove(&button); }
        pointer.button(self, &ButtonEvent { button, state, serial, time: self.now() });
        pointer.frame(self);
        self.sync_pointer_lock(&pointer);
    }

    fn pointer_axis(&mut self, source: Src, dx: f64, dy: f64, v120: Option<(i32, i32)>) {
        let mut frame = AxisFrame::new(self.now()).source(match source {
            Src::Wheel => AxisSource::Wheel,
            Src::Finger => AxisSource::Finger,
        });
        for (axis, value, steps) in [(Axis::Horizontal, dx, v120.map(|v| v.0)), (Axis::Vertical, dy, v120.map(|v| v.1))] {
            if value != 0.0 {
                frame = frame.value(axis, value);
                if let Some(steps) = steps.filter(|s| *s != 0) {
                    frame = frame.v120(axis, steps);
                }
            }
        }
        let pointer = self.seat.get_pointer().unwrap();
        pointer.axis(self, frame);
        pointer.frame(self);
    }

    pub fn surface_under(&self, pos: Point<f64, Logical>) -> Option<(WlSurface, Point<f64, Logical>)> {
        self.space.element_under(pos).and_then(|(window, location)| {
            window
                .surface_under(pos - location.to_f64(), WindowSurfaceType::ALL)
                .map(|(s, p)| (s, (p + location).to_f64()))
        })
    }

    pub fn output_geometry(&self) -> OutputGeometry {
        self.geometry
    }
}

/// Activate a pending *lock* whose region contains the pointer. Confinement is deliberately never
/// activated: the browser can't confine its pointer, so the client keeps seeing an unconfined one.
pub fn activate_lock(surface: &WlSurface, pointer: &PointerHandle<State>, local: Point<f64, Logical>) {
    with_pointer_constraint(surface, pointer, |c| {
        if let Some(c) = c.filter(|c| !c.is_active() && matches!(**c, PointerConstraint::Locked(_))) {
            if c.region().is_none_or(|r| r.contains(local.to_i32_round())) {
                c.activate();
            }
        }
    });
}
