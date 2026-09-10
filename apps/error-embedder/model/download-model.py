"""Download checksum-verified checkpoint and export sources; standard library only."""
import argparse
import hashlib
import shutil
import urllib.request
from pathlib import Path

REVISION = "516f4baf13dec4ddddda8631e019b5737c8bc250"
CODE_REVISION = "3baf9e3ac750e76e8edd3019170176884695fb94"
CHECKPOINT = f"jinaai/jina-embeddings-v2-base-code/resolve/{REVISION}"
SOURCE = f"jinaai/jina-bert-v2-qk-post-norm/resolve/{CODE_REVISION}"
FILES = {
    "model.safetensors": (CHECKPOINT, "8b53bfd4ae2cd586004a6ca4a16551b630a2a1b1d655ff1ee9be1286a1781c5b"),
    "tokenizer.json": (CHECKPOINT, "b01c78a902aa4facb2f47f95449f48e2f7bbfea5d2472ee2f6ce92323c6f86e5"),
    "jina_source/config.json": (CHECKPOINT, "e426aa684c7f9a95c5f020aa855faf93a24f065f5fad0c9e17b124670cabdea6"),
    "jina_source/configuration_bert.py": (SOURCE, "d018de11bcb62cedf436c8876459e5cd119d1faf72bc107338576a6d1b8eeea5"),
    "jina_source/modeling_bert.py": (SOURCE, "8f96ef0576401f6b996f9b72ad34e13190139eec1195767516485876434bb612"),
}


def checksum(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def download(directory, from_cache=None):
    for filename, (source, digest) in FILES.items():
        destination = directory / filename
        if destination.exists() and checksum(destination) == digest:
            continue
        destination.parent.mkdir(parents=True, exist_ok=True)
        temporary = destination.with_suffix(destination.suffix + ".tmp")
        if from_cache:
            shutil.copyfile(from_cache / filename, temporary)
        else:
            with urllib.request.urlopen(f"https://huggingface.co/{source}/{destination.name}", timeout=120) as response, temporary.open("wb") as output:
                shutil.copyfileobj(response, output)
        if checksum(temporary) != digest:
            raise ValueError(f"Checksum mismatch: {filename}")
        temporary.replace(destination)
    (directory / "jina_source/__init__.py").touch()
    print(f"Model sources ready in {directory.resolve()}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(__doc__)
    parser.add_argument("--directory", type=Path, default=Path(__file__).parent / "model")
    parser.add_argument("--from-cache", type=Path, help="Copy previously verified model sources")
    args = parser.parse_args()
    download(args.directory, args.from_cache)
