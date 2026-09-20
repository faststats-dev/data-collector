# replay-summarizer

Infrastructure trial: consume `final-replay-v1`, read PostgreSQL selection settings,
fetch every S3 chunk for the recording revision, render H.264 at 10 fps / 8× speed,
and discard the encoded MP4 stream. No video file or complete-video buffer is
created. One recording runs at a time per worker; replay data, frame buffers,
Chromium and FFmpeg are released after processing.

## Deployment

1. Apply the new monorepo Drizzle migration before starting either replay worker.
2. Provision `final-replay-v1` and the worker/ACL changes in infra.
3. Set `REPLAY_S3_BUCKET_PREFIX`, `REPLAY_S3_ACCESS_KEY_ID`,
   `REPLAY_S3_SECRET_ACCESS_KEY`, and optionally `REPLAY_S3_ENDPOINT` and
   `REPLAY_S3_REGION`, using the same project-bucket configuration as replay-consumer.
4. Build `apps/replay-summarizer/Dockerfile` from the data-collector root.
5. Select Off (default), All, or an exact attribute filter in web project settings.
   For local Docker, use monorepo's `replay` Compose profile after applying migrations.

`DATABASE_URL` is required. Kafka settings match replay-consumer; topic override is
`FINAL_REPLAY_KAFKA_TOPIC`, group override is `REPLAY_SUMMARIZER_KAFKA_GROUP_ID`
(default `replay-summarizer-v1`). Configure
`DIGITALOCEAN_REPLAY_SUMMARIZER_APP_ID` in the admin app to populate its diagram metrics.

## Completion and delivery

`isFinal` is a browser hint, not a delivery guarantee. `beforeunload` can be
canceled and now only flushes; non-persisted pagehide, stop and rotation still
provide terminal hints. The consumer waits for 35 minutes without updates,
longer than the SDK's 30-minute inactivity session timeout, including when no
terminal signal arrives. `REPLAY_FINAL_IDLE_SECONDS` overrides this delay for
local testing (minimum 60 seconds). Lowering it in production can split active
recordings.

Each session/window, storage generation and chunk-count revision gets a durable
outbox job. Late chunks reopen the recording and create a new job after another
quiet period. The outbox is committed before Kafka publication. Duplicate
messages are serialized on the job row and skipped after processing. Old/deleted
storage revisions and disabled/nonmatching settings are skipped.

Only Kafka snapshot/terminal messages arm a durable `finalize_after` timer before
acknowledgement. Historical PostgreSQL rows have no timer and are never queued.
The migration leaves every existing job's `kafka_triggered` flag false, so jobs
from the former database scan are neither published nor processed/retried.
New Kafka groups start at `latest`; existing groups retain their committed offsets.
No automatic historical discovery or offset reset is performed.

Browser delivery remains best effort: inactivity cannot prove that every client
batch arrived. Empty terminal markers consume SDK sequence numbers without an S3
object, so sequence gaps alone cannot prove missing snapshots. All available
chunks of the revision are loaded and validated by the renderer.

## Measurements and retries

Logs report `replay_time_ms`, `download_seconds`, `render_seconds`,
`processing_seconds`, `video_seconds`, and `frames`. FFmpeg really encodes the
video; only its output is discarded. Rendering is offline: external images,
fonts and stylesheets are blocked, as described in the rrweb2video README.

Failed jobs retry after one minute, up to three attempts. They remain in
`replay_summary_jobs` with `last_error` for inspection and do not block a Kafka
partition. To retry a failed job, reset `processed = false`, `attempts = 0`, and
`next_attempt_at = NOW()` while retaining `last_error`. A crash after rendering
but before committing the ledger can cause a repeat render (at-least-once work).
