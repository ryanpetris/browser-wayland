//! Browser input (`Command`) → seat events.

use std::time::{Duration, Instant};

use bw_core::{AxisSource as Src, Command, ControlMsg, ControlOp, Event, InputMsg, OutputGeometry, decoration::Button as DecorButton};
use smithay::{
    backend::input::{Axis, AxisSource, ButtonState, KeyState, Keycode},
    desktop::{LayerSurface, Window, WindowSurface, WindowSurfaceType, layer_map_for_output},
    input::{
        keyboard::{FilterResult, Keysym, xkb},
        pointer::{AxisFrame, ButtonEvent, CursorIcon, CursorImageStatus, Focus, GrabStartData, MotionEvent, PointerHandle, RelativeMotionEvent},
    },
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel::{self, ResizeEdge},
        wayland_server::protocol::wl_surface::WlSurface,
    },
    utils::{Logical, Point, SERIAL_COUNTER, Serial},
    wayland::{
        pointer_constraints::{PointerConstraint, with_pointer_constraint},
        seat::WaylandFocus,
        shell::wlr_layer::Layer,
    },
};

use crate::{
    State,
    decor::{Hit, Under, maximized, resize_cursor},
    desktop::window_id,
};

const BTN_LEFT: u32 = 0x110;

/// The keycode producing `sym` in `layout` and the shift level it sits on; lower levels win.
fn key_for(keymap: &xkb::Keymap, layout: u32, sym: Keysym) -> Option<(Keycode, u32)> {
    (0..4u32).find_map(|level| {
        (keymap.min_keycode().raw()..=keymap.max_keycode().raw())
            .map(Keycode::new)
            .find(|kc| keymap.key_get_syms_by_level(*kc, layout, level).contains(&sym))
            .map(|kc| (kc, level))
    })
}

/// The modifiers that select a level in the usual four-level layouts: Shift, AltGr, both.
fn level_mods(level: u32) -> &'static [Keysym] {
    match level {
        1 => &[Keysym::Shift_L],
        2 => &[Keysym::ISO_Level3_Shift],
        3 => &[Keysym::Shift_L, Keysym::ISO_Level3_Shift],
        _ => &[],
    }
}

/// `ctrl`, `Return`, `F5`, `plus`, `a`, `é`: friendly modifier names, then xkb keysym names, then single characters.
fn keysym(name: &str) -> Option<Keysym> {
    let name = match name.to_ascii_lowercase().as_str() {
        "ctrl" | "control" => "Control_L",
        "shift" => "Shift_L",
        "alt" | "opt" | "option" => "Alt_L",
        "super" | "meta" | "win" | "cmd" | "command" => "Super_L",
        "altgr" => "ISO_Level3_Shift",
        "enter" => "Return",
        "esc" => "Escape",
        "backspace" => "BackSpace",
        "del" => "Delete",
        "pageup" | "pgup" => "Prior",
        "pagedown" | "pgdn" => "Next",
        "space" => "space",
        _ => name,
    };
    let mut chars = name.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return Some(xkb::utf32_to_keysym(c as u32));
    }
    let sym = xkb::keysym_from_name(name, xkb::KEYSYM_CASE_INSENSITIVE);
    (sym.raw() != 0).then_some(sym)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_resolve_in_us_layout() {
        let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(&ctx, "", "", "us", "", None, xkb::KEYMAP_COMPILE_NO_FLAGS).unwrap();
        let (a, level) = key_for(&keymap, 0, keysym("a").unwrap()).unwrap();
        assert_eq!((a.raw() - 8, level), (30, 0)); // KEY_A
        assert_eq!(key_for(&keymap, 0, keysym("A").unwrap()).unwrap(), (a, 1));
        assert_eq!(key_for(&keymap, 0, keysym("ctrl").unwrap()).unwrap().0.raw() - 8, 29); // KEY_LEFTCTRL
        assert_eq!(key_for(&keymap, 0, keysym("Return").unwrap()).unwrap().0.raw() - 8, 28);
        assert_eq!(key_for(&keymap, 0, keysym("F5").unwrap()).unwrap().0.raw() - 8, 63);
        assert_eq!(key_for(&keymap, 0, keysym("plus").unwrap()).unwrap().1, 1); // Shift+=
        assert!(key_for(&keymap, 0, keysym("é").unwrap()).is_none()); // not on a US keyboard
        assert!(keysym("nonsense_key").is_none());
    }

    #[test]
    fn altgr_level_in_german_layout() {
        let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(&ctx, "", "", "de", "", None, xkb::KEYMAP_COMPILE_NO_FLAGS).unwrap();
        let (q, _) = key_for(&keymap, 0, keysym("q").unwrap()).unwrap();
        assert_eq!(key_for(&keymap, 0, keysym("@").unwrap()).unwrap(), (q, 2)); // AltGr+q
        assert_eq!(level_mods(2), &[Keysym::ISO_Level3_Shift]);
    }
}

