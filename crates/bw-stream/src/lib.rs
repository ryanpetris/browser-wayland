//! GStreamer capture → VA-API H.264 encode → encoded frames on a channel.
//!
//! Two front ends share one encode tail:
//! - [`fake_source`] feeds `videotestsrc` (no compositor needed) for end-to-end testing.
//! - [`GstSink`] (a [`bw_core::FrameSink`], added with the compositor) feeds real dmabufs.

use std::sync::{
    Mutex,
    atomic::{AtomicU32, Ordering},
};

use anyhow::{Context, Result};
use bw_core::{Bytes, EncodedFrame, StreamInfo, StreamMsg};
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use tokio::sync::mpsc;

/// A running pipeline. Dropping it stops the stream.
pub struct Stream {
    pipeline: gstreamer::Pipeline,
    encoder: gstreamer::Element,
}

impl Drop for Stream {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gstreamer::State::Null);
    }
}

impl Stream {
    /// Force the next frame to be a keyframe (IDR + SPS/PPS).
    pub fn request_keyframe(&self) {
        let ev = gstreamer_video::UpstreamForceKeyUnitEvent::builder().all_headers(true).build();
        self.encoder.send_event(ev);
    }
}

/// Options shared by both front ends.
pub struct EncodeOpts {
    pub width: u32,
    pub height: u32,
    pub bitrate_kbps: u32,
}

impl Default for EncodeOpts {
    fn default() -> Self {
        Self { width: 1920, height: 1080, bitrate_kbps: 8000 }
    }
}

/// Test source: an animated pattern, no compositor. Emits `StreamMsg` on `tx`.
pub fn fake_source(opts: EncodeOpts, tx: mpsc::Sender<StreamMsg>) -> Result<Stream> {
    gstreamer::init()?;
    let src = format!(
        "videotestsrc is-live=true pattern=ball ! video/x-raw,format=BGRA,width={},height={},framerate=60/1 \
         ! timeoverlay ! vapostproc ! video/x-raw(memory:VAMemory),format=NV12",
        opts.width, opts.height
    );
    build(&src, opts, tx)
}

static STREAM_SEQ: AtomicU32 = AtomicU32::new(1);

/// Build `<head> ! vah264enc ! h264parse ! appsink` and start it. `head` must end at NV12/VAMemory.
fn build(head: &str, opts: EncodeOpts, tx: mpsc::Sender<StreamMsg>) -> Result<Stream> {
    let stream_id = STREAM_SEQ.fetch_add(1, Ordering::Relaxed);
    let desc = format!(
        "{head} \
         ! vah264enc name=enc rate-control=cbr bitrate={} target-usage=7 b-frames=0 ref-frames=1 \
         ! video/x-h264,profile=high ! h264parse config-interval=-1 \
         ! video/x-h264,stream-format=byte-stream,alignment=au \
         ! appsink name=sink sync=false max-buffers=1 drop=true",
        opts.bitrate_kbps
    );
    let pipeline = gstreamer::parse::launch(&desc)?
        .downcast::<gstreamer::Pipeline>()
        .expect("parse::launch returns a pipeline");
    let encoder = pipeline.by_name("enc").context("enc element")?;
    let sink = pipeline.by_name("sink").context("sink element")?.downcast::<gst_app::AppSink>().unwrap();

    let scale = 1.0; // fake source has no HiDPI notion
    let info = Mutex::new(Some(StreamInfo { stream_id, codec: String::new(), width: opts.width, height: opts.height, scale }));

    sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gstreamer::FlowError::Eos)?;
                let buffer = sample.buffer().ok_or(gstreamer::FlowError::Error)?;
                let map = buffer.map_readable().map_err(|_| gstreamer::FlowError::Error)?;
                let keyframe = !buffer.flags().contains(gstreamer::BufferFlags::DELTA_UNIT);

                // First keyframe: derive the WebCodecs codec string from the SPS and announce the stream.
                if keyframe {
                    if let Some(mut info) = info.lock().unwrap().take() {
                        info.codec = codec_string(&map).unwrap_or_else(|| "avc1.640028".into());
                        let _ = tx.try_send(StreamMsg::Info(info));
                    }
                }
                let pts_us = buffer.pts().map(|t| t.useconds()).unwrap_or(0);
                let _ = tx.try_send(StreamMsg::Frame(EncodedFrame {
                    stream_id,
                    keyframe,
                    pts_us,
                    data: Bytes::copy_from_slice(&map),
                }));
                Ok(gstreamer::FlowSuccess::Ok)
            })
            .build(),
    );

    pipeline.set_state(gstreamer::State::Playing)?;
    Ok(Stream { pipeline, encoder })
}

/// `avc1.PPCCLL` from the first SPS in an Annex B access unit, per the WebCodecs AVC registration.
fn codec_string(au: &[u8]) -> Option<String> {
    // find a NAL with header (byte & 0x1f)==7 (SPS) after a start code
    let mut i = 0;
    while i + 4 < au.len() {
        let sc4 = au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 0 && au[i + 3] == 1;
        let sc3 = au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1;
        let (nal, hdr) = if sc4 { (i + 4, au.get(i + 4)?) } else if sc3 { (i + 3, au.get(i + 3)?) } else { i += 1; continue };
        if hdr & 0x1f == 7 {
            let p = au.get(nal + 1..nal + 4)?;
            return Some(format!("avc1.{:02X}{:02X}{:02X}", p[0], p[1], p[2]));
        }
        i = nal;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::codec_string;

    #[test]
    fn parses_high_level_42() {
        // 4-byte start code, SPS NAL (0x67), profile 0x64, constraints 0x00, level 0x2a
        let au = [0, 0, 0, 1, 0x67, 0x64, 0x00, 0x2a, 0xac];
        assert_eq!(codec_string(&au).as_deref(), Some("avc1.64002A"));
    }
}
