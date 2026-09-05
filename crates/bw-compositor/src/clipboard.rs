//! Clipboard bridge for the browser and the API. Text or a PNG copied in a desktop application is read
//! from its owner (a Wayland client, or an X11 client through Xwayland) into `Event::Clipboard`; data set
//! from outside becomes a compositor-owned selection served to whoever asks. Text up to 1 MiB, images 16.

use std::{os::fd::OwnedFd, sync::Arc};

use bw_core::Event;
use smithay::{
    reexports::{
        calloop::{Interest, Mode, PostAction, RegistrationToken, generic::Generic, timer::{TimeoutAction, Timer}},
        rustix,
    },
    wayland::selection::{SelectionTarget, data_device::{request_data_device_client_selection, set_data_device_selection}},
};

use crate::State;

/// What a compositor-owned selection holds: relayed from an X11 client, or data we were given.
#[derive(Clone, Debug, Default)]
pub enum Selection {
    #[default]
    X11,
    Ours(Arc<Ours>),
}

/// Clipboard contents from the browser or the API.
#[derive(Debug)]
pub struct Ours {
    pub mime: String,
    pub data: Vec<u8>,
}

pub const TEXT_MIMES: [&str; 5] = ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING", "TEXT", "STRING"];
pub const PNG: &str = "image/png";
/// A pipe nobody reads or writes for this long is closed, so a stalled peer costs nothing for good.
const DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

fn limit(mime: &str) -> usize {
    if mime == PNG { 16 << 20 } else { 1 << 20 }
}

/// The mime type to read a selection as: its text if it offers any, else a PNG.
pub fn pick_mime(mimes: &[String]) -> Option<String> {
    TEXT_MIMES.iter().chain([&PNG]).find(|m| mimes.iter().any(|o| o == *m)).map(|m| m.to_string())
}

/// The read in flight, if any: its event-loop source, and a generation so a slow owner can't
/// overwrite a newer clipboard.
#[derive(Default)]
pub struct Reading {
    token: Option<RegistrationToken>,
    generation: u64,
}

impl State {
    /// Read the current clipboard as text through a pipe and report it when the owner is done writing.
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

    /// Text or a PNG from the browser or the API becomes the clipboard, offered to Wayland and X11 clients.
    pub fn set_clipboard(&mut self, mime: String, data: Vec<u8>) {
        self.cancel_clipboard_read(); // an application's older clipboard must not land after this one
        let mimes: Vec<String> = if mime == PNG { vec![mime.clone()] } else { TEXT_MIMES.iter().map(|m| m.to_string()).collect() };
        set_data_device_selection(&self.dh, &self.seat, mimes.clone(), Selection::Ours(Arc::new(Ours { mime, data })));
        if let Some(xwm) = self.xwm.as_mut()
            && let Err(e) = xwm.new_selection(SelectionTarget::Clipboard, Some(mimes))
        {
            tracing::warn!("xwayland selection: {e:?}");
        }
    }

    /// Serve our clipboard to a client that asked for it, from the event loop as the pipe accepts it, so
    /// a reader that stalls costs a source, not a thread.
    pub fn serve_clipboard(&mut self, ours: Arc<Ours>, fd: OwnedFd) {
        let _ = rustix::fs::fcntl_setfl(&fd, rustix::fs::OFlags::NONBLOCK);
        let mut written = 0;
        let token = self.handle.insert_source(Generic::new(fd, Interest::WRITE, Mode::Level), move |_, fd, _| {
            loop {
                if written >= ours.data.len() {
                    return Ok(PostAction::Remove); // closes the pipe: end of data
                }
                match rustix::io::write(&*fd, &ours.data[written..]) {
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
