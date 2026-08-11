// SPDX-License-Identifier: MPL-2.0

//! Splices FLV script-data subtitle tags into an already-muxed FLV stream.
//!
//! # Why this sits after the muxer
//!
//! `flvmux`/`eflvmux` declare exactly two sink pad templates, `video` and
//! `audio`, and write exactly one script-data message, `onMetaData`. There is
//! no text pad to request and no hook for arbitrary AMF messages. The muxer's
//! output, however, is a flat sequence of self-delimiting tags, each stamped
//! with a rebased millisecond timestamp — so a correctly framed script-data tag
//! can simply be spliced between two existing tags.
//!
//! Placing this after the muxer has a second consequence that matters more than
//! the convenience: the text path never enters an aggregator. `cccombiner` and
//! `matroskamux` both block until every sink pad is non-empty, which is what
//! forces keepalive machinery onto sparse caption branches. This element polls
//! nothing and waits for nothing; a silent text pad costs zero bytes and zero
//! latency.
//!
//! # What this element is *not* responsible for
//!
//! Deliberately narrow. It does not wrap text, does not decide when a caption
//! is complete, does not clear the display on silence, and does not know that
//! speech recognition exists. Those are windowing decisions that belong to the
//! element producing the cues (`textrollup`), for the same reason that
//! `tttocea708` owns roll-up state and `h264ccinserter` owns only carriage.
//! This element owns exactly one thing: turning a timestamped string into a
//! correctly framed FLV tag at the right point in a byte stream.
//!
//! In particular, a cue whose text is empty is *not* filtered out here. An
//! empty cue is the transport's clear-display signal, and suppressing it would
//! silently strand the previous caption on screen forever.

use std::sync::Mutex;

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use crate::amf::{script_data_body, MessageName};
use crate::flv::{parse_tag_header, script_data_tag, MAXIMUM_TIMESTAMP_MS};

static CAT: std::sync::LazyLock<gst::DebugCategory> = std::sync::LazyLock::new(|| {
  gst::DebugCategory::new(
    "flvsubinject",
    gst::DebugColorFlags::empty(),
    Some("FLV script-data subtitle injection"),
  )
});

/// What to do with a cue whose timestamp the FLV stream has already passed.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, glib::Enum)]
#[enum_type(name = "GstFlvSubInjectLatePolicy")]
pub enum LatePolicy {
  /// Emit it at the current stream position rather than its own timestamp.
  ///
  /// A late caption displayed slightly early is legible; a dropped one is a
  /// gap in the transcript. This is the default because the cue text itself is
  /// still correct, only its placement has degraded.
  #[default]
  #[enum_value(name = "Clamp", nick = "clamp")]
  Clamp,
  /// Discard it.
  ///
  /// Appropriate when a downstream consumer has already served the time range,
  /// which is a judgement only that consumer can make.
  #[enum_value(name = "Drop", nick = "drop")]
  Drop,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, glib::Enum)]
#[enum_type(name = "GstFlvSubInjectMessageName")]
pub enum MessageNameProperty {
  #[default]
  #[enum_value(name = "onCaption", nick = "oncaption")]
  OnCaption,
  #[enum_value(name = "onTextData", nick = "ontextdata")]
  OnTextData,
}

impl From<MessageNameProperty> for MessageName {
  fn from(value: MessageNameProperty) -> Self {
    match value {
      MessageNameProperty::OnCaption => Self::OnCaption,
      MessageNameProperty::OnTextData => Self::OnTextData,
    }
  }
}

#[derive(Clone, Copy, Debug)]
struct Settings {
  message_name: MessageNameProperty,
  late_policy: LatePolicy,
  prime: bool,
}

impl Default for Settings {
  fn default() -> Self {
    Self {
      message_name: MessageNameProperty::default(),
      late_policy: LatePolicy::default(),
      prime: true,
    }
  }
}

/// Payload of the priming cue.
///
/// Not the empty string. A consumer that republishes these cues as WebVTT has
/// to reject empty cue text — WebVTT gives it no meaning, and a blank cue body
/// terminates the cue — so priming with `""` trades an undiscoverable stream
/// for a fatal muxing error further along.
///
/// U+200B ZERO WIDTH SPACE is a character for every validator that asks, and
/// renders as nothing for every viewer that sees it.
const PRIMING_TEXT: &str = "\u{200b}";

