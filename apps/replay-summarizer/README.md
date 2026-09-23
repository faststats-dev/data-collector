# replay-summarizer

Generates summaries and timestamped UX pain points from recorded user sessions.

Summarization always requires at least 2,000 ms of recording duration, including
manual requests. Shorter recordings still finalize normally but are not queued;
already queued recordings are checked again before downloading or rendering.

## How it works

1. Picks up automatic or manually requested jobs from PostgreSQL.
2. Downloads rrweb recordings from S3 and renders them into a temporary video.
3. Sends the video to OpenRouter for analysis.
4. Batch-embeds pain points through OpenRouter (one request per summary).
5. Atomically saves the summary, pain points, embeddings, memberships, and job success.

Each instance processes one replay at a time. Failed jobs can retry, and outdated
recordings cannot overwrite newer results. Temporary videos are deleted after processing.
Grouping retrieves at most four project-local representatives with cosine similarity
of at least 0.65, using exact pgvector search in PostgreSQL. Each representative is
the oldest current member; it changes only when evidence becomes stale, is deleted,
or is manually moved. Member vectors are not loaded into the worker or averaged.
`typesafe/jev-1.13` compares all four candidates in one request per pain point.
Points are assigned sequentially so later points in a summary see the actual groups
created by earlier points, not speculative representatives. A score of at least 0.8 permits a join;
otherwise the observation starts a new group. Scores are not calibrated probabilities.

Embedding requests run before publication while the lease is renewed. Jev runs under
the existing publication transaction and project lock, with a **five-second total
deadline and no retries**. This briefly delays same-project publications and manual
edits, but avoids stale decisions and duplicate concurrent groups. Failed, malformed,
or timed-out decisions leave remaining observations separate; they do not fail the summary or
fall back to vector-only joins. There is no separate worker or regrouping service.

## Configuration

Required environment variables:

- `DATABASE_URL`: PostgreSQL connection string.
- `OPENROUTER_API_KEY`: API key for video analysis.
- `REPLAY_S3_BUCKET_PREFIX`: prefix for project replay buckets.
- `REPLAY_S3_ACCESS_KEY_ID` and `REPLAY_S3_SECRET_ACCESS_KEY`: storage credentials.

Set `REPLAY_S3_ENDPOINT` for S3-compatible storage and `REPLAY_S3_REGION` if needed
(default: `us-east-1`).

The embedding model defaults to `qwen/qwen3-embedding-8b` and may be overridden with
`REPLAY_INSIGHTS_EMBED_MODEL`. Use a model supporting Matryoshka truncation to 1024
dimensions. Canonical preparation is `ux-point-canonical-v2-1024`; the first 1024
dimensions are normalized before storage.
Embeddings from different models/preparation versions are never compared; switching
models requires re-embedding existing observations to match against their groups.
The cutoffs are provisional and must be reevaluated when changing models.

Before deploying the worker/API, provision PostgreSQL with pgvector and apply the
monorepo's consolidated `20260923092552_cross_replay_insights` migration. It creates
the extension and `vector(1024)` column; the migration role needs permission to create
the extension, or an administrator must create it first. Local Compose uses
`pgvector/pgvector:pg18`. Do not run the old array-based worker against this schema.

## Local insight backfill

`--backfill-insights [--project UUID]` groups only current existing pain points
without a membership. Existing automatic and manual memberships are never changed,
so a fully backfilled database exits without paid calls. It accepts
`REPLAY_INSIGHTS_ENV_FILE` (falling back to `REPLAY_EVAL_ENV_FILE`), requires a
loopback `DATABASE_URL`, and handles at most `REPLAY_INSIGHTS_BACKFILL_LIMIT`
points per invocation (default 100, maximum 1000):

```sh
REPLAY_INSIGHTS_ENV_FILE=../monorepo/apps/backend/.env \
  cargo run -p replay-summarizer -- --backfill-insights
```

Backfill reuses embeddings when their model version and input hash match.
To exercise real grouping without changing saved memberships, set
`REPLAY_TEST_DATABASE_URL` and `OPENROUTER_API_KEY`, then run (makes paid Jev calls):

```sh
cargo test -p replay-summarizer evaluate_local_grouping -- --ignored --nocapture
```

