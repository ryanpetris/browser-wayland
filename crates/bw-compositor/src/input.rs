//! Browser input (`Command`) → seat events.

use bw_core::{AxisSource as Src, Command, OutputGeometry};
use smithay::{
    backend::input::{Axis, AxisSource, ButtonState, KeyState, Keycode},
    desktop::WindowSurfaceType,
    input::{
        keyboard::FilterResult,
        pointer::{AxisFrame, ButtonEvent, MotionEvent},
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, SERIAL_COUNTER},
};

use crate::State;

impl State {
    pub fn handle_command(&mut self, cmd: Command) {
        match cmd {
            Command::Key { evdev, pressed } => self.key(evdev, pressed),
            Command::ReleaseAllKeys => self.release_all(),
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
        let geo = self.space.output_geometry(&self.output).unwrap();
        let location = Point::from((
            location.x.clamp(0.0, (geo.size.w - 1) as f64),
            location.y.clamp(0.0, (geo.size.h - 1) as f64),
        ));
        self.pointer_location = location;
        let under = self.surface_under(location);
        let pointer = self.seat.get_pointer().unwrap();
        pointer.motion(self, under, &MotionEvent { location, serial: SERIAL_COUNTER.next_serial(), time: self.now() });
        pointer.frame(self);
        self.dirty = true; // the composited cursor moved
    }

    fn pointer_button(&mut self, button: u32, pressed: bool) {
        let pointer = self.seat.get_pointer().unwrap();
        let keyboard = self.seat.get_keyboard().unwrap();
        let serial = SERIAL_COUNTER.next_serial();
        if pressed && !pointer.is_grabbed() {
            // click-to-focus and raise
            let clicked = self.space.element_under(self.pointer_location).map(|(w, _)| w.clone());
            for window in self.space.elements() {
                window.set_activated(false);
            }
            if let Some(window) = &clicked {
                self.space.raise_element(window, true);
                keyboard.set_focus(self, Some(window.toplevel().unwrap().wl_surface().clone()), serial);
            } else {
                keyboard.set_focus(self, None, serial);
            }
            for window in self.space.elements() {
                window.toplevel().unwrap().send_pending_configure();
            }
        }
        let state = if pressed { ButtonState::Pressed } else { ButtonState::Released };
        if pressed { self.pressed_buttons.insert(button); } else { self.pressed_buttons.remove(&button); }
        pointer.button(self, &ButtonEvent { button, state, serial, time: self.now() });
        pointer.frame(self);
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
