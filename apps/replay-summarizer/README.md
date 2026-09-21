# replay-summarizer

One deployed summarizer runs two independent loops: Kafka dispatch and one render
worker. `final-replay-v1` remains the durable notification stream. Dispatch marks
the existing outbox job ready in PostgreSQL before acknowledging Kafka; rendering
never occupies a Kafka partition. No additional service is needed.

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

Publication and consumption are at least once; duplicate descriptors do not
restart jobs. The outbox republishes jobs still awaiting dispatch after five
minutes. Rendering retries after one minute, then five minutes, up to three
attempts. Invalid input is terminal. Errors remain in `last_error`.

To intentionally retry an inspected failed job, set `state='ready'`,
`processed=false`, `processed_at=NULL`, `attempts=0`, and
`next_attempt_at=NOW()` on that specific job. Do not reset running jobs or Kafka
offsets. Old jobs without `kafka_triggered` are never discovered or retried.

Structured logs report the job ID, stage, frame/capture counts, download time,
render stage timings and total processing time. Every 30 seconds an index-backed
probe reports `ready_due_age_seconds` and `expired_lease_age_seconds`.
Kafka lag measures dispatch backlog; it no longer measures unfinished renders.
Use queue age and the per-job timings to diagnose capacity or stalls.

## Deployment order

1. Apply `20260921092354_replay_worker_leases` through the **monorepo database
   package**. There are no migrations in this Rust repository.
2. Stop the old summarizer before starting the new one: the old implementation
   does not honor execution leases. Deploy the consumer and summarizer images,
   then the collector's generation-aware patch messages. Existing messages remain
   compatible. Pending outbox jobs recover through republication.
3. Keep the existing database, S3 and Kafka credentials. `DATABASE_URL` is required;
   `DATABASE_MAX_CONNECTIONS` remains bounded. Topic and group overrides are
   `FINAL_REPLAY_KAFKA_TOPIC` and `REPLAY_SUMMARIZER_KAFKA_GROUP_ID`.
4. Check successful dispatch, queue age and a completed render report. Invalid
   descriptors are dropped with a topic/partition/offset warning and acknowledged.

Keep one instance of each existing service. No infrastructure changes are required.

Build from the repository root with `apps/replay-summarizer/Dockerfile`.
No production migration or deployment is performed by building the image.