/// A cue waiting for the FLV stream to reach its timestamp.
#[derive(Clone, Debug)]
struct PendingCue {
  running_time: gst::ClockTime,
  text: String,
}

#[derive(Default)]
struct State {
  /// Cues not yet written, ordered by running time.
  pending: Vec<PendingCue>,
  /// Segments observed on each sink pad, for running-time conversion.
  ///
  /// Both timelines must be expressed in the same domain before they can be
  /// compared, and running time is the only one both branches share.
  flv_segment: Option<gst::FormattedSegment<gst::ClockTime>>,
  text_segment: Option<gst::FormattedSegment<gst::ClockTime>>,
  /// Bytes of a partially received tag, carried between chained buffers.
  ///
  /// `flvmux` emits whole tags, but nothing in the pad contract guarantees a
  /// buffer boundary falls on one, and a downstream `queue` may recombine
  /// them. Splicing mid-tag would corrupt the stream, so partial tags are held.
  partial: Vec<u8>,
  /// Whether the 9-byte FLV header has been consumed.
  header_seen: bool,
  /// Millisecond timestamp of the most recent tag forwarded.
  stream_position_ms: u32,
  /// Running time of the first A/V tag, used to rebase cue timestamps.
  ///
  /// `flvmux` writes tag timestamps relative to its own first timestamp, while
  /// the text pad's buffers carry pipeline running time. Calibrating from the
  /// first tag observed is what keeps the two in one domain: the alternative,
  /// assuming both start at zero, silently skews whenever the muxer's first
  /// buffer does not.
  origin: Option<gst::ClockTime>,
  /// Cues written, for diagnostics.
  written: u64,
  /// Cues discarded under [`LatePolicy::Drop`].
  dropped: u64,
  /// Whether the priming cue has been written.
  primed: bool,
}

impl State {
  fn running_time(
    segment: Option<&gst::FormattedSegment<gst::ClockTime>>,
    pts: gst::ClockTime,
  ) -> gst::ClockTime {
    segment
      .and_then(|segment| segment.to_running_time(pts))
      .unwrap_or(pts)
  }
}

pub struct FlvSubInject {
  flv_sink: gst::Pad,
  text_sink: gst::Pad,
  src: gst::Pad,
  settings: Mutex<Settings>,
  state: Mutex<State>,
}

impl FlvSubInject {
  /// Accept a cue from the text pad.
  ///
  /// This runs on the *text* streaming thread, which is not the thread carrying
  /// FLV bytes. It therefore only ever queues: it must never touch the tag
  /// stream or push anything downstream.
  ///
  /// The reason is the difference between this element and `cccombiner`.
  /// `cccombiner` is a `GstAggregator`, so alignment is structural: a single
  /// `aggregate()` call pulls from both pads on one thread, and nothing it
  /// emits can interleave or misorder. This element is a plain `GstElement`
  /// with two independently-scheduled chain functions, so that guarantee has
  /// to be built rather than inherited.
  ///
  /// It is built by making the FLV thread the only writer. If this thread also
  /// pushed, two failures would follow, and the first is far worse than the
  /// second:
  ///
  /// 1. **Framing.** The FLV thread emits whole tags, but it holds a partially
  ///    consumed buffer between them. A push from here can land between two
  ///    `src.push()` calls of a single chain invocation, splicing a script tag
  ///    into the middle of another tag's body. That does not lose one caption;
  ///    it desynchronizes the byte stream and every byte after it is garbage.
  /// 2. **Ordering.** Even when framing survives, a cue written without
  ///    reference to the current tag position can land after a later-timestamped
  ///    A/V tag, so a consumer that trusts tag order mis-times it.
  fn handle_text_buffer(&self, buffer: &gst::Buffer) -> Result<gst::FlowSuccess, gst::FlowError> {
    let Some(pts) = buffer.pts() else {
      gst::warning!(CAT, imp = self, "text buffer without PTS, dropping");
      return Ok(gst::FlowSuccess::Ok);
    };

    let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
    let text = match std::str::from_utf8(map.as_slice()) {
      Ok(text) => text.to_owned(),
      Err(error) => {
        gst::warning!(CAT, imp = self, "text buffer is not UTF-8: {error}");
        return Ok(gst::FlowSuccess::Ok);
      }
    };

    let mut state = self.state.lock().unwrap();
    let running_time = State::running_time(state.text_segment.as_ref(), pts);
    gst::log!(
      CAT,
      imp = self,
      "queued cue at {running_time} ({} bytes)",
      text.len()
    );
    state.pending.push(PendingCue { running_time, text });
    state
      .pending
      .sort_by_key(|cue: &PendingCue| cue.running_time);
    Ok(gst::FlowSuccess::Ok)
  }

