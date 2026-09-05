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
use bw_core::{Bytes, Codec, DmabufFrame, EncodedFrame, FrameSink, OutputGeometry, SinkError, StreamControl, StreamInfo, StreamMsg};
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
    bitrate_kbps: u32,
    codec: Codec,
}

/// Encoder + parser for a codec, producing one WebCodecs chunk per buffer.
fn encode_tail(codec: Codec, bitrate_kbps: u32) -> String {
    let common = format!("name=enc rate-control=cbr bitrate={bitrate_kbps} target-usage=7 ref-frames=1");
    match codec {
        Codec::H264 => format!(
            "vah264enc {common} b-frames=0 ! video/x-h264,profile=high ! h264parse config-interval=-1 \
             ! video/x-h264,stream-format=byte-stream,alignment=au"
        ),
        Codec::Hevc => format!(
            "vah265enc {common} b-frames=0 ! video/x-h265,profile=main ! h265parse config-interval=-1 \
             ! video/x-h265,stream-format=byte-stream,alignment=au"
        ),
        Codec::Vp9 => format!("vavp9enc {common} ! vp9parse ! video/x-vp9"),
    }
}

static STREAM_SEQ: AtomicU32 = AtomicU32::new(1);

/// Build `<head> ! vah264enc ! h264parse ! appsink` and start it. `head` must end at NV12/VAMemory.
fn build(head: &str, opts: EncodeOpts, tx: mpsc::Sender<StreamMsg>) -> Result<Stream> {
    let stream_id = STREAM_SEQ.fetch_add(1, Ordering::Relaxed);
    let desc = format!(
        "{head} ! {} ! appsink name=sink sync=false max-buffers=0",
        encode_tail(opts.codec, opts.bitrate_kbps)
    );
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

/// Opus-encodes whatever plays into `device` (a sink monitor); drop to stop.
pub struct AudioStream(gst::Pipeline);

impl Drop for AudioStream {
    fn drop(&mut self) {
        let _ = self.0.set_state(gst::State::Null);
    }
}

pub fn audio_source(device: &str, tx: mpsc::Sender<StreamMsg>) -> Result<AudioStream> {
    gst::init()?;
    let desc = format!(
        "pulsesrc device={device} buffer-time=40000 latency-time=10000 ! audio/x-raw,rate=48000,channels=2 \
         ! audioconvert ! audioresample ! opusenc bitrate=96000 frame-size=20 audio-type=generic dtx=true \
         ! appsink name=sink sync=false max-buffers=0"
    );
    let pipeline = gst::parse::launch(&desc)?.downcast::<gst::Pipeline>().expect("parse::launch returns a pipeline");
    let sink = pipeline.by_name("sink").context("sink element")?.downcast::<gst_app::AppSink>().unwrap();
    sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                let pts_us = buffer.pts().map(|t| t.useconds()).unwrap_or(0);
                let _ = tx.blocking_send(StreamMsg::Audio { pts_us, data: Bytes::copy_from_slice(&map) });
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    pipeline.set_state(gst::State::Playing)?;
    Ok(AudioStream(pipeline))
}

/// The real sink: compositor dmabufs → `appsrc`. Clone it for a keyframe handle before moving it into the compositor.
#[derive(Clone)]
pub struct GstSink(Arc<Mutex<Inner>>);

struct Inner {
    tx: mpsc::Sender<StreamMsg>,
    bitrate_kbps: u32,
    codec: Codec,
    accepted: Vec<(u32, u64)>,
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
}

impl GstSink {
    pub fn control(&self) -> GstControl {
        GstControl(Arc::downgrade(&self.0))
    }

    pub fn new(bitrate_kbps: u32, tx: mpsc::Sender<StreamMsg>) -> Result<GstSink> {
        gst::init()?;
        Ok(GstSink(Arc::new(Mutex::new(Inner {
            tx,
            bitrate_kbps,
            codec: Codec::H264,
            accepted: accepted_formats(),
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
}

/// Tear a pipeline down off the caller's thread: stopping it waits for streaming threads,
/// which may be waiting on the server, which may be waiting on us.
fn discard(old: (Option<(Stream, gst_app::AppSrc)>, HashMap<u32, gst::Memory>)) {
    if old.0.is_some() || !old.1.is_empty() {
        std::thread::spawn(move || drop(old));
    }
}

impl FrameSink for GstSink {
    fn accepted_formats(&self) -> Vec<(u32, u64)> {
        self.0.lock().unwrap().accepted.clone()
    }

    fn output_changed(&mut self, geo: OutputGeometry, fourcc: u32, modifier: u64) {
        let mut i = self.0.lock().unwrap();
        i.geo = Some((geo, fourcc, modifier));
        let old = i.take_stream();
        drop(i);
        discard(old);
    }

    fn submit(&mut self, frame: DmabufFrame) -> Result<(), SinkError> {
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
        let head = format!(
            "appsrc name=src is-live=true format=time do-timestamp=true block=false max-buffers=2 leaky-type=downstream \
             caps=\"video/x-raw(memory:DMABuf),format=DMA_DRM,drm-format={},width={},height={},framerate=60/1\" \
             ! vapostproc ! video/x-raw(memory:VAMemory),format=NV12,width={width},height={height}",
            drm_format_string(fourcc, modifier),
            geo.width_px,
            geo.height_px
        );
        let scale = geo.scale * width as f64 / geo.width_px as f64;
        let opts = EncodeOpts { width, height, scale, bitrate_kbps: self.bitrate_kbps, codec: self.codec };
        let stream = build(&head, opts, self.tx.clone())?;
        let appsrc = stream.pipeline.by_name("src").unwrap().downcast::<gst_app::AppSrc>().unwrap();
        self.stream = Some((stream, appsrc));
        Ok(())
    }

    fn push(&mut self, frame: DmabufFrame) -> Result<()> {
        if self.stream.is_none() {
            self.start()?;
        }
        let mem = match self.mems.get(&frame.slot_id) {
            Some(m) => m.clone(), // frame.fd (a dup) is closed on drop; GStreamer already holds one
            None => {
                let size = dmabuf_size(&frame.fd)?;
                // Safety: the fd is a dmabuf and GStreamer takes ownership of it.
                let m = unsafe { self.alloc.alloc_dmabuf(frame.fd, size) }?;
                if self.mems.len() >= 8 {
                    self.mems.clear(); // slots get replaced now and then; in-flight buffers hold their own refs
                }
                self.mems.insert(frame.slot_id, m.clone());
                m
            }
        };
        let format = gst_video::dma_drm_fourcc_to_format(frame.fourcc).context("unmappable fourcc")?;
        let mut buffer = gst::Buffer::new();
        {
            let b = buffer.get_mut().unwrap();
            b.append_memory(mem);
            gst_video::VideoMeta::add_full(b, gst_video::VideoFrameFlags::empty(), format, frame.width, frame.height, &[frame.offset as usize], &[frame.stride as i32])?;
            lease::attach(b, frame.lease);
        }
        let (_, appsrc) = self.stream.as_ref().unwrap();
        appsrc.push_buffer(buffer)?; // leaky appsrc drops on its own; the lease then frees the slot
        Ok(())
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

/// `(fourcc, modifier)` pairs vapostproc advertises for `memory:DMABuf` input: what the compositor may render into.
pub fn accepted_formats() -> Vec<(u32, u64)> {
    if gst::init().is_err() {
        return vec![];
    }
    let Some(factory) = gst::ElementFactory::find("vapostproc") else { return vec![] };
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
