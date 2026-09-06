//! Clipboard bridge for the browser and the API. Text or a PNG copied in a desktop application is read
//! from its owner (a Wayland client, or an X11 client through Xwayland, whose selection code in Smithay
//! 0.7 only resolves text targets, so an X11 client's PNG isn't read) into `Event::Clipboard`; data set
//! from outside becomes a compositor-owned selection served to whoever asks. Text up to 1 MiB, images 16.

use std::{os::fd::OwnedFd, sync::Arc};

use bw_core::{Drag, Event};
use smithay::{
    backend::input::ButtonState,
    input::pointer::{ButtonEvent, GrabStartData, MotionEvent},
    reexports::{
        wayland_server::protocol::wl_data_device_manager::DndAction,
        calloop::{Interest, Mode, PostAction, RegistrationToken, generic::Generic, timer::{TimeoutAction, Timer}},
        rustix,
    },
    utils::SERIAL_COUNTER,
    wayland::selection::{
        SelectionTarget,
        data_device::{SourceMetadata, request_data_device_client_selection, set_data_device_selection, start_dnd},
    },
};

use crate::State;

/// What a compositor-owned selection holds: relayed from an X11 client, or the bytes we were given
/// (the mimes offered say what they are).
#[derive(Clone, Debug, Default)]
pub enum Selection {
    #[default]
    X11,
    Ours(Arc<Vec<u8>>),
}

pub const TEXT_MIMES: [&str; 5] = ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING", "TEXT", "STRING"];
pub const PNG: &str = "image/png";
/// Files: file managers put both on the clipboard, the second with a `copy\n` (or `cut\n`) first line.
pub const URI_LIST: &str = "text/uri-list";
pub const GNOME_FILES: &str = "x-special/gnome-copied-files";
/// A pipe nobody reads or writes for this long is closed, so a stalled peer costs nothing for good.
const DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

fn limit(mime: &str) -> usize {
    if mime == PNG { 16 << 20 } else { 1 << 20 }
}

/// The mime type to read a selection as: a URI list if it offers one (a file manager's copy offers the
/// paths as text too, and the list says more), else its text if it offers any, else a PNG.
pub fn pick_mime(mimes: &[String]) -> Option<String> {
    [URI_LIST].iter().chain(TEXT_MIMES.iter()).chain([&PNG]).find(|m| mimes.iter().any(|o| o == *m)).map(|m| m.to_string())
}

/// The read in flight, if any: its event-loop source, and a generation so a slow owner can't
/// overwrite a newer clipboard.
#[derive(Default)]
pub struct Reading {
    token: Option<RegistrationToken>,
    generation: u64,
}

impl State {
    /// Read the current clipboard (a URI list, text or a PNG, whichever `pick_mime` chose) through a pipe and report it when the owner is done writing.
    /// `x11` says the owner is an X11 client (the request goes through Xwayland). Deferred to the next
    /// loop turn so the selection this is called about is installed by then; a previous read still in
    /// flight is dropped.
    pub fn read_clipboard(&mut self, mime: String, x11: bool) {
        let generation = self.cancel_clipboard_read();
        self.handle.insert_idle(move |state| state.start_clipboard_read(mime, x11, generation));
    }

    /// Drop the read in flight; whatever it still delivers is ignored. Returns the new generation.
    fn cancel_clipboard_read(&mut self) -> u64 {
        if let Some(token) = self.reading.token.take() {
            self.handle.remove(token);
        }
        self.reading.generation += 1;
        self.reading.generation
    }

    /// Remove `token`'s source after `DEADLINE` (a no-op if it removed itself by then).
    fn expire(&self, token: RegistrationToken) {
        let _ = self.handle.insert_source(Timer::from_duration(DEADLINE), move |_, _, state| {
            state.handle.remove(token);
            TimeoutAction::Drop
        });
    }