impl State {
    pub fn handle_command(&mut self, cmd: Command) {
        match cmd {
            Command::Key { evdev, pressed } => {
                self.key(evdev, pressed);
            }
            Command::Input(msg) => self.input(msg),
            Command::SetClipboard(text) => self.set_clipboard(text),
            Command::ReleaseAllInput => self.release_all(),
            Command::PointerMotionAbsolute { x, y } => self.pointer_motion((x, y).into()),
            Command::PointerMotionRelative { dx, dy } => self.pointer_motion(self.pointer_location + Point::<f64, Logical>::from((dx, dy))),
            Command::PointerButton { button, pressed } => self.pointer_button(button, pressed),
            Command::PointerAxis { source, dx, dy, v120 } => self.pointer_axis(source, dx, dy, v120),
            Command::Resize(geo) => self.resize(geo),
            Command::ViewerStream { key, sink: Some(sink) } => self.start_viewer_stream(key, sink),
            Command::ViewerStream { key, sink: None } => self.stop_viewer_stream(key),
            Command::RequestFullFrame => self.force_full_frame = true,
            Command::ReleasePointerLock => self.release_pointer_lock(),
            Command::Control(msg) => self.control(msg),
            Command::Snapshot { id, scale, reply } => (reply.0)(self.snapshot(id, scale)),
            Command::WindowStream { key, window, sink: Some(sink) } => self.start_window_stream(key, window, sink),
            Command::WindowStream { key, sink: None, .. } => self.stop_window_stream(key),
            Command::Quit => self.running = false,
        }
    }

    fn now(&self) -> u32 {
        self.clock.now().as_millis()
    }

    /// Returns whether the event was sent: a press for a held key (browser auto-repeat) or a stray release is dropped.
    fn key(&mut self, evdev: u32, pressed: bool) -> bool {
        let keyboard = self.seat.get_keyboard().unwrap();
        let keycode = Keycode::new(evdev + 8); // xkb keycodes are evdev + 8
        if pressed == keyboard.pressed_keys().contains(&keycode) {
            return false; // clients repeat keys themselves
        }
        let state = if pressed { KeyState::Pressed } else { KeyState::Released };
        keyboard.input::<(), _>(self, keycode, state, SERIAL_COUNTER.next_serial(), self.now(), |_, _, _| FilterResult::Forward);
        true
    }