  /// Serialize every queued cue the stream position has reached.
  ///
  /// Only ever called from the FLV streaming thread, which is what keeps the
  /// element single-writer. A cue therefore leaves the queue when the next tag
  /// arrives rather than the instant it is produced.
  ///
  /// That is a real latency bound, and it is the muxer's cadence rather than
  /// anything this element adds: video at 30fps means a tag roughly every
  /// 33ms, and audio adds more. A caption is delayed by at most one tag
  /// interval, which is far below the delay the caption pipeline already
  /// carries to align text with A/V. Draining eagerly from the text thread
  /// would trade that bounded delay for a corrupt byte stream.
  fn drain_ready(&self, state: &mut State) -> Vec<gst::Buffer> {
    let Some(origin) = state.origin else {
      return Vec::new();
    };
    let settings = *self.settings.lock().unwrap();

    let position_ms = u64::from(state.stream_position_ms);
    let mut tags = Vec::new();
    let mut remaining = Vec::new();

    for cue in std::mem::take(&mut state.pending) {
      let cue_ms = cue
        .running_time
        .saturating_sub(origin)
        .mseconds()
        .min(MAXIMUM_TIMESTAMP_MS);

      if cue_ms > position_ms {
        remaining.push(cue);
        continue;
      }

      // The cue's own time has passed. Whether that is "late" depends on
      // whether the stream moved beyond it, which only matters for a consumer
      // that has already served the range.
      let timestamp_ms = if cue_ms < position_ms {
        match settings.late_policy {
          LatePolicy::Drop => {
            state.dropped += 1;
            gst::debug!(
              CAT,
              imp = self,
              "dropping cue {cue_ms}ms behind stream position {position_ms}ms"
            );
            continue;
          }
          LatePolicy::Clamp => position_ms,
        }
      } else {
        cue_ms
      };

      let body = script_data_body(settings.message_name.into(), &cue.text, None);
      let tag = script_data_tag(
        u32::try_from(timestamp_ms).unwrap_or(u32::MAX),
        &body,
      );
      state.written += 1;
      tags.push(gst::Buffer::from_mut_slice(tag));
    }

    state.pending = remaining;
    tags
  }

  fn push_tags(&self, tags: Vec<gst::Buffer>) -> Result<gst::FlowSuccess, gst::FlowError> {
    for tag in tags {
      self.src.push(tag)?;
    }
    Ok(gst::FlowSuccess::Ok)
  }

