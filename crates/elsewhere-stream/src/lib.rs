//! GStreamer: compositor dmabufs into an `appsrc` zero-copy, VA-API scaling and encoding, encoded
//! frames on a channel. One [`GstSink`] per viewer, each at its own size and codec.

mod lease;

use std::{
    collections::HashMap,
    io::{Seek, SeekFrom},
    os::fd::OwnedFd,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
};

use anyhow::{Context, Result};
use elsewhere_core::{Bytes, Codec, EncodedFrame, Frame, FrameBuffer, FrameSink, OutputGeometry, Quality, SinkError, StreamControl, StreamInfo, StreamMsg, Submit};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_allocators as gst_allocators;
use gstreamer_allocators::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use tokio::sync::mpsc;

/// A running pipeline. Dropping it stops the stream.
pub struct Stream {
    pipeline: gst::Pipeline,
    encoder: gst::Element,
    /// Set by the bus watcher on a pipeline error; the sink then rebuilds on the next frame.
    dead: Arc<AtomicBool>,
}

impl Drop for Stream {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

impl Stream {
    /// Force the next frame to be a keyframe (IDR + SPS/PPS).
    pub fn request_keyframe(&self) {
        let ev = gst_video::UpstreamForceKeyUnitEvent::builder().all_headers(true).build();
        if !self.encoder.send_event(ev) {
            tracing::warn!("encoder refused the keyframe request");
        }
    }
}

struct EncodeOpts {
    width: u32,
    height: u32,
    scale: f64,
    codec: Codec,
}

/// The parser after an encoder, producing one WebCodecs chunk per buffer.
fn parse_tail(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => "! h264parse config-interval=-1 ! video/x-h264,stream-format=byte-stream,alignment=au",
        Codec::Hevc => "! h265parse config-interval=-1 ! video/x-h265,stream-format=byte-stream,alignment=au",
        Codec::Vp9 => "! vp9parse ! video/x-vp9",
        // low-overhead OBUs, one temporal unit per buffer: what WebCodecs takes; the encoders put a
        // sequence header in every keyframe's unit
        Codec::Av1 => "! av1parse ! video/x-av1,stream-format=obu-stream,alignment=tu",
        Codec::Vp8 => "! video/x-vp8",
    }
}

/// A VA encoder with its parser. `enc` is the element for this device. Keyframes come on request (a
/// viewer joining, a gap in what one got), so the periodic one is pushed out as far as the property goes:
/// its hundred-odd kilobytes every second are the burst a lossy link chokes on.
fn hardware_tail(codec: Codec, enc: &str, bitrate_kbps: u32) -> String {
    let common = format!("{enc} name=enc rate-control=cbr bitrate={bitrate_kbps} target-usage=7 ref-frames=1 key-int-max=1024");
    match codec {
        Codec::H264 => format!("{common} b-frames=0 ! video/x-h264,profile=high {}", parse_tail(codec)),
        Codec::Hevc => format!("{common} b-frames=0 ! video/x-h265,profile=main {}", parse_tail(codec)),
        Codec::Vp9 | Codec::Av1 => format!("{common} {}", parse_tail(codec)),
        Codec::Vp8 => unreachable!("VA has no VP8 encoder"),
    }
}

/// A CPU encoder (`--software-encoding`) with its parser, at its fastest low-latency settings and with its
/// periodic keyframe pushed out of reach, as for the VA encoders. `enc` is the element `software_encoder` found.
fn software_tail(codec: Codec, enc: &str, bitrate_kbps: u32) -> String {
    let bps = bitrate_kbps * 1000;
    let encoder = match (codec, enc) {
        (Codec::Vp8, _) => format!("vp8enc name=enc deadline=1 cpu-used=8 end-usage=cbr target-bitrate={bps} lag-in-frames=0 threads=4 keyframe-max-dist=2147483647"),
        (Codec::Vp9, _) => format!("vp9enc name=enc deadline=1 cpu-used=8 end-usage=cbr target-bitrate={bps} lag-in-frames=0 row-mt=true threads=4 keyframe-max-dist=2147483647"),
        (Codec::H264, "x264enc") => format!("x264enc name=enc tune=zerolatency speed-preset=superfast bitrate={bitrate_kbps} bframes=0 key-int-max=2147483647 ! video/x-h264,profile=main"),
        (Codec::H264, _) => format!("openh264enc name=enc rate-control=bitrate bitrate={bps} gop-size=0"),
        (Codec::Hevc, _) => format!("x265enc name=enc tune=zerolatency speed-preset=ultrafast bitrate={bitrate_kbps} key-int-max=2147483647 ! video/x-h265,profile=main"),
        // a two-frame mini-GOP: about six frames of delay instead of thirty-odd (the library insists on
        // some lookahead); keyframes on request (the documented -1, no periodic intra, is refused in the
        // bitrate mode, so the period is the largest the property takes)
        (Codec::Av1, _) => format!("svtav1enc name=enc preset=12 target-bitrate={bitrate_kbps} intra-period-length=2147483647 parameters-string=hierarchical-levels=1:force-key-frames=1"),
    };
    format!("{encoder} {}", parse_tail(codec))
}