    /// API/MCP input. Window-relative coordinates use the window's geometry as it is now, and a click's
    /// motion and buttons go out together, so nothing can slip in between.
    fn input(&mut self, msg: InputMsg) {
        let at = |st: &Self, x: f64, y: f64, window: Option<u64>| -> Option<Point<f64, Logical>> {
            match window {
                Some(id) => {
                    let geo = st.space.element_geometry(&st.window_by_id(id)?)?; // None while minimized
                    Some((geo.loc.x as f64 + x, geo.loc.y as f64 + y).into())
                }
                None => Some((x, y).into()),
            }
        };
        match msg {
            InputMsg::Move { x, y, window } => {
                if let Some(p) = at(self, x, y, window) {
                    self.pointer_motion(p);
                }
            }
            InputMsg::Click { x, y, window, button, count } => {
                if let Some(p) = at(self, x, y, window) {
                    self.pointer_motion(p);
                    for _ in 0..count.unwrap_or(1).clamp(1, 3) {
                        self.pointer_button(button.code(), true);
                        self.pointer_button(button.code(), false);
                    }
                }
            }
            InputMsg::Button { button, pressed } => self.pointer_button(button.code(), pressed),
            InputMsg::Scroll { dx, dy } => self.handle_command(Command::wheel(dx, dy)),
            InputMsg::Key { keys } => self.chord(keys.split('+').map(str::trim).filter(|k| !k.is_empty())),
            InputMsg::Text { text } => self.type_text(&text),
        }
    }

    fn type_text(&mut self, text: &str) {
        for ch in text.chars() {
            let sym = match ch {
                '\n' => Keysym::Return,
                '\t' => Keysym::Tab,
                c => xkb::utf32_to_keysym(c as u32),
            };
            if !self.tap(&[sym]) {
                tracing::warn!(%ch, "the keyboard layout can't type this character");
            }
        }
    }

