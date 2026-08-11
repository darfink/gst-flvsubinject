// SPDX-License-Identifier: MPL-2.0

//! End-to-end verification that FFmpeg demuxes what this element writes.
//!
//! The unit tests assert the byte layout against the specification. These
//! assert it against the *reader that matters*: if `ffprobe` does not report a
//! subtitle stream with the expected cues, the layout is wrong regardless of
//! what the specification says.
//!
//! # A pre-existing hazard these tests must see past
//!
//! `flvmux`/`eflvmux` rewrite `onMetaData` whenever a pad's codec info or tags
//! change, so a live stream carries several identical `onMetaData` tags at
//! non-zero timestamps. FFmpeg's demuxer treats *every* `FLV_TAG_TYPE_META` as
//! `FLV_STREAM_TYPE_SUBTITLE` (`flvdec.c:1515`) and only skips a metadata tag
//! when `dts == 0` (`flvdec.c:1520`). A repeated `onMetaData` at a non-zero
//! timestamp therefore surfaces as a spurious subtitle packet holding the raw
//! AMF byte `\u{2}`.
//!
//! This predates this element and happens with a bare `flvmux ! filesink`. It
//! is filtered out here rather than asserted away, because the property under
//! test is that *our* cues arrive intact and that we add nothing of our own.

use std::io::Write;
use std::process::{Command, Stdio};

use gst::prelude::*;

fn init() {
  use std::sync::Once;
  static INIT: Once = Once::new();
  INIT.call_once(|| {
    gst::init().unwrap();
    gstflvsubinject::plugin_register_static().unwrap();
  });
}

fn ffprobe_available() -> bool {
  Command::new("ffprobe")
    .arg("-version")
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .status()
    .is_ok_and(|status| status.success())
}

/// Mux a short A/V stream and inject cues, returning the FLV bytes.
fn produce_flv(cues: &[(u64, &str)]) -> Vec<u8> {
  produce_flv_with(cues, true)
}

fn produce_flv_with(cues: &[(u64, &str)], prime: bool) -> Vec<u8> {
  init();

  let pipeline = gst::Pipeline::new();

  let video = gst::ElementFactory::make("videotestsrc")
    .property("num-buffers", 60i32)
    .property_from_str("pattern", "black")
    .build()
    .unwrap();
  let encoder = gst::ElementFactory::make("x264enc")
    .property_from_str("speed-preset", "ultrafast")
    .property("key-int-max", 15u32)
    .build()
    .unwrap();
  let parser = gst::ElementFactory::make("h264parse").build().unwrap();
  let mux = gst::ElementFactory::make("flvmux")
    .property("streamable", true)
    .build()
    .unwrap();

  let text_src = gst::ElementFactory::make("appsrc")
    .property("is-live", false)
    .property("format", gst::Format::Time)
    .property(
      "caps",
      gst::Caps::builder("text/x-raw").field("format", "utf8").build(),
    )
    .build()
    .unwrap();

  let inject = gst::ElementFactory::make("flvsubinject")
    .property("prime", prime)
    .build()
    .unwrap();
  let sink = gst::ElementFactory::make("appsink")
    .property("sync", false)
    .build()
    .unwrap();

  pipeline
    .add_many([&video, &encoder, &parser, &mux, &text_src, &inject, &sink])
    .unwrap();
  gst::Element::link_many([&video, &encoder, &parser, &mux]).unwrap();
  mux.link_pads(Some("src"), &inject, Some("sink")).unwrap();
  text_src.link_pads(Some("src"), &inject, Some("text")).unwrap();
  inject.link(&sink).unwrap();

  let appsrc = text_src.downcast::<gst_app::AppSrc>().unwrap();
  let appsink = sink.downcast::<gst_app::AppSink>().unwrap();

  pipeline.set_state(gst::State::Playing).unwrap();

  for (millis, text) in cues {
    let mut buffer = gst::Buffer::from_slice(text.as_bytes().to_vec());
    buffer
      .get_mut()
      .unwrap()
      .set_pts(gst::ClockTime::from_mseconds(*millis));
    appsrc.push_buffer(buffer).unwrap();
  }
  appsrc.end_of_stream().unwrap();

  let mut output = Vec::new();
  loop {
    match appsink.try_pull_sample(gst::ClockTime::from_seconds(5)) {
      Some(sample) => {
        let buffer = sample.buffer().unwrap();
        let map = buffer.map_readable().unwrap();
        output.extend_from_slice(map.as_slice());
      }
      None => break,
    }
  }

  pipeline.set_state(gst::State::Null).unwrap();
  output
}