/// The CPU encoder element for a codec, if its plugin is installed.
fn software_encoder(codec: Codec) -> Option<&'static str> {
    let names: &[&str] = match codec {
        Codec::Vp8 => &["vp8enc"],
        Codec::Vp9 => &["vp9enc"],
        Codec::H264 => &["x264enc", "openh264enc"],
        Codec::Hevc => &["x265enc"],
        Codec::Av1 => &["svtav1enc"],
    };
    names.iter().copied().find(|n| gst::ElementFactory::find(n).is_some())
}

/// What this machine can encode, best first: with `software`, the CPU encoders that are installed
/// (VP8 is the cheapest); else the GPU's VA encoders (see `hardware_codecs`).
pub fn codecs(prefix: &str, software: bool) -> Vec<Codec> {
    if gst::init().is_err() {
        return vec![];
    }
    if software {
        [Codec::Vp8, Codec::H264, Codec::Vp9, Codec::Hevc, Codec::Av1].into_iter().filter(|&c| software_encoder(c).is_some()).collect()
    } else {
        hardware_codecs(prefix)
    }
}

/// The name prefix of the VA elements for a render node: `va` for the driver's first device,
/// `va<node>` (as in `varenderD129h264enc`) for another.
pub fn va_prefix(render_node: &std::path::Path) -> String {
    match render_node.file_name().and_then(|n| n.to_str()) {
        Some(node) if node != "renderD128" => format!("va{node}"),
        _ => "va".into(),
    }
}

/// The VA encoder element for a codec on the device: the regular one, else the low-power one.
fn va_encoder(prefix: &str, codec: Codec) -> Option<String> {
    let base = match codec {
        Codec::H264 => "h264",
        Codec::Hevc => "h265",
        Codec::Vp9 => "vp9",
        Codec::Av1 => "av1",
        Codec::Vp8 => return None,
    };
    ["enc", "lpenc"].iter().map(|kind| format!("{prefix}{base}{kind}")).find(|name| gst::ElementFactory::find(name).is_some())
}

/// The codecs the GPU encodes: those whose VA element the driver registered, best first.
fn hardware_codecs(prefix: &str) -> Vec<Codec> {
    if gst::init().is_err() {
        return vec![];
    }
    [Codec::Av1, Codec::Hevc, Codec::Vp9, Codec::H264].into_iter().filter(|&c| va_encoder(prefix, c).is_some()).collect()
}

static STREAM_SEQ: AtomicU32 = AtomicU32::new(1);

