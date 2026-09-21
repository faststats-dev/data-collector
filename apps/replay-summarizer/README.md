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
before accepting success.

Rendering runs in a supervised child process at **3 FPS, 8×, full recorded
viewport, H.264 CRF 23**. FFmpeg encodes to the null sink; model summarization is
not connected yet. The child reuses Chromium with a new isolated browser context
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
5. Check queue age and a completed render report.

Keep one instance of each existing service. No infrastructure changes are required.

Build from the repository root with `apps/replay-summarizer/Dockerfile`.
No production migration or deployment is performed by building the image.
