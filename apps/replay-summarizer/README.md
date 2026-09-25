# replay-summarizer

Generates summaries and timestamped UX pain points from recorded user sessions.
Recordings must contain at least two seconds of replay time, including manually
requested summaries.

## How it works

1. Claims automatic or manually requested jobs from PostgreSQL.
2. Downloads and decodes rrweb recordings from S3, orders events and prepares interaction evidence.
3. Sends events to the [replay-renderer](../replay-renderer/README.md) and receives a temporary video.
4. Sends the video and evidence to OpenRouter for analysis.
5. Batch-embeds the resulting pain points and assigns them to project-local insight groups.
6. Atomically saves the summary, pain points, embeddings, memberships and job success.

Each worker processes one replay at a time and renews its lease while working.
The renderer is a separate service accessed through an authenticated API; the
worker contains no Chromium or FFmpeg. Busy renderer responses return the job to
the queue without consuming a failed attempt. Temporary videos are deleted after
processing.

Publication is fenced by the lease, recording revision, deletion state and storage
generation so outdated work cannot overwrite newer results. Failed jobs can retry,
but successful intermediate outputs are not durably checkpointed: a later failure
can repeat rendering or inference.

## Analysis

The default summary model is `google/gemini-3.8-flash`. The instructions in
[prompt.md](prompt.md) account for replay limitations, such as missing canvas
charts, to reduce false bug reports. Responses use a structured schema, with
local checks for text lengths, array sizes and recording-duration bounds.

The default is 3 FPS at 1× playback for all recording lengths, preserving one
rendered frame every 333 ms of original activity. `REPLAY_RENDER_FPS` and
`REPLAY_RENDER_SPEED` can override these settings. Idle time is retained, and the selected settings are saved with the
summary. Higher frame density does not guarantee the provider examines every frame.
Real-time playback increases processing cost and video size for long recordings;
the existing 64 MiB video limit still applies.

Summaries explain causes supported by visible messages or demonstrated interactions,
while distinguishing application-reported reasons from verified observations and
omitting unsupported technical explanations.

The video is accompanied by up to 500 recorded click, touch, scroll and input-change
events. This evidence excludes URLs, DOM text and entered values, and marks
truncation. It distinguishes interactions such as touch scrolling from clicks
without claiming that an action succeeded or failed.

Malformed or truncated summaries and HTTP 4xx errors other than 429 are not
retried as whole jobs. Connection failures, timeouts, 429s and server errors remain
retryable.

## Grouping pain points

Pain points are embedded in one batch per summary. The default embedding model is
`qwen/qwen3-embedding-8b`; canonical preparation uses `ux-point-canonical-v2-1024`.
The first 1,024 dimensions are normalized before storage. Different models or
preparation versions are never compared.

Exact pgvector search retrieves up to four project-local representatives with
cosine similarity of at least 0.65. Each representative is the oldest current
member of its group; member vectors are not averaged. `typesafe/jev-1.13` compares
all candidates in one request per pain point. A score of at least 0.8 permits a
join; otherwise the observation starts a new group. These scores are not
calibrated probabilities.

Points are assigned sequentially so later points see groups created by earlier
ones. Embedding happens before publication while the lease is renewed. Grouping
runs inside the publication transaction and project lock, with a five-second
total deadline and no retries. Failed, malformed or timed-out decisions leave
remaining observations separate rather than falling back to vector-only joins.

## Stored results

`replay_summaries` stores the recording revision, summary text, confidence, model,
response ID, prompt/schema version, provider cost and token counts, latency and
render settings. `replay_summary_pain_points` stores ordered timestamped findings,
evidence and confidence.

Missing provider accounting remains null rather than becoming zero. Cost and
token fields describe the successful summary call, not failed attempts. Job
reports retain rendering diagnostics without duplicating summary text or model
metadata. Confidence is a model estimate, not a calibrated probability.
