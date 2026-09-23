# replay-renderer

Turns rrweb recordings into H.264 MP4 video using Chromium and FFmpeg.
The [replay-summarizer](../replay-summarizer/README.md) sends recorded events and
render settings to this service, then uses the returned video for analysis.
The renderer does not access storage, databases or model providers.

## Request handling

`POST /v1/render` accepts versioned JSON events, frame rate and playback speed,
authenticated with a bearer token. It streams NDJSON progress messages, video
chunks and a completion report. Shared types live in `crates/replay-render-protocol`.

Each instance accepts one render at a time. Busy requests receive HTTP 429 rather
than waiting in memory. Malformed recordings are rejected before rendering.
Inputs and video outputs are each limited to 64 MiB; individual response messages
are limited to 128 KiB. The render slot is released before the summarizer begins
inference.

## Rendering pipeline

1. Validate event ordering, viewport metadata and the presence of a full snapshot.
2. Launch Chromium with a fresh profile and load the local rrweb player.
3. Reconstruct the recorded DOM and advance replay time explicitly.
4. Capture changed frames and reuse pixels while the page is known to be static.
5. Stream timestamped JPEGs to FFmpeg and encode a constant-frame-rate H.264 video.
6. Return the video and render report, then clean up the job.

Event payloads stay as raw JSON in Rust. They are transferred to Chromium in
batches, where rrweb rebuilds the DOM and applies mutations, scrolls and pointer
movements. The largest recorded metadata dimensions define the capture viewport.

Replay time comes from the frame index:

```text
replay_time_ms = min(frame_index × 1000 × speed / fps, replay_duration_ms)
```

A virtual clock controls timers, animation callbacks and time APIs. Playback
advances only when instructed, so a busy CPU slows conversion without skipping
replay time. The final sample includes the recording's terminal state.

Chromium's `HeadlessExperimental.beginFrame` captures JPEG frames. After two
identical captures, pixels can be reused until an event, DOM mutation or resource
change invalidates them. Dynamic or unfamiliar resources keep capture active.
Idle ticks can be batched while still executing every logical timer tick.

A bounded writer thread streams timestamped JPEGs through Matroska to FFmpeg.
Backpressure limits how far capture can advance. FFmpeg expands unchanged
intervals, pads odd dimensions and encodes `yuv420p` H.264 using x264, CRF 23,
`veryfast` and `zerolatency`. Thread budgets follow available CPU parallelism.
The timestamp footer is applied after frame expansion so reused frames show the
correct replay time.

Output is published only after encoding succeeds, without overwriting existing
files. The report contains the output path, frame count, video duration and stage
timings. A local `render-replay` CLI also accepts recordings and explicit asset
paths. The encoding code supports discarding output for benchmarks.

## Isolation and cleanup

The service discards inherited environment variables before accepting work.
Each job, Chromium and FFmpeg receive only local process settings. The API
process prevents children from reading its authentication token through process
memory or `/proc`.

Each job gets a fresh process group, browser profile and scratch directory.
Inherited Linux seccomp rules block TCP/UDP/IPv6 sockets, namespace creation,
process-group escape, ptrace and io_uring. Browser control uses local pipes.
Chromium's namespace sandbox is disabled; isolation relies on the process
restrictions and surrounding container boundary.

Process, file and CPU limits bound each job. Rendering has a 30-minute deadline,
with a 60-second stall timeout and 60-second upload/transfer limits. Disconnects,
timeouts and shutdown kill the process group, reap descendants and remove scratch
before another job is admitted. Startup removes abandoned scratch. Failed
isolation checks or incomplete process cleanup stop the service.

## Replay limitations

External resources are blocked, so remote stylesheets, images and fonts are
absent unless embedded in the recording. CSS animations, transitions, smooth
scrolling, cursor effects and caret blinking are disabled. Canvas replay and
audio are unsupported. The virtual clock controls the outer player; arbitrary
iframe/media clocks and identical pixels across browser versions are not guaranteed.