/// Build `<head> ! <tail: encoder and parser> ! appsink` and start it.
fn build(head: &str, tail: &str, opts: EncodeOpts, tx: mpsc::Sender<StreamMsg>) -> Result<Stream> {
    let stream_id = STREAM_SEQ.fetch_add(1, Ordering::Relaxed);
    let desc = format!("{head} ! {tail} ! appsink name=sink sync=false max-buffers=0");
    let pipeline = gst::parse::launch(&desc)?.downcast::<gst::Pipeline>().expect("parse::launch returns a pipeline");
    let encoder = pipeline.by_name("enc").context("enc element")?;
    let sink = pipeline.by_name("sink").context("sink element")?.downcast::<gst_app::AppSink>().unwrap();

    let (codec, width, height) = (opts.codec, opts.width, opts.height);
    let failed_tx = tx.clone();
    let mut info = Some(StreamInfo { stream_id, codec: String::new(), width, height, scale: opts.scale });
    sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                let keyframe = !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);
                tracing::debug!(keyframe, bytes = map.len(), "encoded");
                // First keyframe: derive the WebCodecs codec string from the SPS and announce the stream.
                if keyframe && info.is_some() {
                    let mut i = info.take().unwrap();
                    i.codec = codec_string(codec, &map, width, height).unwrap_or_else(|| {
                        tracing::error!("no parameter sets in the first keyframe; guessing the codec string");
                        match codec {
                            Codec::H264 => "avc1.640028",
                            Codec::Hevc => "hev1.1.6.L120.90",
                            Codec::Vp9 => "vp09.00.41.08",
                            Codec::Av1 => "av01.0.09M.08",
                            Codec::Vp8 => "vp8",
                        }
                        .into()
                    });
                    tracing::info!(codec = %i.codec, width, height, "stream started");
                    let _ = tx.blocking_send(StreamMsg::Info(i));
                }
                // ponytail: blocking send so no frame silently vanishes between encoder and server;
                // the consumer only does try_send, so this never blocks in practice.
                let _ = tx.blocking_send(StreamMsg::Frame(EncodedFrame {
                    stream_id,
                    keyframe,
                    pts_us: buffer.pts().map(|t| t.useconds()).unwrap_or(0),
                    data: Bytes::copy_from_slice(&map),
                }));
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    let dead = Arc::new(AtomicBool::new(false));
    let flag = dead.clone();
    // Runs on the posting thread and is freed with the pipeline. (A thread waiting on the bus would
    // never wake once the pipeline is gone, and its copy of `tx` would keep the channel open.)
    pipeline.bus().unwrap().set_sync_handler(move |_, msg| {
        match msg.view() {
            gst::MessageView::Error(e) => {
                tracing::error!(src = ?e.src().map(|s| s.path_string()), "gstreamer: {} ({:?})", e.error(), e.debug());
            }
            gst::MessageView::Eos(_) => {}
            _ => return gst::BusSyncReply::Drop,
        }
        // Dead: the next keyframe request drops the pipeline (freeing its leases) and the next frame rebuilds it.
        if !flag.swap(true, Ordering::Relaxed) {
            let tx = failed_tx.clone();
            std::thread::spawn(move || {
                let _ = tx.blocking_send(StreamMsg::Failed); // off the streaming thread, which must not block here
            });
        }
        gst::BusSyncReply::Drop
    });

    pipeline.set_state(gst::State::Playing)?;
    Ok(Stream { pipeline, encoder, dead })
}

/// The WebCodecs codec string for the first keyframe, per the AVC/HEVC/VP9 codec registrations.
fn codec_string(codec: Codec, au: &[u8], width: u32, height: u32) -> Option<String> {
    match codec {
        Codec::H264 => {
            // SPS (nal type 7): profile_idc, constraint flags, level_idc
            let sps = nal_units(au).find(|n| n[0] & 0x1f == 7)?;
            Some(format!("avc1.{:02X}{:02X}{:02X}", sps.get(1)?, sps.get(2)?, sps.get(3)?))
        }
        Codec::Hevc => {
            // SPS (nal type 33): 2-byte header, 1 byte, then profile_tier_level
            let sps = unescape(nal_units(au).find(|n| (n[0] >> 1) & 0x3f == 33)?);
            let ptl = sps.get(3..15)?;
            let profile = ptl[0] & 0x1f;
            let tier = if ptl[0] & 0x20 != 0 { 'H' } else { 'L' };
            let compat = u32::from_be_bytes([ptl[1], ptl[2], ptl[3], ptl[4]]).reverse_bits();
            let mut constraints: Vec<u8> = ptl[5..11].to_vec();
            while constraints.len() > 1 && constraints.last() == Some(&0) {
                constraints.pop();
            }
            let constraints = constraints.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(".");
            Some(format!("hev1.{profile}.{compat:X}.{tier}{}.{constraints}", ptl[11]))
        }
        Codec::Vp9 => {
            // profile 0, 8-bit; the level only has to be high enough for the picture size
            let level = if width * height <= 1920 * 1080 { "41" } else if width * height <= 4096 * 2176 { "51" } else { "61" };
            Some(format!("vp09.00.{level}.08"))
        }
        Codec::Av1 => {
            // main profile, 8-bit, main tier; the level (4.1, 5.1, 6.1 at 60 fps) only has to cover the picture size
            let level = if width * height <= 2048 * 1088 { "09" } else if width * height <= 4096 * 2176 { "13" } else { "17" };
            Some(format!("av01.0.{level}M.08"))
        }
        Codec::Vp8 => Some("vp8".into()),
    }
}

/// Remove emulation-prevention bytes (`00 00 03` → `00 00`) from a NAL unit.
fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// NAL unit payloads (header byte first) of an Annex B access unit.
fn nal_units(au: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut starts = vec![];
    let mut i = 0;
    while i + 3 <= au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut ends: Vec<usize> = starts.iter().skip(1).map(|&s| s - 3).collect();
    ends.push(au.len());
    starts.into_iter().zip(ends).map(move |(s, e)| &au[s..e.max(s)]).filter(|n| !n.is_empty())
}