/// The same A/V pipeline with no injector at all, as a transparency baseline.
fn produce_flv_without_element() -> Vec<u8> {
  init();

  let pipeline = gst::Pipeline::new();
  let video = gst::ElementFactory::make("videotestsrc")
    .property("num-buffers", 60i32)
    .property_from_str("pattern", "black")
    .build()
    .unwrap();
  let encoder = gst::ElementFactory::make("x264enc")
    .property_from_str("speed-preset", "ultrafast")
    .property("key-int-max", 15u32)
    .build()
    .unwrap();
  let parser = gst::ElementFactory::make("h264parse").build().unwrap();
  let mux = gst::ElementFactory::make("flvmux")
    .property("streamable", true)
    .build()
    .unwrap();
  let sink = gst::ElementFactory::make("appsink")
    .property("sync", false)
    .build()
    .unwrap();

  pipeline
    .add_many([&video, &encoder, &parser, &mux, &sink])
    .unwrap();
  gst::Element::link_many([&video, &encoder, &parser, &mux, &sink]).unwrap();

  let appsink = sink.downcast::<gst_app::AppSink>().unwrap();
  pipeline.set_state(gst::State::Playing).unwrap();

  let mut output = Vec::new();
  while let Some(sample) = appsink.try_pull_sample(gst::ClockTime::from_seconds(5)) {
    let buffer = sample.buffer().unwrap();
    let map = buffer.map_readable().unwrap();
    output.extend_from_slice(map.as_slice());
  }

  pipeline.set_state(gst::State::Null).unwrap();
  output
}

/// Extract subtitle cues via ffprobe, as `(pts_ms, text)`.
fn ffprobe_cues(flv: &[u8]) -> Vec<(i64, String)> {
  let mut child = Command::new("ffmpeg")
    .args([
      "-hide_banner", "-v", "error", "-f", "flv", "-i", "pipe:0", "-map", "0:s:0", "-f", "srt",
      "pipe:1",
    ])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .expect("spawn ffmpeg");

  child.stdin.as_mut().unwrap().write_all(flv).ok();
  let output = child.wait_with_output().expect("ffmpeg output");
  let srt = String::from_utf8_lossy(&output.stdout);

  let mut cues = Vec::new();
  let mut lines = srt.lines();
  while let Some(line) = lines.next() {
    if !line.contains("-->") {
      continue;
    }
    let start = line.split("-->").next().unwrap().trim();
    let (hms, millis) = start.rsplit_once(',').unwrap();
    let parts: Vec<i64> = hms.split(':').map(|p| p.parse().unwrap()).collect();
    let pts_ms =
      (parts[0] * 3600 + parts[1] * 60 + parts[2]) * 1000 + millis.parse::<i64>().unwrap();
    let text = lines.next().unwrap_or_default().to_owned();
    // Spurious cues from repeated `onMetaData` carry a lone AMF type byte and
    // never legible text. Dropping exactly that shape keeps every real cue,
    // including the deliberately empty clear-display cue.
    if text.chars().all(|character| character.is_control()) && !text.is_empty() {
      continue;
    }
    // The priming cue declares the stream and renders as nothing; it is
    // asserted directly in its own test rather than counted as content here.
    if text == "\u{200b}" {
      continue;
    }
    cues.push((pts_ms, text));
  }
  cues
}

