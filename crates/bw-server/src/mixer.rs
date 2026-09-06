use crate::{App, Viewers, protocol};
use bw_core::audio::{Command, Level, Request, Snapshot};
use std::sync::Arc;
use tokio::sync::{mpsc, watch};

#[derive(Clone, Copy, Default, PartialEq)]
pub struct MixerAudience {
    pub subscribed: bool,
    pub controller: Option<u64>,
    pub epoch: u64,
}

/// Latest state and authority use watch channels; commands and errors have bounded queues.
pub struct Mixer {
    pub commands: mpsc::Sender<Request>,
    pub audience: watch::Sender<MixerAudience>,
    pub epoch: Arc<bw_core::audio::Epoch>,
    pub state: watch::Receiver<Snapshot>,
    pub levels: watch::Receiver<Vec<Level>>,
    pub errors: Option<mpsc::Receiver<(u64, String)>>,
}

impl App {
    pub(crate) fn mixer_audience(&self, viewers: &Viewers) {
        if let Some(mixer) = &self.mixer {
            mixer.epoch.publish(viewers.control_epoch);
            let next = MixerAudience {
                subscribed: viewers.sessions.values().any(|session| session.mixer_subscribed),
                controller: viewers.controller, epoch: viewers.control_epoch,
            };
            mixer.audience.send_if_modified(|current| {
                if *current == next { false } else { *current = next; true }
            });
        }
    }

    pub(crate) fn mixer_state(&self) -> Snapshot {
        self.mixer.as_ref().map(|mixer| mixer.state.borrow().clone()).unwrap_or_else(|| Snapshot {
            error: Some("Session audio is unavailable.".into()), ..Snapshot::default()
        })
    }

    pub(crate) fn mixer_message(&self, viewers: &mut Viewers, id: u64, command: Result<Command, &'static str>) {
        let result = (|| -> Result<(), &'static str> {
            let command = command?;
            command.validate()?;
            if let Command::Subscribe { enabled } = command {
                if let Some(session) = viewers.sessions.get_mut(&id) { session.mixer_subscribed = enabled; }
                self.mixer_audience(viewers);
                return Ok(());
            }
            if viewers.controller != Some(id) { return Err("Only the controlling viewer can change session audio."); }
            let mixer = self.mixer.as_ref().ok_or("Session mixer is unavailable.")?;
            if !mixer.state.borrow().available { return Err("Session mixer is unavailable."); }
            mixer.commands.try_send(Request::Command { viewer: id, epoch: viewers.control_epoch, command })
                .map_err(|_| "Session mixer is busy or disconnected. Try again.")
        })();
        if let Err(error) = result {
            if let Some(session) = viewers.sessions.get(&id) { let _ = session.events.try_send(protocol::mixer_error(error)); }
        }
    }
}

pub(crate) async fn errors(app: Arc<App>, mut errors: mpsc::Receiver<(u64, String)>) {
    while let Some((viewer, error)) = errors.recv().await {
        if let Some(session) = app.viewers.lock().unwrap().sessions.get(&viewer) {
            let _ = session.events.try_send(protocol::mixer_error(&error));
        }
    }
}