/// A running pipeline (audio capture, microphone playback, the webcam); drop to stop it.
pub struct Running {
    pipeline: gst::Pipeline,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
    // The plugin duplicates this descriptor and keys its shared cores by the original number.
    _connection: Option<std::os::unix::net::UnixStream>,
}

impl Running {
    pub fn check(&self) -> Result<()> {
        if let Some(msg) = self.pipeline.bus().context("pipeline bus")?.pop_filtered(&[gst::MessageType::Error, gst::MessageType::Eos]) {
            if let gst::MessageView::Error(e) = msg.view() {
                anyhow::bail!("{} ({:?})", e.error(), e.debug());
            }
            anyhow::bail!("audio pipeline ended");
        }
        Ok(())
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() { let _ = stop.send(()); }
        let _ = self.pipeline.set_state(gst::State::Null);
        if let Some(thread) = self.thread.take() { let _ = thread.join(); }
    }
}

pub fn audio_source(socket: &std::path::Path, device: &str, tx: mpsc::Sender<StreamMsg>) -> Result<Running> {
    gst::init()?;
    let desc = format!(
        "pipewiresrc name=source target-object={device} \
         stream-properties=\"properties,node.name=(string)elsewhere-capture,stream.capture.sink=(boolean)true,node.dont-fallback=(boolean)true\" \
         ! audio/x-raw,rate=48000,channels=2 \
         ! audioconvert ! audioresample ! opusenc bitrate=96000 frame-size=20 audio-type=generic dtx=true \
         ! appsink name=sink sync=false max-buffers=0"
    );
    let pipeline = gst::parse::launch(&desc)?.downcast::<gst::Pipeline>().expect("parse::launch returns a pipeline");
    use std::os::fd::AsRawFd;
    let connection = std::os::unix::net::UnixStream::connect(socket).context("private PipeWire connection")?;
    pipeline.by_name("source").context("PipeWire source")?.set_property("fd", connection.as_raw_fd());
    let sink = pipeline.by_name("sink").context("sink element")?.downcast::<gst_app::AppSink>().unwrap();
    sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                let pts_us = buffer.pts().map(|t| t.useconds()).unwrap_or(0);
                let _ = tx.try_send(StreamMsg::Audio { pts_us, data: Bytes::copy_from_slice(&map) });
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    let running = Running { pipeline, stop: None, thread: None, _connection: Some(connection) };
    running.pipeline.set_state(gst::State::Playing)?;
    Ok(running)
}

/// Plays Opus packets from the browser's microphone into the private microphone loopback input.
/// With `sync=false`, the sink paces at 48 kHz and
/// swallows the network's jitter in its buffer, and packets that pile up behind a stall (or clock
/// drift) are dropped at the source rather than played late.
pub fn audio_sink(socket: &std::path::Path, device: &str, rx: mpsc::Receiver<Bytes>) -> Result<Running> {
    feed(
        &format!(
            "appsrc name=src is-live=true format=time do-timestamp=true max-buffers=10 leaky-type=downstream \
             caps=audio/x-opus,channel-mapping-family=0,channels=1,rate=48000 ! opusdec \
             ! pipewiresink name=output target-object={device} sync=false \
             stream-properties=\"properties,node.name=(string)elsewhere-microphone-stream,node.dont-fallback=(boolean)true\""
        ),
        "microphone",
        rx,
        Some(socket),
    )
}

/// Plays VP8 frames from the browser's webcam into `device`, a v4l2loopback camera, as 1280×720 YUY2 (the
/// loopback keeps the first format it is given, so every frame is scaled to that one, letterboxed):
/// applications open it like any camera. The decoder gets every frame it is handed (a dropped delta would
/// corrupt the picture until the next keyframe): the source blocks the feeder when it is four frames
/// behind, the channel fills, and the server drops there, deltas until the next keyframe.
pub fn video_sink(device: &std::path::Path, rx: mpsc::Receiver<Bytes>) -> Result<Running> {
    feed(
        &format!(
            "appsrc name=src is-live=true format=time do-timestamp=true max-buffers=4 block=true caps=video/x-vp8 \
             ! vp8dec ! videoconvert ! videoscale ! video/x-raw,format=YUY2,width=1280,height=720,pixel-aspect-ratio=1/1 ! v4l2sink device={} sync=false",
            device.display()
        ),
        "webcam",
        rx,
        None,
    )
}

