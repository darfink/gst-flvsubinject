# gst-flvsubinject

A GStreamer element that injects timed text into a muxed FLV stream as
`onCaption` / `onTextData` script data, so captions travel over RTMP as one
sparse subtitle timeline instead of being embedded in every video rendition.

```text
flvmux ! flvsubinject ! rtmp2sink
           ^
     text/x-raw,format=utf8
```

The element is `flvsubinject`; the plugin is `flvsubinject`.

## Why it sits after the muxer

`flvmux` and `eflvmux` declare exactly two sink pad templates, `video` and
`audio`, and write exactly one script-data message, `onMetaData`. There is no
text pad to request and no hook for arbitrary AMF messages, in any GStreamer
release through 1.28.5.

Their output, however, is a flat sequence of self-delimiting tags, each stamped
with a rebased millisecond timestamp. A correctly framed script-data tag can
simply be spliced between two existing tags, which is all this element does.

Sitting after the muxer has a second consequence that matters more than the
convenience: **the text path never enters an aggregator.** `cccombiner` and
`matroskamux` block until every sink pad is non-empty, which is what forces
keepalive machinery onto sparse caption branches. This element waits for
nothing, and silence costs zero bytes.

## Wire format

The layout is dictated by FFmpeg's `flv_data_packet()` in
`libavformat/flvdec.c`, which is the reader every consumer ultimately goes
through:

* the message name must be `onTextData`, `onCaption`, or `onCaptionInfo`;
* the payload must be an ECMA mixed array, object, or strict array;
* the cue text lives in a property named `text` holding an AMF0 string.

Two details of that reader constrain what is written. Property names are read
into a `char buf[20]`, and FFmpeg stops at the *first* `text` property it
finds, so exactly one is written, first.

`onCaption` is the default rather than the more familiar `onTextData`, because
`onTextData` reaches `avpriv_request_sample()` first and logs
`OnTextData packet is not implemented` once per cue before decoding it
correctly. Both reach the same branch and produce an `AV_CODEC_ID_TEXT`
subtitle stream.

## Cues have no duration

FFmpeg sets `pkt->dts = pkt->pts = dts` for these packets and never sets a
duration. The presentation model is therefore strictly **cue replacement**: a
cue displays until the next one arrives.

This is what `textrollup` already emits — each cue carries the entire window
and ends where the next begins — so the two compose without translation. It
also means silence must be expressed as a cue, not as an absence: whatever is
on screen stays there until something replaces it.

## Priming

FLV has no track table. A script-data subtitle stream exists only once its
first cue has been seen, and a demuxer that finishes probing before then
concludes the stream has no captions and never revisits it.

Speech-derived captions always lose that race: the first cue cannot appear
until someone has spoken and the recognizer has committed a word, which is
seconds after any reasonable probe window closes.

`prime` (default `true`) writes one invisible cue at the head of the stream to
declare the timeline. It is the exact analogue of what CEA-708 does by sending
null padding from the first frame, long before any caption text exists.

The priming payload is U+200B ZERO WIDTH SPACE rather than the empty string: a
consumer that republishes these cues as WebVTT has to reject empty cue text,
so priming with `""` trades an undiscoverable stream for a fatal muxing error
further along.

## Properties

| Property | Default | Meaning |
| --- | ---: | --- |
| `message-name` | `oncaption` | AMF0 message name: `oncaption` or `ontextdata` |
| `late-policy` | `clamp` | A cue the stream has passed: `clamp` to the current position, or `drop` |
| `prime` | `true` | Declare the subtitle stream with one invisible cue at the start |

## What this element does not do

Deliberately narrow. It does not wrap text, decide when a caption is complete,
clear the display on silence, or know that speech recognition exists. Those are
windowing decisions belonging to the element producing the cues, for the same
reason `tttocea708` owns roll-up state while `h264ccinserter` owns only
carriage.

In particular, a cue whose text is empty is **not** filtered out: an empty cue
is a caller's clear-display signal, and suppressing it would strand the
previous caption on screen.

## Build

```bash
cargo build --release
export GST_PLUGIN_PATH="$PWD/target/release"
gst-inspect-1.0 flvsubinject
```

## Tests

```bash
cargo test
```

Unit tests assert the byte layout against the specification. The round-trip
tests in `tests/roundtrip.rs` assert it against the reader that matters, by
muxing a real stream and requiring `ffmpeg` to demux the cues back at their
original timestamps.

### A pre-existing hazard the tests filter

`flvmux` rewrites `onMetaData` whenever a pad's codec info or tags change, so a
live stream carries several identical `onMetaData` tags at non-zero timestamps.
FFmpeg treats every `FLV_TAG_TYPE_META` as `FLV_STREAM_TYPE_SUBTITLE` and only
skips a metadata tag when `dts == 0`, so each repeat surfaces as a spurious
subtitle packet holding a lone AMF control byte.

This predates this element and reproduces with a bare `flvmux ! filesink`. The
tests filter exactly that shape rather than asserting it away.