  /// Forward FLV bytes, splicing queued cues at tag boundaries.
  fn handle_flv_buffer(&self, buffer: gst::Buffer) -> Result<gst::FlowSuccess, gst::FlowError> {
    let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;

    let mut state = self.state.lock().unwrap();
    state.partial.extend_from_slice(map.as_slice());
    drop(map);

    // Calibrate the cue timeline against the muxer's, once.
    //
    // The FLV header and the `onMetaData` tag that follow it are generated
    // rather than derived from media, and `flvmux` pushes them with no PTS at
    // all. Only buffers carrying media tags are timestamped, so calibration
    // waits for the first of those instead of the first buffer: keying off the
    // first buffer leaves the origin unset forever and silently strands every
    // cue in the queue.
    if state.origin.is_none()
      && let Some(pts) = buffer.pts()
    {
      let running_time = State::running_time(state.flv_segment.as_ref(), pts);
      gst::debug!(CAT, imp = self, "calibrated cue origin at {running_time}");
      state.origin = Some(running_time);
    }

    let mut output: Vec<gst::Buffer> = Vec::new();
    let mut consumed = 0usize;
    let (settings_prime, message_name) = {
      let settings = self.settings.lock().unwrap();
      (settings.prime, MessageName::from(settings.message_name))
    };

    // The 9-byte FLV header plus its zero PreviousTagSize precede all tags.
    if !state.header_seen {
      const FLV_HEADER_LEN: usize = 9 + 4;
      if state.partial.len() < FLV_HEADER_LEN {
        return Ok(gst::FlowSuccess::Ok);
      }
      output.push(gst::Buffer::from_mut_slice(
        state.partial[..FLV_HEADER_LEN].to_vec(),
      ));
      consumed = FLV_HEADER_LEN;
      state.header_seen = true;
    }

    loop {
      let Some(header) = parse_tag_header(&state.partial[consumed..]) else {
        break;
      };
      let total = header.total_len();
      if state.partial.len() - consumed < total {
        break;
      }

      // Cues belonging before this tag go out ahead of it, so the output stays
      // ordered by timestamp.
      state.stream_position_ms = header.timestamp_ms;

      // Declare the subtitle timeline at the very start of the stream.
      //
      // FLV has no track table: a script-data subtitle stream exists only once
      // its first cue has been seen. A demuxer that finishes probing before
      // then concludes the stream has no captions and never revisits it —
      // FFmpeg's `flv_data_packet` creates the `AV_CODEC_ID_TEXT` stream
      // lazily, and a packager built on it reports zero subtitle tracks.
      //
      // Speech-derived captions always lose that race: the first cue cannot
      // appear until someone has spoken and the recognizer has committed a
      // word, which is seconds after the probe window closes.
      //
      // One invisible cue at the head of the stream fixes it, and is the exact
      // analogue of what the carriage this replaces does: CEA-708 declares its
      // service by sending null padding from the first frame, long before any
      // caption text exists.
      if settings_prime && !state.primed {
        let body = script_data_body(message_name, PRIMING_TEXT, None);
        output.push(gst::Buffer::from_mut_slice(script_data_tag(
          header.timestamp_ms,
          &body,
        )));
        state.primed = true;
        gst::debug!(
          CAT,
          imp = self,
          "primed subtitle stream at {}ms",
          header.timestamp_ms
        );
      }

      let ready = self.drain_ready(&mut state);
      output.extend(ready);

      output.push(gst::Buffer::from_mut_slice(
        state.partial[consumed..consumed + total].to_vec(),
      ));
      consumed += total;
    }

    if consumed > 0 {
      state.partial.drain(..consumed);
    }
    drop(state);

    self.push_tags(output)
  }
}

#[glib::object_subclass]
impl ObjectSubclass for FlvSubInject {
  const NAME: &'static str = "GstFlvSubInject";
  type Type = super::FlvSubInject;
  type ParentType = gst::Element;

  fn with_class(class: &Self::Class) -> Self {
    let flv_sink = gst::Pad::builder_from_template(&class.pad_template("sink").unwrap())
      .chain_function(|pad, parent, buffer| {
        FlvSubInject::catch_panic_pad_function(
          parent,
          || Err(gst::FlowError::Error),
          |this| this.handle_flv_buffer(buffer),
        )
        .inspect_err(|error| gst::debug!(CAT, obj = pad, "flv chain failed: {error}"))
      })
      .event_function(|pad, parent, event| {
        FlvSubInject::catch_panic_pad_function(
          parent,
          || false,
          |this| {
            if let gst::EventView::Segment(segment) = event.view()
              && let Some(segment) = segment.segment().downcast_ref::<gst::ClockTime>()
            {
              this.state.lock().unwrap().flv_segment = Some(segment.clone());
            }
            gst::Pad::event_default(pad, Some(&*this.obj()), event)
          },
        )
      })
      .build();

    let text_sink = gst::Pad::builder_from_template(&class.pad_template("text").unwrap())
      .chain_function(|_pad, parent, buffer| {
        FlvSubInject::catch_panic_pad_function(
          parent,
          || Err(gst::FlowError::Error),
          |this| this.handle_text_buffer(&buffer),
        )
      })
      .event_function(|_pad, parent, event| {
        FlvSubInject::catch_panic_pad_function(
          parent,
          || false,
          |this| {
            // The text pad is sparse and its stream events must not reach the
            // source pad: the output is FLV, and forwarding a text caps or EOS
            // event would either renegotiate the src pad or end the stream
            // while A/V is still flowing.
            use gst::EventView;
            match event.view() {
              EventView::Segment(segment) => {
                if let Some(segment) = segment.segment().downcast_ref::<gst::ClockTime>() {
                  this.state.lock().unwrap().text_segment = Some(segment.clone());
                }
                true
              }
              EventView::Caps(_) | EventView::StreamStart(_) => true,
              EventView::Eos(_) => {
                gst::debug!(CAT, imp = this, "text stream ended; FLV continues");
                true
              }
              EventView::Gap(_) => true,
              _ => true,
            }
          },
        )
      })
      .build();

    let src = gst::Pad::builder_from_template(&class.pad_template("src").unwrap()).build();

    Self {
      flv_sink,
      text_sink,
      src,
      settings: Mutex::new(Settings::default()),
      state: Mutex::new(State::default()),
    }
  }
}

