//! Session mixer messages. Object identifiers include the graph generation and object serial.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Output,
    Input,
    Playback,
    Recording,
}

impl Kind {
    pub fn target_kind(self) -> Option<Self> {
        match self {
            Self::Playback => Some(Self::Output),
            Self::Recording => Some(Self::Input),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub name: String,
    pub application: Option<String>,
    pub kind: Kind,
    pub state: String,
    /// Perceptual percent: a value of 50 corresponds to linear gain 0.125.
    pub volume: Option<f32>,
    pub mute: Option<bool>,
    pub volume_writable: bool,
    pub mute_writable: bool,
    pub routing_writable: bool,
    pub targets: Vec<String>,
    pub is_default: bool,
    pub meter_before_volume: bool,
    pub meter_active: bool,
    pub meter_error: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub generation: String,
    pub available: bool,
    /// With available=true this describes degraded service; existing rows remain valid.
    pub error: Option<String>,
    pub routing: bool,
    pub nodes: Vec<Node>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Level {
    pub id: String,
    pub peak: f32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    Subscribe { enabled: bool },
    Volume { id: String, value: f32 },
    Mute { id: String, value: bool },
    Target { id: String, target: Option<String> },
    Default { id: String },
}

impl Command {
    pub fn validate(&self) -> Result<(), &'static str> {
        let id = match self {
            Self::Subscribe { .. } => return Ok(()),
            Self::Volume { id, value } => {
                if !value.is_finite() || !(0.0..=100.0).contains(value) {
                    return Err("Volume must be between 0 and 100 percent.");
                }
                id
            }
            Self::Mute { id, .. } | Self::Default { id } => id,
            Self::Target { id, target } => {
                if target
                    .as_ref()
                    .is_some_and(|target| target.is_empty() || target.len() > 128)
                {
                    return Err("Invalid audio target.");
                }
                id
            }
        };
        if id.is_empty() || id.len() > 128 {
            return Err("Invalid audio object.");
        }
        Ok(())
    }
}

/// The server supplies the viewer and control epoch; clients cannot choose them.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Request {
    Audience {
        subscribed: bool,
        controller: Option<u64>,
        epoch: u64,
    },
    Command {
        viewer: u64,
        epoch: u64,
        command: Command,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Event {
    State(Snapshot),
    Levels(Vec<Level>),
    Error { viewer: u64, message: String },
}

/// Shared dispatch epoch, published before the server announces a control handoff.
/// The private helper maps the same fixed-size file through its own descriptor.
pub struct Epoch {
    memory: memmap2::MmapMut,
    _file: std::fs::File,
}

impl Epoch {
    /// # Safety
    /// The file must remain eight bytes for the lifetime of every mapping. After
    /// initialization, all access to its contents must use aligned AtomicU64 operations.
    pub unsafe fn map(file: std::fs::File) -> std::io::Result<Self> {
        if file.metadata()?.len() != 8 {
            return Err(std::io::Error::other("invalid mixer epoch size"));
        }
        // The owner creates eight zero bytes before mapping and never resizes the file.
        // mmap is page-aligned; all subsequent accesses use the same AtomicU64 type.
        let memory = unsafe { memmap2::MmapOptions::new().len(8).map_mut(&file)? };
        Ok(Self { memory, _file: file })
    }

    fn value(&self) -> &std::sync::atomic::AtomicU64 {
        // The shared mapping outlives every access and its base satisfies atomic alignment.
        unsafe { &*self.memory.as_ptr().cast::<std::sync::atomic::AtomicU64>() }
    }

    pub fn load(&self) -> u64 { self.value().load(std::sync::atomic::Ordering::SeqCst) }
    pub fn publish(&self, epoch: u64) { self.value().store(epoch, std::sync::atomic::Ordering::SeqCst); }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_and_object_boundaries() {
        assert!(serde_json::from_str::<Command>(r#"{"op":"mute","id":"x","value":true,"extra":1}"#).is_err());
        assert!(Command::Default { id: "x".repeat(129) }.validate().is_err());
        assert!(Command::Target { id: "x".into(), target: Some(String::new()) }.validate().is_err());
        for value in [f32::NAN, f32::INFINITY, -1.0, 100.1] {
            assert!(
                Command::Volume {
                    id: "generation:7".into(),
                    value
                }
                .validate()
                .is_err()
            );
        }
        for value in [0.0, 50.0, 100.0] {
            assert!(
                Command::Volume {
                    id: "generation:7".into(),
                    value
                }
                .validate()
                .is_ok()
            );
        }
        assert!(
            Command::Mute {
                id: String::new(),
                value: true
            }
            .validate()
            .is_err()
        );
        assert!(
            Command::Target {
                id: "generation:7".into(),
                target: Some("x".repeat(129))
            }
            .validate()
            .is_err()
        );
    }
}