    /// `ctrl+shift+t`: friendly modifier names, keysym names, single characters. A lone letter means the
    /// key, not Shift+key, whatever its case.
    fn chord<'a>(&mut self, keys: impl Iterator<Item = &'a str>) {
        let mut syms = Vec::new();
        for k in keys {
            let k = if k.len() == 1 && k.is_ascii() { k.to_ascii_lowercase() } else { k.to_string() };
            match keysym(&k) {
                Some(s) => syms.push(s),
                None => {
                    tracing::warn!(key = k, "unknown key name");
                    return;
                }
            }
        }
        if !self.tap(&syms) {
            tracing::warn!("the keyboard layout has no key for part of the chord");
        }
    }

    /// Press `syms` together (the modifiers their levels need first) and release them in reverse; keys a
    /// viewer already holds stay held. False if some keysym has no key in the active layout.
    fn tap(&mut self, syms: &[Keysym]) -> bool {
        let keyboard = self.seat.get_keyboard().unwrap();
        let keys: Option<Vec<Keycode>> = keyboard.with_xkb_state(self, |ctx| {
            let xkb = ctx.xkb().lock().unwrap();
            // Safety: the keymap reference doesn't outlive the lock.
            let keymap = unsafe { xkb.keymap() };
            let layout = xkb.active_layout().0;
            let resolved = syms.iter().map(|s| key_for(keymap, layout, *s)).collect::<Option<Vec<_>>>()?;
            let mut keys = Vec::new();
            for m in resolved.iter().flat_map(|(_, level)| level_mods(*level)) {
                let (k, _) = key_for(keymap, layout, *m)?;
                if !keys.contains(&k) {
                    keys.push(k);
                }
            }
            keys.extend(resolved.into_iter().map(|(k, _)| k));
            Some(keys)
        });
        let Some(keys) = keys else { return false };
        let pressed: Vec<Keycode> = keys.iter().copied().filter(|k| self.key(k.raw() - 8, true)).collect();
        for k in pressed.iter().rev() {
            self.key(k.raw() - 8, false);
        }
        true
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

    pub(crate) fn pointer_motion(&mut self, location: Point<f64, Logical>) {
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
        if under.is_none() && !pointer.is_grabbed() {
            // over our decorations or the bare desktop: the cursor is ours to set
            let icon = match self.decoration_under(location) {
                Some((_, Hit::Edge(edge))) => resize_cursor(edge),
                _ => CursorIcon::Default,
            };
            if self.cursor_status != CursorImageStatus::Named(icon) {
                self.cursor_status = CursorImageStatus::Named(icon);
                self.export_cursor();
            }
        }
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
        if !pressed && button == BTN_LEFT && let Some((window, b)) = self.decor_press.take() {
            // a decoration button acts on release, if the pointer is still on it
            if self.decoration_under(self.pointer_location).is_some_and(|(w, h)| w == window && h == Hit::Button(b)) {
                let op = match b {
                    DecorButton::Close => ControlOp::Close,
                    DecorButton::Minimize => ControlOp::Minimize,
                    DecorButton::Maximize if maximized(&window) => ControlOp::Unmaximize,
                    DecorButton::Maximize => ControlOp::Maximize,
                };
                self.control(ControlMsg { id: window_id(&window), op });
            }
        }
        if pressed && !pointer.is_grabbed() {
            if let Some((layer, _, _)) = self.layer_under(self.pointer_location, true) {
                // a panel: the windows under it stay where they are; it gets the keyboard only if it asked
                if layer.can_receive_keyboard_focus() {
                    self.focus_window(None, serial);
                    keyboard.set_focus(self, Some(layer.wl_surface().clone()), serial);
                }
            } else if let Some((window, hit)) = self.decoration_under(self.pointer_location) {
                // our title bar (a higher window's surfaces, resize handles and popups included, would have won):
                // focus, then a button, a drag (or a double-click), or a resize from the band
                self.focus_window(Some(&window), serial);
                match hit {
                    Hit::Button(b) if button == BTN_LEFT => self.decor_press = Some((window, b)),
                    Hit::Bar if button == BTN_LEFT => {
                        let now = Instant::now();
                        let again = self.bar_click.take().is_some_and(|(w, at)| w == window && now.duration_since(at) < Duration::from_millis(400));
                        if again {
                            self.fill(&window, xdg_toplevel::State::Maximized, !maximized(&window));
                        } else {
                            self.bar_click = Some((window.clone(), now));
                        }
                        if !again && !maximized(&window) {
                            let initial_location = self.space.element_location(&window).unwrap();
                            let start_data = GrabStartData { focus: None, button, location: self.pointer_location };
                            pointer.set_grab(self, crate::grabs::MoveGrab { start_data, window, initial_location }, serial, Focus::Clear);
                        }
                    }
                    Hit::Edge(edges) if button == BTN_LEFT => self.start_resize(&window, edges, button, serial),
                    _ => {}
                }
            } else {
                // Super/Alt + left drag moves any window, decorated or not.
                let mods = keyboard.modifier_state();
                if button == BTN_LEFT && (mods.logo || mods.alt) {
                    let draggable = |w: &Window| match w.underlying_surface() {
                        WindowSurface::X11(x) => !(x.is_override_redirect() || x.is_maximized() || x.is_fullscreen()),
                        WindowSurface::Wayland(t) => {
                            use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State as S;
                            let st = t.current_state().states;
                            !(st.contains(S::Maximized) || st.contains(S::Fullscreen))
                        }
                    };
                    if let Some((window, loc)) = self.space.element_under(self.pointer_location).filter(|(w, _)| draggable(w)).map(|(w, l)| (w.clone(), l)) {
                        self.space.raise_element(&window, true);
                        let start_data = GrabStartData { focus: None, button, location: self.pointer_location };
                        let grab = crate::grabs::MoveGrab { start_data, window, initial_location: loc };
                        pointer.set_grab(self, grab, serial, Focus::Clear);
                        // fall through: the press goes to the grab (which sends nothing) and keeps the pressed set right
                    }
                }
                if !pointer.is_grabbed() {
                    // click-to-focus and raise
                    let clicked = self.space.element_under(self.pointer_location).map(|(w, _)| w.clone());
                    self.focus_window(clicked.as_ref(), serial);
                    if clicked.is_none() {
                        // empty desktop: a bottom/background layer may want the keyboard (on-demand panels)
                        if let Some((layer, _, _)) = self.layer_under(self.pointer_location, false).filter(|(l, _, _)| l.can_receive_keyboard_focus()) {
                            keyboard.set_focus(self, Some(layer.wl_surface().clone()), serial);
                        }
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

    /// A resize from our decoration band, like an xdg resize request but started by us.
    fn start_resize(&mut self, window: &Window, edges: ResizeEdge, button: u32, serial: Serial) {
        let pointer = self.seat.get_pointer().unwrap();
        let mut initial_rect = window.geometry();
        initial_rect.loc = self.space.element_location(window).unwrap();
        if let Some(toplevel) = window.toplevel() {
            crate::grabs::ResizeState::with(toplevel.wl_surface(), |s| *s = crate::grabs::ResizeState::Resizing { edges, initial_rect });
            toplevel.with_pending_state(|s| s.states.set(xdg_toplevel::State::Resizing));
            toplevel.send_pending_configure();
        }
        let start_data = GrabStartData { focus: None, button, location: self.pointer_location };
        let grab = crate::grabs::ResizeGrab { start_data, window: window.clone(), edges, initial_rect, last_size: initial_rect.size };
        pointer.set_grab(self, grab, serial, Focus::Clear);
    }

    /// Raise and activate `window` (none: just deactivate everything) and give it the keyboard.
    pub fn focus_window(&mut self, window: Option<&Window>, serial: Serial) {
        self.active = window.cloned();
        self.dirty = true; // the bars follow the focus
        let keyboard = self.seat.get_keyboard().unwrap();
        for w in self.space.elements() {
            w.set_activated(false);
        }
        if let Some(window) = window {
            self.space.raise_element(window, true);
            if let Some(x11) = window.x11_surface().filter(|x| !x.is_override_redirect()) {
                if let Some(xwm) = self.xwm.as_mut() {
                    let _ = xwm.raise_window(x11);
                }
            }
        }
        keyboard.set_focus(self, window.and_then(|w| w.wl_surface().map(|s| s.into_owned())), serial);
        for w in self.space.elements() {
            match w.underlying_surface() {
                WindowSurface::Wayland(t) => {
                    t.send_pending_configure();
                }
                WindowSurface::X11(x) => {
                    let _ = x.set_activated(Some(w) == window);
                }
            }
        }
    }

    /// Layer surface under `pos` on the layers drawn above the windows (Overlay, Top) or below them (Bottom, Background).
    fn layer_under(&self, pos: Point<f64, Logical>, above: bool) -> Option<(LayerSurface, WlSurface, Point<f64, Logical>)> {
        let layers = layer_map_for_output(&self.output);
        let (a, b) = if above { (Layer::Overlay, Layer::Top) } else { (Layer::Bottom, Layer::Background) };
        let hidden_top = above && self.fullscreen_window_mapped(); // panels are under a fullscreen window
        // top-most first; a surface with an empty input region (OSDs) lets the point fall through to the next
        layers.layers_on(a).rev().chain(layers.layers_on(b).rev()).filter(|l| !(hidden_top && l.layer() == Layer::Top)).find_map(|layer| {
            let loc = layers.layer_geometry(layer)?.loc;
            let (surface, p) = layer.surface_under(pos - loc.to_f64(), WindowSurfaceType::ALL)?;
            Some((layer.clone(), surface, (p + loc).to_f64()))
        })
    }

    pub fn surface_under(&self, pos: Point<f64, Logical>) -> Option<(WlSurface, Point<f64, Logical>)> {
        if let Some((_, surface, loc)) = self.layer_under(pos, true) {
            return Some((surface, loc));
        }
        match self.window_under(pos) {
            Some(Under::Surface(surface, p)) => Some((surface, p)),
            Some(Under::Decoration(..)) => None, // our chrome: nothing of the clients' is under the pointer
            None => self.layer_under(pos, false).map(|(_, s, p)| (s, p)),
        }
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
