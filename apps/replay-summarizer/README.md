# replay-summarizer

One deployed summarizer runs one render worker and a queue-health observer.
The replay consumer atomically creates ready jobs in PostgreSQL when a recording
revision is finalized. PostgreSQL is the durable queue and execution ledger; the
summarizer has no Kafka dependency or credentials.

The worker claims a job using `FOR UPDATE SKIP LOCKED`, then releases the database
connection before downloading or rendering. Each claim has a 120-second lease and
an execution token. Renewal runs every 30 seconds. Expired attempts are reclaimed;
stale workers cannot renew or publish results. Selection settings, storage
generation, deletion and recording revision are checked before rendering and
before accepting success. Summary storage and job completion commit atomically.
Manual requests set `manual=true, priority=100`, bypass automatic selection, and
are claimed before normal priority jobs. Repeated requests promote the existing
revision job without starting a second concurrent attempt. Existing current
summaries are returned without another model call. New chunks make a stored
summary stale; the API and `hasSummary` filter exclude stale summaries.

Rendering runs in a supervised child process at **3 FPS, 8×, full recorded
viewport, H.264 CRF 23**. FFmpeg writes a temporary MP4 with a footer showing
original replay milliseconds, including idle time. The child sends that actual
video to OpenRouter's `z-ai/glm-5.3-flash` using strict JSON Schema output:
`{ "summary": string, "painPoints": [{ "timestampMs": integer, "description": string }] }`.
Timestamps are relative to the first rrweb event, never accelerated video time.
Responses are validated (including timestamp bounds) before publication. The MP4
is removed after the attempt; no video is persisted or made publicly accessible.
`OPENROUTER_API_KEY` is required. Requests time out after five minutes and videos
larger than 64 MiB are rejected; neither case produces a fake summary. The child reuses Chromium with a new isolated browser context
for every recording, recycling after 50 renders or observed memory pressure.
Downloads overlap up to four objects within the input budget. A 60-second progress
watchdog and 30-minute attempt deadline terminate the renderer process group,
including Chromium and FFmpeg.

`REPLAY_MAX_DECODED_BYTES` defaults to 32 MiB per recording. This limits input,
not Chromium DOM memory. The worker executes one recording at a time. There is no
automatic replica or instance-size increase.

## Completion, retries and monitoring

An explicit terminal signal starts a 10-second grace period, checked every five
seconds. Without a terminal signal the consumer retains the 35-minute inactivity
fallback, longer than the SDK's 30-minute session timeout. Late data can create a
new recording revision. Browser exit delivery remains best effort.

The unique recording revision prevents duplicate jobs. Rendering retries after
one minute, then five minutes, up to three attempts. Invalid input is terminal.
Errors remain in `last_error`. Expired execution leases recover interrupted work.

To intentionally retry an inspected failed job, set `state='ready'`,
`processed=false`, `processed_at=NULL`, `attempts=0`, and
`next_attempt_at=NOW()` on that specific job. Do not reset running jobs.
Historical scanner jobs are marked terminal by the queue migration and are not
automatically retried.

Structured logs report the job ID, stage, frame/capture counts, download time,
render stage timings and total processing time. Every 30 seconds an index-backed
probe reports `ready_due_age_seconds` and `expired_lease_age_seconds`.
Use PostgreSQL queue age and the per-job timings to diagnose capacity or stalls.
Kafka lag on the upstream replay-snapshot topic measures ingestion only.

## Deployment order

1. Stop the old replay consumer and summarizer before applying the schema change.
   The collector can continue queuing upstream replay commands during this pause.
2. Apply the monorepo database migration ending in `_replay_postgres_queue` (after
   `20260921092354_replay_worker_leases`). It promotes pending dispatch jobs to
   ready, preserves historical exclusions, and removes obsolete publication fields.
3. Deploy the updated consumer and summarizer. Do not restart old binaries against
   the new schema. Interrupted running jobs recover through lease expiry.
4. Apply the sibling infra changes to remove the completion topic and the
   summarizer's Kafka credentials/ACLs. It only needs the existing database and S3
   credentials. `DATABASE_URL` is required; `DATABASE_MAX_CONNECTIONS` is bounded.
5. Apply the monorepo migration ending in `_replay_ai_summaries` before deploying
   this worker and the summary API/UI. Set `OPENROUTER_API_KEY` on the summarizer.
   Stop older summarizer binaries before deploying: they only render and could
   consume summary jobs without producing summaries. Completed render-only jobs
   are not backfilled automatically; the manual button can requeue them.
6. Check queue age and a completed summary.

Keep one instance of each existing service. No infrastructure changes are required.

Build from the repository root with `apps/replay-summarizer/Dockerfile`.
No production migration or deployment is performed by building the image.