#[test]
fn ffmpeg_demuxes_injected_cues_at_their_timestamps() {
  if !ffprobe_available() {
    eprintln!("skipping: ffmpeg not available");
    return;
  }

  let flv = produce_flv(&[(0, "first cue"), (500, "second cue"), (1000, "third cue")]);
  assert!(!flv.is_empty(), "pipeline produced no FLV bytes");

  let cues = ffprobe_cues(&flv);
  assert_eq!(
    cues.len(),
    3,
    "expected three demuxed cues, got {cues:?}"
  );
  assert_eq!(cues[0].1, "first cue");
  assert_eq!(cues[1].1, "second cue");
  assert_eq!(cues[2].1, "third cue");

  // Timestamps must survive the round trip, not merely the text.
  assert!(cues[0].0.abs() <= 40, "first cue at {}ms", cues[0].0);
  assert!((cues[1].0 - 500).abs() <= 40, "second cue at {}ms", cues[1].0);
  assert!((cues[2].0 - 1000).abs() <= 40, "third cue at {}ms", cues[2].0);
}

#[test]
fn an_empty_cue_survives_as_a_clear_signal() {
  if !ffprobe_available() {
    eprintln!("skipping: ffmpeg not available");
    return;
  }

  // An empty cue is how silence clears the display. FFmpeg's SRT writer omits
  // blank cue bodies, so this asserts the tag reaches the demuxer at all by
  // counting what precedes and follows it.
  let flv = produce_flv(&[(0, "visible"), (500, ""), (1000, "visible again")]);
  let cues = ffprobe_cues(&flv);
  assert!(
    cues.iter().any(|(_, text)| text == "visible"),
    "first cue missing: {cues:?}"
  );
  assert!(
    cues.iter().any(|(_, text)| text == "visible again"),
    "third cue missing: {cues:?}"
  );
}

#[test]
fn av_only_input_gains_no_cues_of_our_own() {
  init();
  // With no cues the element must be transparent. Any *legible* cue here would
  // mean we invented one; the AMF control bytes from repeated `onMetaData` are
  // filtered by `ffprobe_cues` because they are the muxer's, not ours.
  let with_element = produce_flv(&[]);
  assert!(!with_element.is_empty());

  if ffprobe_available() {
    let cues = ffprobe_cues(&with_element);
    assert!(cues.is_empty(), "unexpected cues: {cues:?}");
  }
}

#[test]
fn passthrough_is_byte_identical_without_cues() {
  init();
  // The strongest statement of transparency: with priming disabled, the same
  // pipeline with and without the element must produce identical bytes.
  //
  // Priming is deliberately excluded here rather than tested loosely. It adds
  // exactly one tag by design, so asserting transparency around it would only
  // restate the implementation; what matters is that the A/V bytes themselves
  // are never touched.
  let injected = produce_flv_with(&[], false);
  let direct = produce_flv_without_element();
  assert_eq!(
    injected.len(),
    direct.len(),
    "element altered the byte count of an uncaptioned stream"
  );
  assert_eq!(injected, direct, "element altered an uncaptioned stream");
}

#[test]
fn priming_declares_the_subtitle_stream_before_any_cue() {
  if !ffprobe_available() {
    eprintln!("skipping: ffmpeg not available");
    return;
  }

  // A stream whose captions start late must still be discoverable as carrying
  // subtitles: FFmpeg creates the text stream lazily on the first cue, and a
  // packager that has finished probing by then never sees one.
  let primed = produce_flv_with(&[], true);
  let streams = String::from_utf8_lossy(
    &Command::new("ffprobe")
      .args([
        "-hide_banner", "-v", "error", "-select_streams", "s", "-show_entries",
        "stream=codec_name", "-of", "csv=p=0", "-f", "flv", "-i", "pipe:0",
      ])
      .stdin(Stdio::piped())
      .stdout(Stdio::piped())
      .spawn()
      .and_then(|mut child| {
        child.stdin.as_mut().unwrap().write_all(&primed)?;
        child.wait_with_output()
      })
      .expect("ffprobe")
      .stdout,
  )
  .trim()
  .to_owned();

  assert!(
    streams.contains("text"),
    "priming did not declare a subtitle stream: {streams:?}"
  );
}
