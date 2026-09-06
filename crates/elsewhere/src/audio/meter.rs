//! Passive private-graph meters. All callbacks run on the management main loop.
use anyhow::{Context, Result, ensure};
use elsewhere_core::audio::Kind;
use pipewire::{self as pw, properties::properties, spa::{
    param::{ParamType, audio::{AudioFormat, AudioInfoRaw}},
    pod::{Object, Pod, Value, serialize::PodSerializer},
    utils::Direction,
}};
use std::{cell::RefCell, io::Cursor, rc::Rc, time::{Duration, Instant}};

pub const NAME: &str = "elsewhere-meter";

#[derive(Default)]
struct Reading {
    peak: f32,
    error: Option<String>,
    received: Option<Instant>,
}

pub struct Meter {
    // Listener registration must end before the stream is destroyed.
    _listener: pw::stream::StreamListener<Rc<RefCell<Reading>>>,
    _stream: pw::stream::StreamRc,
    reading: Rc<RefCell<Reading>>,
}

impl Meter {
    pub fn new(core: pw::core::CoreRc, serial: &str, kind: Kind) -> Result<Self> {
        ensure!(serial.parse::<u64>().is_ok(), "meter target serial is required");
        let stream = pw::stream::StreamRc::new(core, NAME, properties! {
            "node.name" => NAME,
            "target.object" => serial,
            "stream.monitor" => "true",
            "stream.capture.sink" => matches!(kind, Kind::Output | Kind::Recording).to_string(),
            "node.passive" => "true",
            "node.dont-fallback" => "true",
            // WirePlumber may temporarily rebuild the target's session item.
            "node.linger" => "true",
            "resample.peaks" => "true",
        })?;
        let reading: Rc<RefCell<Reading>> = Rc::default();
        let listener = stream.add_local_listener_with_user_data(reading.clone())
            .state_changed(|_, reading, old, state| {
                let mut reading = reading.borrow_mut();
                match &state {
                    pw::stream::StreamState::Error(error) => reading.error = Some(error.clone()),
                    pw::stream::StreamState::Unconnected if !matches!(old, pw::stream::StreamState::Unconnected) => reading.error = Some("Audio meter disconnected.".into()),
                    _ => {}
                }
                if !matches!(state, pw::stream::StreamState::Streaming) { reading.peak = 0.0; reading.received = None; }
            })
            .process(|stream, reading| {
                while let Some(mut buffer) = stream.dequeue_buffer() {
                let mut reading = reading.borrow_mut();
                for data in buffer.datas_mut() {
                    let offset = data.chunk().offset() as usize;
                    let size = data.chunk().size() as usize;
                    let Some(bytes) = data.data() else { continue; };
                    let Some(samples) = bytes.get(offset..offset.saturating_add(size)) else { continue; };
                    for sample in samples.chunks_exact(4) {
                        let value = f32::from_ne_bytes(sample.try_into().unwrap()).abs();
                        if value.is_finite() { reading.peak = reading.peak.max(value); reading.received = Some(Instant::now()); }
                    }
                }
                }
            }).register()?;
        let mut format = AudioInfoRaw::new();
        format.set_format(AudioFormat::F32P);
        format.set_rate(25);
        let pod = Value::Object(Object {
            type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
            id: ParamType::EnumFormat.as_raw(),
            properties: format.into(),
        });
        let bytes = PodSerializer::serialize(Cursor::new(Vec::new()), &pod)?.0.into_inner();
        // Without RT_PROCESS, the callback shares the management loop's thread.
        stream.connect(Direction::Input, None,
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut [Pod::from_bytes(&bytes).context("meter format")?])?;
        Ok(Self { _listener: listener, _stream: stream, reading })
    }

    pub fn peak(&self) -> f32 { self.reading.borrow().peak }

    pub fn take_peak(&self) -> f32 {
        std::mem::take(&mut self.reading.borrow_mut().peak)
    }

    pub fn active(&self) -> bool { self.reading.borrow().received.is_some_and(|received| received.elapsed() < Duration::from_secs(1)) }

    pub fn error(&self) -> Option<String> { self.reading.borrow().error.clone() }
}
