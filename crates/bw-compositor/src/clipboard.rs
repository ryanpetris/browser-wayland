//! Clipboard bridge for the browser and the API. Text copied in a desktop application is read from its
//! owner (a Wayland client, or an X11 client through Xwayland) into `Event::Clipboard`; text set from
//! outside becomes a compositor-owned selection served to whoever asks. Text only, 1 MiB at most.

use std::{io::Write, os::fd::OwnedFd, sync::Arc};

use bw_core::Event;
use smithay::{
    reexports::{
        calloop::{Interest, Mode, PostAction, generic::Generic},
        rustix,
    },
    wayland::selection::{SelectionTarget, data_device::{request_data_device_client_selection, set_data_device_selection}},
};

use crate::State;

/// What a compositor-owned selection holds: relayed from an X11 client, or text we were given.
#[derive(Clone, Debug, Default)]
pub enum Selection {
    #[default]
    X11,
    Text(Arc<String>),
}

pub const TEXT_MIMES: [&str; 5] = ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING", "TEXT", "STRING"];
const LIMIT: usize = 1 << 20;

/// The mime type to read a selection as text, if it offers one.
pub fn text_mime(mimes: &[String]) -> Option<String> {
    TEXT_MIMES.iter().find(|m| mimes.iter().any(|o| o == *m)).map(|m| m.to_string())
}

impl State {
    /// Read the current clipboard as text through a pipe and report it when the owner is done writing.
    /// `x11` says the owner is an X11 client (the request goes through Xwayland).
    pub fn read_clipboard(&mut self, mime: String, x11: bool) {
        let Ok((read, write)) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC) else { return };
        let _ = rustix::fs::fcntl_setfl(&read, rustix::fs::OFlags::NONBLOCK);
        let requested = if x11 {
            self.xwm.as_mut().map(|xwm| xwm.send_selection(SelectionTarget::Clipboard, mime, write, self.handle.clone()).map_err(|e| format!("{e:?}"))).unwrap_or(Err("no xwm".into()))
        } else {
            request_data_device_client_selection(&self.seat, mime, write).map_err(|e| format!("{e:?}"))
        };
        if let Err(e) = requested {
            tracing::debug!("clipboard read: {e}");
            return;
        }
        let mut data = Vec::new();
        let source = Generic::new(read, Interest::READ, Mode::Level);
        let _ = self.handle.insert_source(source, move |_, fd, state| {
            let mut chunk = [0u8; 8192];
            loop {
                match rustix::io::read(&*fd, &mut chunk) {
                    Ok(0) => {
                        let _ = state.events.send(Event::Clipboard(String::from_utf8_lossy(&data).into_owned()));
                        return Ok(PostAction::Remove);
                    }
                    Ok(n) => {
                        data.extend_from_slice(&chunk[..n]);
                        if data.len() > LIMIT {
                            tracing::debug!("clipboard read: over {LIMIT} bytes, dropped");
                            return Ok(PostAction::Remove);
                        }
                    }
                    Err(rustix::io::Errno::AGAIN) => return Ok(PostAction::Continue),
                    Err(_) => return Ok(PostAction::Remove),
                }
            }
        });
    }

    /// Text from the browser or the API becomes the clipboard, offered to Wayland and X11 clients.
    pub fn set_clipboard(&mut self, text: String) {
        let mimes: Vec<String> = TEXT_MIMES.iter().map(|m| m.to_string()).collect();
        set_data_device_selection(&self.dh, &self.seat, mimes.clone(), Selection::Text(Arc::new(text)));
        if let Some(xwm) = self.xwm.as_mut()
            && let Err(e) = xwm.new_selection(SelectionTarget::Clipboard, Some(mimes))
        {
            tracing::warn!("xwayland selection: {e:?}");
        }
    }
}

/// Serve our text to a client that asked for it; the write happens off the compositor thread.
pub fn serve(text: Arc<String>, fd: OwnedFd) {
    std::thread::spawn(move || {
        let _ = std::fs::File::from(fd).write_all(text.as_bytes());
    });
}
