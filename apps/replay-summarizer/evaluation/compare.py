#!/usr/bin/env python3
"""Compare rendered local recordings with paid OpenRouter requests; never writes to the DB."""
import argparse
import concurrent.futures
import json
import os
from pathlib import Path
import subprocess
import time

ROOT = Path(__file__).resolve().parents[3]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--directory", type=Path, required=True, help="Contains SAMPLE.json and SAMPLE-FPSfps-SPEEDx.mp4")
parser.add_argument("--output", type=Path, required=True, help="Private output directory for summaries and raw responses")
parser.add_argument("--samples", nargs="+", required=True)
parser.add_argument("--models", nargs="+", default=["z-ai/glm-5.3-flash", "google/gemini-3.8-flash", "qwen/qwen3.8-flash"])
parser.add_argument("--speeds", nargs="+", type=int, default=[1])
parser.add_argument("--fps", type=int, default=3)
args = parser.parse_args()
args.output.mkdir(parents=True, exist_ok=True)
(args.output / "prompt.md").write_text((ROOT / "apps/replay-summarizer/prompt.md").read_text())
jobs = []
for sample in args.samples:
    events = json.loads((args.directory / f"{sample}.json").read_text())
    duration = events[-1]["timestamp"] - events[0]["timestamp"]
    for model in args.models:
        for speed in args.speeds:
            video = args.directory / f"{sample}-{args.fps}fps-{speed}x.mp4"
            if not video.exists():
                parser.error(f"Missing rendered video: {video}")
            jobs.append((sample, duration, model, speed, video))

def run(job):
    sample, duration, model, speed, video = job
    stem = f"{sample}-{model.replace('/', '_')}-{speed}x"
    raw_path = args.output / f"{stem}.response.json"
    env = os.environ | {"REPLAY_SUMMARY_MODEL": model, "REPLAY_EVAL_RESPONSE_FILE": str(raw_path.resolve())}
    started = time.monotonic()
    try:
        result = subprocess.run([
            str(ROOT / "target/debug/replay-summarizer"), "--evaluate", "summarize",
            str(video.resolve()), str(duration), str(args.fps), str(speed),
            str((args.output / f"{stem}.json").resolve()), str((args.directory / f"{sample}.json").resolve()),
        ], env=env, cwd=ROOT, capture_output=True, text=True, timeout=330)
        record = {"sample": sample, "model": model, "speed": speed,
                  "seconds": round(time.monotonic() - started, 2), "success": result.returncode == 0}
        if result.returncode:
            record["error"] = result.stderr[-2000:]
        else:
            record["metadata"] = json.loads(result.stdout)
        if raw_path.exists():
            raw = json.loads(raw_path.read_text())
            record["usage"] = raw.get("usage")
            record["provider"] = raw.get("provider")
    except subprocess.TimeoutExpired:
        record = {"sample": sample, "model": model, "speed": speed, "success": False, "error": "330 second evaluation timeout"}
    (args.output / f"{stem}.run.json").write_text(json.dumps(record, indent=2))
    print(json.dumps(record), flush=True)
    return record

with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
    records = list(pool.map(run, jobs))
(args.output / "comparison.json").write_text(json.dumps(records, indent=2))