/// A pipeline whose `appsrc` a thread feeds from `rx`, one buffer per message; the pipeline stops when the
/// handle is dropped (the thread ends with the channel), or when it fails (the failure is logged and the
/// thread ends: pushing into a leaky source never fails by itself).
fn feed(desc: &str, name: &str, mut rx: mpsc::Receiver<Bytes>, socket: Option<&std::path::Path>) -> Result<Running> {
    gst::init()?;
    let pipeline = gst::parse::launch(desc)?.downcast::<gst::Pipeline>().expect("parse::launch returns a pipeline");
    let src = pipeline.by_name("src").context("src element")?.downcast::<gst_app::AppSrc>().unwrap();
    let connection = socket.map(std::os::unix::net::UnixStream::connect).transpose().context("private PipeWire connection")?;
    if let Some(connection) = &connection {
        use std::os::fd::AsRawFd;
        pipeline.by_name("output").context("PipeWire sink")?.set_property("fd", connection.as_raw_fd());
    }
    // Native audio has a supervisor polling Running::check; webcam feeds own their error reporting.
    let bus = if socket.is_none() { Some(pipeline.bus().context("pipeline bus")?) } else { None };
    let mut running = Running { pipeline, stop: None, thread: None, _connection: connection };
    running.pipeline.set_state(gst::State::Playing)?;
    let what = name.to_string();
    let runtime = tokio::runtime::Builder::new_current_thread().build()?;
    let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel();
    running.stop = Some(stop_tx);
    running.thread = Some(std::thread::Builder::new().name(name.into()).spawn(move || runtime.block_on(async move {
        loop {
            let packet = tokio::select! {
                _ = &mut stop_rx => break,
                packet = rx.recv() => match packet { Some(packet) => packet, None => break },
            };
            if src.push_buffer(gst::Buffer::from_slice(packet)).is_err() {
                break; // the pipeline is gone
            }
            if let Some(msg) = bus.as_ref().and_then(|bus| bus.pop_filtered(&[gst::MessageType::Error]))
                && let gst::MessageView::Error(e) = msg.view()
            {
                tracing::warn!("{what}: {} ({:?})", e.error(), e.debug());
                break;
            }
        }
    }))?);
    Ok(running)
}

/// The real sink: compositor dmabufs → `appsrc`. Clone it for a keyframe handle before moving it into the compositor.
#[derive(Clone)]
pub struct GstSink(Arc<Mutex<Inner>>);

struct Inner {
    tx: mpsc::Sender<StreamMsg>,
    /// `va` or `va<node>`: which device's elements (see `va_prefix`).
    prefix: String,
    /// CPU encoders on linear frames instead of the GPU's.
    software: bool,
    quality: Quality,
    /// The last frame handed over (the frame cap), and the bitrate to restore after a refine frame.
    last_push: std::time::Instant,
    restore_kbps: Option<u32>,
    codec: Codec,
    /// Set by `output_changed`; the pipeline is (re)built lazily on the next frame.
    geo: Option<(OutputGeometry, u32, u64)>,
    /// The viewer's size: frames are scaled to it (none: the frames' own size).
    target: Option<(u32, u32)>,
    stream: Option<(Stream, gst_app::AppSrc)>,
    /// One imported memory per compositor swapchain slot, so VA keeps its surface import.
    mems: HashMap<u32, gst::Memory>,
    alloc: gst_allocators::DmaBufAllocator,
}

/// Keyframe and codec requests that don't keep the pipeline alive: once every `GstSink` clone is
/// dropped (the compositor let go of a window stream), the pipeline stops and `tx` closes with it.
pub struct GstControl(Weak<Mutex<Inner>>);

impl StreamControl for GstControl {
    fn request_keyframe(&self) {
        if let Some(inner) = self.0.upgrade() {
            GstSink(inner).request_keyframe();
        }
    }
    fn set_codec(&self, codec: Codec) {
        if let Some(inner) = self.0.upgrade() {
            GstSink(inner).set_codec(codec);
        }
    }
    fn set_size(&self, size: Option<(u32, u32)>) {
        if let Some(inner) = self.0.upgrade() {
            GstSink(inner).set_size(size);
        }
    }
    fn set_quality(&self, quality: Quality) {
        if let Some(inner) = self.0.upgrade() {
            GstSink(inner).set_quality(quality);
        }
    }
}

impl GstSink {
    pub fn control(&self) -> GstControl {
        GstControl(Arc::downgrade(&self.0))
    }