    fn start_clipboard_read(&mut self, mime: String, x11: bool, generation: u64) {
        if generation != self.reading.generation {
            return; // superseded before it started
        }
        let Ok((read, write)) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC) else { return };
        let _ = rustix::fs::fcntl_setfl(&read, rustix::fs::OFlags::NONBLOCK);
        let requested = if x11 {
            self.xwm.as_mut().map(|xwm| xwm.send_selection(SelectionTarget::Clipboard, mime.clone(), write, self.handle.clone()).map_err(|e| format!("{e:?}"))).unwrap_or(Err("no xwm".into()))
        } else {
            request_data_device_client_selection(&self.seat, mime.clone(), write).map_err(|e| format!("{e:?}"))
        };
        if let Err(e) = requested {
            tracing::debug!("clipboard read: {e}");
            return;
        }
        let (mut data, limit) = (Vec::new(), limit(&mime));
        let source = Generic::new(read, Interest::READ, Mode::Level);
        let token = self.handle.insert_source(source, move |_, fd, state| {
            if generation != state.reading.generation {
                return Ok(PostAction::Remove); // a newer selection took over
            }
            let mut chunk = [0u8; 8192];
            loop {
                match rustix::io::read(&*fd, &mut chunk) {
                    Ok(0) => {
                        state.reading.token = None;
                        let _ = state.events.send(Event::Clipboard { mime: mime.clone(), data: std::mem::take(&mut data).into() });
                        return Ok(PostAction::Remove);
                    }
                    Ok(n) => {
                        data.extend_from_slice(&chunk[..n]);
                        if data.len() > limit {
                            tracing::debug!("clipboard read: over {limit} bytes, dropped");
                            state.reading.token = None;
                            return Ok(PostAction::Remove);
                        }
                    }
                    Err(rustix::io::Errno::AGAIN) => return Ok(PostAction::Continue),
                    Err(_) => {
                        state.reading.token = None;
                        return Ok(PostAction::Remove);
                    }
                }
            }
        });
        if let Ok(token) = token {
            self.reading.token = Some(token);
            self.expire(token);
        }
    }

    /// Text, a PNG or a file list (`text/uri-list`) from the browser or the API becomes the clipboard, offered
    /// to Wayland and X11 clients.
    pub fn set_clipboard(&mut self, mime: String, data: Vec<u8>) {
        self.cancel_clipboard_read(); // an application's older clipboard must not land after this one
        let mimes: Vec<String> = match mime.as_str() {
            PNG => vec![mime.clone()],
            URI_LIST => vec![URI_LIST.into(), GNOME_FILES.into()],
            _ => TEXT_MIMES.iter().map(|m| m.to_string()).collect(),
        };
        let _ = self.events.send(Event::Clipboard { mime: mime.clone(), data: data.clone().into() }); // the server learns of every change in order
        set_data_device_selection(&self.dh, &self.seat, mimes.clone(), Selection::Ours(Arc::new(data)));
        if let Some(xwm) = self.xwm.as_mut()
            && let Err(e) = xwm.new_selection(SelectionTarget::Clipboard, Some(mimes))
        {
            tracing::warn!("xwayland selection: {e:?}");
        }
    }

    /// The browser drags local files over the desktop. `Start` presses the left button over nothing (so no
    /// client sees a press it never gets the release of) and starts a compositor-owned drag offering
    /// `text/uri-list` from there, to copy or to move (Thunar moves the staged file into the folder
    /// shown; Nautilus copies, as GTK 4 prefers when both are offered, and so does an application that only
    /// copies, leaving the staged file to the sweep); the browser's pointer motion moves it, and the application under it is
    /// told what is coming. `Drop` supplies the list, which the files were uploaded for after the user let
    /// go, so the target only now learns what it is being given: it is left and entered again with a fresh
    /// offer (Thunar reads the list during the drag to decide, once per offer, and refuses without it;
    /// Nautilus preloads it and keeps what it read), and the button is released
    /// once it has accepted a mime and chosen an action, with a motion every 100 ms to make it look again,
    /// or after 1.5 s regardless. `Cancel` lets go over nothing; `release_all` cancels too, so a viewer
    /// that goes away or loses control mid-drag lets go.
    pub fn drag(&mut self, drag: Drag) {
        let pointer = self.seat.get_pointer().unwrap();
        let location = self.pointer_location;
        let (serial, time) = (SERIAL_COUNTER.next_serial(), self.now());
        match drag {
            Drag::Start => {
                self.drag_active = true;
                self.drag_data = None;
                self.drag_accepted = false;
                self.drag_action = DndAction::empty();
                self.drag_dropping = None;
                pointer.motion(self, None, &MotionEvent { location, serial, time });
                pointer.button(self, &ButtonEvent { button: crate::input::BTN_LEFT, state: ButtonState::Pressed, serial, time });
                pointer.frame(self);
                self.pressed_buttons.insert(crate::input::BTN_LEFT);
                let start = GrabStartData { focus: None, button: crate::input::BTN_LEFT, location };
                let source = SourceMetadata { mime_types: vec![URI_LIST.into()], dnd_action: DndAction::Copy | DndAction::Move };
                let (dh, seat) = (self.dh.clone(), self.seat.clone());
                start_dnd(&dh, &seat, self, serial, Some(start), None, source);
                self.pointer_motion(location); // enter the application under the pointer
            }
            Drag::Drop(_) if !self.drag_active => {
                // the grab ended while the files were uploading (the viewer blurred or lost control)
                let _ = self.events.send(Event::DragEnded { taken: false });
            }
            Drag::Drop(list) => {
                self.drag_data = Some(Arc::new(list));
                self.drag_dropping = Some(std::time::Instant::now() + std::time::Duration::from_millis(1500));
                // out and back in: a fresh offer, readable now (a target that asked early keeps what it read,
                // an empty list; its predecessor's late accept/action requests can't count for the new one:
                // the flags start over)
                self.drag_accepted = false;
                self.drag_action = DndAction::empty();
                pointer.motion(self, None, &MotionEvent { location, serial, time });
                self.pointer_motion(location);
                let _ = self.handle.insert_source(Timer::from_duration(std::time::Duration::from_millis(100)), |_, _, state| match state.drag_dropping {
                    None => TimeoutAction::Drop,
                    Some(deadline) if std::time::Instant::now() >= deadline => {
                        state.drag_release();
                        TimeoutAction::Drop
                    }
                    Some(_) => {
                        state.pointer_motion(state.pointer_location); // the target looks at the offer again
                        TimeoutAction::ToDuration(std::time::Duration::from_millis(100))
                    }
                });
            }
            Drag::Cancel => {
                self.drag_dropping = None;
                pointer.motion(self, None, &MotionEvent { location, serial, time }); // out of the application: the release drops on nothing
                self.drag_release();
                self.pointer_motion(location); // back in, as a plain pointer
            }
        }
    }

    /// Let go of a drop the target is ready for (`ServerDndGrabHandler::accept` and `action`). On the next
    /// loop turn: the callbacks run inside the offer's request handler, which holds the lock the drop takes.
    pub fn drag_settle(&mut self) {
        if self.drag_dropping.is_some() && self.drag_accepted && !self.drag_action.is_empty() {
            self.handle.insert_idle(|state| {
                if state.drag_dropping.is_some() && state.drag_accepted && !state.drag_action.is_empty() {
                    state.drag_release();
                }
            });
        }
    }

    /// Release the drag's button: the grab drops (`dropped`, then `cancelled` too if nothing took it).
    fn drag_release(&mut self) {
        let pointer = self.seat.get_pointer().unwrap();
        let ended = self.drag_dropping.take().is_some();
        self.drag_active = false;
        pointer.button(self, &ButtonEvent { button: crate::input::BTN_LEFT, state: ButtonState::Released, serial: SERIAL_COUNTER.next_serial(), time: self.now() });
        pointer.frame(self);
        self.pressed_buttons.remove(&crate::input::BTN_LEFT);
        if ended {
            let _ = self.events.send(Event::DragEnded { taken: self.drag_taken });
        }
    }

    /// Serve our clipboard to a client that asked for it, from the event loop as the pipe accepts it, so
    /// a reader that stalls costs a source, not a thread.
    pub fn serve_clipboard(&mut self, data: Arc<Vec<u8>>, mime: &str, fd: OwnedFd) {
        let _ = rustix::fs::fcntl_setfl(&fd, rustix::fs::OFlags::NONBLOCK);
        // a file list asked for in the file managers' own format is rewritten the way they write it: "copy",
        // then the URIs, one per line, LF only and no trailing newline (Nautilus refuses a CR or an empty line)
        let data: Arc<Vec<u8>> = if mime == GNOME_FILES {
            let list = String::from_utf8_lossy(&data);
            Arc::new(std::iter::once("copy").chain(list.lines().map(str::trim_end)).collect::<Vec<_>>().join("\n").into_bytes())
        } else {
            data
        };
        let mut written = 0;
        let token = self.handle.insert_source(Generic::new(fd, Interest::WRITE, Mode::Level), move |_, fd, _| {
            loop {
                if written >= data.len() {
                    return Ok(PostAction::Remove); // closes the pipe: end of data
                }
                match rustix::io::write(&*fd, &data[written..]) {
                    Ok(n) => written += n,
                    Err(rustix::io::Errno::AGAIN) => return Ok(PostAction::Continue),
                    Err(_) => return Ok(PostAction::Remove), // reader gone
                }
            }
        });
        if let Ok(token) = token {
            self.expire(token);
        }
    }
}