This exercises within-summary and across-summary matching and idempotent publication
using temporary grouping tables, then rolls back. Existing memberships are not
treated as human-labelled truth.

## Prompt

Edit [prompt.md](prompt.md) to change the analysis and writing instructions.
Rebuild and redeploy to apply changes. The prompt accounts for replay rendering
limitations, such as missing canvas charts, to reduce false bug reports.

## Build

From the repository root:

```sh
docker build -f apps/replay-summarizer/Dockerfile -t replay-summarizer .
```

The image includes Chromium and FFmpeg for rendering.

## Summary entities and deployment

Apply the monorepo's `replay_summary_entities` migration, then
`replay_summary_entity_cutover`, before starting this worker and the updated API.
The first backfills existing summaries; the second removes the old inline columns.
`replay_summaries` stores the recording revision, text, confidence, model, response
ID, prompt/schema version, provider-reported USD cost, token counts, latency, and
render settings. `replay_summary_pain_points` stores ordered timestamped findings,
evidence, and confidence. Publication remains atomic with job completion and fenced
by lease, recording revision, deletion, and storage generation.

Cost/token fields are nullable: absent provider accounting is unknown, not zero.
These fields describe the successful summary call, not cumulative billing across
failed attempts. Job reports retain rendering diagnostics without duplicating
summary text or model metadata. Confidence is a model estimate, not calibrated.

## Rendering and model configuration

- `REPLAY_SUMMARY_MODEL`: OpenRouter model ID; default `google/gemini-3.8-flash`.
- `REPLAY_RENDER_FPS`: default `3` (1–120).
- `REPLAY_RENDER_SPEED`: optional fixed-speed override (0.1–64).

At 3 FPS, the default preserves 1× speed for recordings up to one minute, uses 4×
through 30 minutes, and 8× for longer recordings. This keeps full detail for short
sessions while bounding video duration and model cost for long sessions. Idle time
is retained. The worker accepts queued legacy render profiles and claims them under
the adaptive profile; persisted render settings record the selected speed or fixed
override. Higher input density does not guarantee the provider examines every frame.

A bounded timeline of up to 500 recorded click, touch, scroll, and input-change
events accompanies the video. It excludes URLs, DOM text, and entered values and
explicitly marks truncation. It helps distinguish touch scrolling from clicks but
does not establish that an interaction succeeded or failed.

The request uses a portable structured-output schema; string lengths, array size,
and recording-duration bounds are checked locally and described in the prompt.
Large constrained schema bounds caused real provider errors during evaluation.
Malformed/truncated summaries and HTTP 4xx errors other than 429 are not retried
as whole render jobs. Connection/timeouts, 429s, and server errors remain retryable.

## Local evaluation

Build with `cargo build -p replay-summarizer -p rrweb2video`. These commands do not
claim jobs or write summaries to PostgreSQL:

```sh
REPLAY_EVAL_ENV_FILE=../monorepo/apps/backend/.env target/debug/replay-summarizer \
  --evaluate export PROJECT_UUID SESSION_ID WINDOW_ID /tmp/replay-evaluation/sample.json
```

Export uses local PostgreSQL by default, independently of `DATABASE_URL`.
Override with `REPLAY_EVAL_DATABASE_URL` if necessary. Storage settings come from
the supplied environment file. Render the JSON using the `rrweb2video` CLI with
`--fps 3 --speed 1 --timestamp-overlay` on Linux; Chromium's BeginFrameControl is
not available on macOS. Name the video `sample-3fps-1x.mp4`.

```sh
REPLAY_EVAL_ENV_FILE=../monorepo/apps/backend/.env \
  python3 apps/replay-summarizer/evaluation/compare.py \
  --directory /tmp/replay-evaluation --output /tmp/replay-evaluation/results \
  --samples sample --models z-ai/glm-5.3-flash google/gemini-3.8-flash
```

The comparison makes paid API calls. Outputs include summaries, raw responses,
usage even for rejected model output when returned, timing, and a prompt snapshot.
Keep recordings and these potentially private outputs outside version control.
See [the initial comparison](evaluation/2026-09-22.md) for findings and limitations.