    pub fn new(bitrate_kbps: u32, prefix: &str, software: bool, tx: mpsc::Sender<StreamMsg>) -> Result<GstSink> {
        gst::init()?;
        Ok(GstSink(Arc::new(Mutex::new(Inner {
            tx,
            prefix: prefix.to_string(),
            software,
            quality: Quality { bitrate_kbps, max_fps: 0 },
            last_push: std::time::Instant::now(),
            restore_kbps: None,
            codec: Codec::H264,
            geo: None,
            target: None,
            stream: None,
            mems: HashMap::new(),
            alloc: gst_allocators::DmaBufAllocator::new(),
        }))))
    }

}

impl StreamControl for GstSink {
    fn request_keyframe(&self) {
        let mut i = self.0.lock().unwrap();
        if i.stream.as_ref().is_some_and(|(s, _)| s.dead.load(Ordering::Relaxed)) {
            let old = i.take_stream();
            drop(i);
            discard(old); // releases the compositor's leases so it can render the requested full frame
            return;
        }
        // Send the event without holding the lock: it can wait on streaming threads.
        let stream = i.stream.as_ref().map(|(s, _)| (s.encoder.clone(), s.dead.clone()));
        drop(i);
        if let Some((encoder, _)) = stream {
            let ev = gst_video::UpstreamForceKeyUnitEvent::builder().all_headers(true).build();
            if !encoder.send_event(ev) {
                tracing::warn!("encoder refused the keyframe request");
            }
        }
    }

    fn set_codec(&self, codec: Codec) {
        let mut i = self.0.lock().unwrap();
        if i.codec == codec {
            return;
        }
        i.codec = codec;
        let old = i.take_stream();
        drop(i);
        discard(old);
    }

    fn set_size(&self, size: Option<(u32, u32)>) {
        let mut i = self.0.lock().unwrap();
        if i.target == size {
            return;
        }
        i.target = size;
        let old = i.take_stream();
        drop(i);
        discard(old);
    }
    fn set_quality(&self, quality: Quality) {
        let mut i = self.0.lock().unwrap();
        if i.quality == quality {
            return;
        }
        i.quality = quality;
        i.restore_kbps = None; // a refine boost in flight ends with the new rate
        if let Some((stream, _)) = &i.stream {
            set_bitrate(&stream.encoder, quality.bitrate_kbps);
        }
    }
}

/// Tear a pipeline down off the caller's thread: stopping it waits for streaming threads,
/// which may be waiting on the server, which may be waiting on us.
fn discard(old: (Option<(Stream, gst_app::AppSrc)>, HashMap<u32, gst::Memory>)) {
    if old.0.is_some() || !old.1.is_empty() {
        std::thread::spawn(move || drop(old));
    }
}

impl FrameSink for GstSink {
    fn output_changed(&mut self, geo: OutputGeometry, fourcc: u32, modifier: u64) {
        let mut i = self.0.lock().unwrap();
        i.geo = Some((geo, fourcc, modifier));
        let old = i.take_stream();
        drop(i);
        discard(old);
    }

    fn submit(&mut self, frame: Frame) -> Result<Submit, SinkError> {
        let mut i = self.0.lock().unwrap();
        if i.stream.as_ref().is_some_and(|(s, _)| s.dead.load(Ordering::Relaxed)) {
            let old = i.take_stream();
            discard(old);
        }
        Ok(i.push(frame)?)
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        discard(self.take_stream()); // never stop a pipeline on the compositor's thread
    }
}

impl Inner {
    /// Detach the pipeline and its imported memories; the caller drops them *after* unlocking.
    fn take_stream(&mut self) -> (Option<(Stream, gst_app::AppSrc)>, HashMap<u32, gst::Memory>) {
        (self.stream.take(), std::mem::take(&mut self.mems))
    }

