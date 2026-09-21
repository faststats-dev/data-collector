# replay-summarizer

Generates summaries and timestamped UX pain points from recorded user sessions.

## How it works

1. Picks up automatic or manually requested jobs from PostgreSQL.
2. Downloads rrweb recordings from S3 and renders them into a temporary video.
3. Sends the video to OpenRouter for analysis.
4. Validates the response and saves the summary and pain points to PostgreSQL.

Each instance processes one replay at a time. Failed jobs can retry, and outdated
recordings cannot overwrite newer results. Temporary videos are deleted after processing.

## Configuration

Required environment variables:

- `DATABASE_URL`: PostgreSQL connection string.
- `OPENROUTER_API_KEY`: API key for video analysis.
- `REPLAY_S3_BUCKET_PREFIX`: prefix for project replay buckets.
- `REPLAY_S3_ACCESS_KEY_ID` and `REPLAY_S3_SECRET_ACCESS_KEY`: storage credentials.

Set `REPLAY_S3_ENDPOINT` for S3-compatible storage and `REPLAY_S3_REGION` if needed
(default: `us-east-1`).

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