impl ObjectImpl for FlvSubInject {
  fn properties() -> &'static [glib::ParamSpec] {
    static PROPERTIES: std::sync::OnceLock<Vec<glib::ParamSpec>> = std::sync::OnceLock::new();
    PROPERTIES.get_or_init(|| {
      vec![
        glib::ParamSpecEnum::builder::<MessageNameProperty>("message-name")
          .nick("Message name")
          .blurb("AMF0 script-data message name carrying each cue")
          .default_value(MessageNameProperty::default())
          .mutable_ready()
          .build(),
        glib::ParamSpecEnum::builder::<LatePolicy>("late-policy")
          .nick("Late cue policy")
          .blurb("What to do with a cue the FLV stream has already passed")
          .default_value(LatePolicy::default())
          .mutable_playing()
          .build(),
        glib::ParamSpecBoolean::builder("prime")
          .nick("Prime subtitle stream")
          .blurb(
            "Write one empty cue at the start so demuxers discover the \
             subtitle stream before any caption text exists",
          )
          .default_value(true)
          .mutable_ready()
          .build(),
      ]
    })
  }

  fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
    let mut settings = self.settings.lock().unwrap();
    match pspec.name() {
      "message-name" => settings.message_name = value.get().expect("message-name"),
      "late-policy" => settings.late_policy = value.get().expect("late-policy"),
      "prime" => settings.prime = value.get().expect("prime"),
      other => unimplemented!("set {other}"),
    }
  }

  fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
    let settings = self.settings.lock().unwrap();
    match pspec.name() {
      "message-name" => settings.message_name.to_value(),
      "late-policy" => settings.late_policy.to_value(),
      "prime" => settings.prime.to_value(),
      other => unimplemented!("get {other}"),
    }
  }

  fn constructed(&self) {
    self.parent_constructed();
    let obj = self.obj();
    obj.add_pad(&self.flv_sink).unwrap();
    obj.add_pad(&self.text_sink).unwrap();
    obj.add_pad(&self.src).unwrap();
  }
}

impl GstObjectImpl for FlvSubInject {}

impl ElementImpl for FlvSubInject {
  fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
    static METADATA: std::sync::OnceLock<gst::subclass::ElementMetadata> =
      std::sync::OnceLock::new();
    Some(METADATA.get_or_init(|| {
      gst::subclass::ElementMetadata::new(
        "FLV subtitle injector",
        "Muxer/Subtitle",
        "Injects timed text into a muxed FLV stream as onCaption/onTextData script data",
        "Elliott Darfink <elliott.darfink@gmail.com>",
      )
    }))
  }

  fn pad_templates() -> &'static [gst::PadTemplate] {
    static TEMPLATES: std::sync::OnceLock<Vec<gst::PadTemplate>> = std::sync::OnceLock::new();
    TEMPLATES.get_or_init(|| {
      let flv_caps = gst::Caps::builder("video/x-flv").build();
      let text_caps = gst::Caps::builder("text/x-raw")
        .field("format", "utf8")
        .build();
      vec![
        gst::PadTemplate::new(
          "sink",
          gst::PadDirection::Sink,
          gst::PadPresence::Always,
          &flv_caps,
        )
        .unwrap(),
        gst::PadTemplate::new(
          "text",
          gst::PadDirection::Sink,
          gst::PadPresence::Always,
          &text_caps,
        )
        .unwrap(),
        gst::PadTemplate::new(
          "src",
          gst::PadDirection::Src,
          gst::PadPresence::Always,
          &flv_caps,
        )
        .unwrap(),
      ]
    })
  }

  fn change_state(
    &self,
    transition: gst::StateChange,
  ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
    if transition == gst::StateChange::ReadyToPaused {
      *self.state.lock().unwrap() = State::default();
    }
    let result = self.parent_change_state(transition)?;
    if transition == gst::StateChange::PausedToReady {
      let state = self.state.lock().unwrap();
      gst::debug!(
        CAT,
        imp = self,
        "wrote {} cues, dropped {}, {} still queued",
        state.written,
        state.dropped,
        state.pending.len()
      );
    }
    Ok(result)
  }
}