    fn start(&mut self) -> Result<()> {
        let (geo, fourcc, modifier) = self.geo.context("output_changed was never called")?;
        // vapostproc scales the frame to the viewer's size on the way to NV12; the stream's scale then
        // maps its pixels to the desktop's logical pixels
        let (width, height) = self.target.unwrap_or((geo.width_px, geo.height_px));
        let src = "appsrc name=src is-live=true format=time do-timestamp=true block=false max-buffers=2 leaky-type=downstream";
        let (head, tail) = if self.software {
            // linear frames the CPU can map: plain raw caps on the dmabuf memory, converted and scaled on the
            // CPU; the rate is the compositor's clock in this mode, which the CPU encoders budget bits by
            // GBM reports a buffer made with its linear usage flag as "no modifier" (DRM_FORMAT_MOD_INVALID)
            anyhow::ensure!(modifier == 0 || modifier == 0x00ff_ffff_ffff_ffff, "software encoding needs linear frames, got modifier {modifier:#x}");
            let format = gst_video::dma_drm_fourcc_to_format(fourcc).context("unmappable fourcc")?;
            let head = format!(
                "{src} caps=\"video/x-raw,format={},width={},height={},framerate={}/1\" \
                 ! videoconvertscale n-threads=4 ! video/x-raw,format=I420,width={width},height={height}",
                format.to_str(),
                geo.width_px,
                geo.height_px,
                geo.refresh_mhz / 1000
            );
            let enc = software_encoder(self.codec).with_context(|| format!("no software encoder for {:?}", self.codec))?;
            (head, software_tail(self.codec, enc, self.quality.bitrate_kbps))
        } else {
            let head = format!(
                "{src} caps=\"video/x-raw(memory:DMABuf),format=DMA_DRM,drm-format={},width={},height={},framerate=60/1\" \
                 ! {}postproc ! video/x-raw(memory:VAMemory),format=NV12,width={width},height={height}",
                drm_format_string(fourcc, modifier),
                geo.width_px,
                geo.height_px,
                self.prefix
            );
            let enc = va_encoder(&self.prefix, self.codec).with_context(|| format!("no VA encoder for {:?}", self.codec))?;
            (head, hardware_tail(self.codec, &enc, self.quality.bitrate_kbps))
        };
        let scale = geo.scale * width as f64 / geo.width_px as f64;
        let opts = EncodeOpts { width, height, scale, codec: self.codec };
        let stream = build(&head, &tail, opts, self.tx.clone())?;
        let appsrc = stream.pipeline.by_name("src").unwrap().downcast::<gst_app::AppSrc>().unwrap();
        self.stream = Some((stream, appsrc));
        Ok(())
    }

    fn push(&mut self, frame: Frame) -> Result<Submit> {
        if self.stream.is_none() {
            self.start()?;
            self.restore_kbps = None;
        }
        // The viewer's frame cap: a frame closer to the last than its interval waits for the next one
        // (a refine frame never waits; it only comes when the picture stopped changing).
        if !frame.refine && self.quality.max_fps > 0 && self.last_push.elapsed() < std::time::Duration::from_micros(900_000 / self.quality.max_fps as u64) {
            return Ok(Submit::Held);
        }
        self.last_push = std::time::Instant::now();
        // A refine frame gets four times the bitrate with the CPU encoders, which take a new rate in
        // stride: the picture didn't change, so the bits go into sharpening what the motion before left
        // rough, and the next frame gets the rate back. The VA encoders open a new GOP on every rate
        // change, so there the refine frame is a plain one and CBR spends its usual budget on the residual.
        let enc = self.stream.as_ref().unwrap().0.encoder.clone();
        if let Some(kbps) = self.restore_kbps.take() {
            set_bitrate(&enc, kbps);
        }
        if frame.refine && self.software {
            set_bitrate(&enc, self.quality.bitrate_kbps.saturating_mul(4).min(60_000));
            self.restore_kbps = Some(self.quality.bitrate_kbps);
        }
        let format = gst_video::dma_drm_fourcc_to_format(frame.fourcc).context("unmappable fourcc")?;
        let buffer = match frame.buffer {
            FrameBuffer::Dmabuf { fd, stride, offset, slot_id, lease, .. } => {
                let mem = match self.mems.get(&slot_id) {
                    Some(m) => m.clone(), // fd (a dup) is closed on drop; GStreamer already holds one
                    None => {
                        let size = dmabuf_size(&fd)?;
                        // Safety: the fd is a dmabuf and GStreamer takes ownership of it.
                        // the CPU maps a software frame every time: keep the mapping instead of faulting it in again
                        let flags = if self.software { gst_allocators::FdMemoryFlags::KEEP_MAPPED } else { gst_allocators::FdMemoryFlags::NONE };
                        let m = unsafe { self.alloc.alloc_dmabuf_with_flags(std::os::fd::IntoRawFd::into_raw_fd(fd), size, flags) }?;
                        if self.mems.len() >= 8 {
                            self.mems.clear(); // slots get replaced now and then; in-flight buffers hold their own refs
                        }
                        self.mems.insert(slot_id, m.clone());
                        m
                    }
                };
                let mut buffer = gst::Buffer::new();
                let b = buffer.get_mut().unwrap();
                b.append_memory(mem);
                gst_video::VideoMeta::add_full(b, gst_video::VideoFrameFlags::empty(), format, frame.width, frame.height, &[offset as usize], &[stride as i32])?;
                lease::attach(b, lease);
                buffer
            }
            // read back by a software renderer: the pixels themselves, for the (software) pipeline's plain raw caps
            FrameBuffer::Memory { data, stride } => {
                let mut buffer = gst::Buffer::from_slice(data);
                gst_video::VideoMeta::add_full(buffer.get_mut().unwrap(), gst_video::VideoFrameFlags::empty(), format, frame.width, frame.height, &[0], &[stride as i32])?;
                buffer
            }
        };
        let (_, appsrc) = self.stream.as_ref().unwrap();
        appsrc.push_buffer(buffer)?; // leaky appsrc drops on its own; the lease then frees the slot
        Ok(Submit::Encoded)
    }
}

/// The encoder's bitrate, live where the element allows it (the VA encoders, x264, x265 and libvpx do;
/// the others take it at their next start).
fn set_bitrate(enc: &gst::Element, kbps: u32) {
    let name = enc.factory().map(|f| f.name().to_string()).unwrap_or_default();
    match name.as_str() {
        "vp8enc" | "vp9enc" => enc.set_property("target-bitrate", (kbps * 1000) as i32),
        "openh264enc" => enc.set_property("bitrate", kbps * 1000),
        "svtav1enc" => enc.set_property("target-bitrate", kbps),
        _ => enc.set_property("bitrate", kbps),
    }
}

fn dmabuf_size(fd: &OwnedFd) -> Result<usize> {
    let mut file = std::fs::File::from(fd.try_clone()?);
    Ok(file.seek(SeekFrom::End(0))? as usize)
}

fn drm_format_string(fourcc: u32, modifier: u64) -> String {
    let code = String::from_utf8_lossy(&fourcc.to_le_bytes()).into_owned();
    if modifier == 0 { code } else { format!("{code}:0x{modifier:016x}") }
}

/// `(fourcc, modifier)` pairs the encoders take: what the compositor may render into. With `software`,
/// linear XR24 and AR24, which the CPU maps; else what the device's vapostproc advertises for
/// `memory:DMABuf` input.
pub fn accepted_formats(prefix: &str, software: bool) -> Vec<(u32, u64)> {
    if gst::init().is_err() {
        return vec![];
    }
    if software {
        return vec![(u32::from_le_bytes(*b"XR24"), 0), (u32::from_le_bytes(*b"AR24"), 0)];
    }
    let Some(factory) = gst::ElementFactory::find(&format!("{prefix}postproc")) else { return vec![] };
    let mut out = vec![];
    for tmpl in factory.static_pad_templates() {
        if tmpl.direction() != gst::PadDirection::Sink {
            continue;
        }
        for (s, features) in tmpl.caps().iter_with_features() {
            if !features.contains("memory:DMABuf") {
                continue;
            }
            let Ok(v) = s.value("drm-format") else { continue };
            let names: Vec<String> = match v.get::<gst::List>() {
                Ok(list) => list.iter().filter_map(|x| x.get::<String>().ok()).collect(),
                Err(_) => v.get::<String>().into_iter().collect(),
            };
            out.extend(names.iter().filter_map(|n| gst_video::dma_drm_fourcc_from_str(n).ok()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_high_level_42() {
        let au = [0, 0, 0, 1, 0x09, 0xf0, 0, 0, 0, 1, 0x67, 0x64, 0x00, 0x2a, 0xac];
        assert_eq!(codec_string(Codec::H264, &au, 1920, 1080).as_deref(), Some("avc1.64002A"));
    }

    #[test]
    fn parses_hevc_main_level_4() {
        // VPS then SPS: profile_space 0, tier L, profile 1 (Main), compat flags 0x60000000, progressive+frame_only, level 120
        let mut au = vec![0, 0, 0, 1, 0x40, 0x01, 0x0c];
        // constraint bytes `90 00 00 00 00 00` appear escaped on the wire: `90 00 00 03 00 00 03 00`
        au.extend([0, 0, 0, 1, 0x42, 0x01, 0x01, 0x01, 0x60, 0, 0, 0x03, 0, 0x90, 0, 0, 0x03, 0, 0, 0x03, 0, 120, 0xa0]);
        assert_eq!(codec_string(Codec::Hevc, &au, 1920, 1080).as_deref(), Some("hev1.1.6.L120.90"));
        assert_eq!(codec_string(Codec::Vp9, &au, 2560, 1440).as_deref(), Some("vp09.00.51.08"));
    }

    #[test]
    fn drm_format_strings() {
        assert_eq!(drm_format_string(u32::from_le_bytes(*b"AR24"), 0x0100000000000009), "AR24:0x0100000000000009");
        assert_eq!(drm_format_string(u32::from_le_bytes(*b"XR24"), 0), "XR24");
    }
}
