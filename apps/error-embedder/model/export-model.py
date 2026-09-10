"""Export and validate the pinned checkpoint. Python is build/test tooling only."""
import argparse
import importlib
import json
import os
import runpy
from pathlib import Path
import subprocess
import sys

import numpy as np
import onnx
import onnxruntime as ort
from onnxruntime.transformers.onnx_model import OnnxModel
from safetensors.torch import load_file
import torch
from tokenizers import Tokenizer

from transformers.modeling_utils import no_init_weights
sources = runpy.run_path(str(Path(__file__).with_name("download-model.py")))
REVISION, CODE_REVISION, checksum, download = (
    sources[name] for name in ("REVISION", "CODE_REVISION", "checksum", "download")
)


def reference(directory):
    source = directory / "jina_source"
    sys.path.insert(0, str(directory.resolve()))
    config_class = importlib.import_module("jina_source.configuration_bert").JinaBertConfig
    model_class = importlib.import_module("jina_source.modeling_bert").JinaBertModel
    config = config_class.from_json_file(source / "config.json")
    config.attn_implementation = "eager"
    config.emb_pooler = None  # Wrapper pools; avoid the unused encode() tokenizer download.
    # Every parameter is loaded strictly below; random initialization is wasted work.
    with no_init_weights():
        model = model_class(config)
    # The checked checkpoint is the embedding model, without a task head.
    state = load_file(directory / "model.safetensors")
    model.load_state_dict(state, strict=True)
    return model.eval()


class Embedding(torch.nn.Module):
    def __init__(self, model):
        super().__init__()
        self.model = model

    def forward(self, input_ids, attention_mask):
        hidden = self.model(input_ids=input_ids, attention_mask=attention_mask, return_dict=False)[0]
        mask = attention_mask.unsqueeze(-1).float()
        pooled = (hidden * mask).sum(1) / mask.sum(1).clamp(min=1)
        return torch.nn.functional.normalize(pooled.float(), p=2, dim=1)


def fp16_storage(graph):
    """Store exactly representable weights in FP16; keep all arithmetic FP32.

    Gather in FP16 before casting selected rows avoids expanding the full word
    embedding table. MatMul weights are cast just before use. The session must
    disable graph optimizations so constant folding cannot expand them at load.
    """
    casts = []
    for tensor in graph.graph.initializer:
        if tensor.data_type != onnx.TensorProto.FLOAT or len(tensor.dims) != 2:
            continue
        name = tensor.name
        original = onnx.numpy_helper.to_array(tensor)
        packed = original.astype(np.float16)
        if not np.array_equal(original, packed.astype(np.float32)):
            raise ValueError(f"{name}: FP16 storage would lose weight precision")
        consumers = [node for node in graph.graph.node if name in node.input]
        if consumers and all(node.op_type == "Gather" and node.input[0] == name for node in consumers):
            tensor.CopyFrom(onnx.numpy_helper.from_array(packed, name))
            for node in consumers:
                output = node.output[0]
                node.output[0] = output + "_fp16_storage"
                casts.append(onnx.helper.make_node("Cast", [node.output[0]], [output], to=onnx.TensorProto.FLOAT))
        else:
            tensor.CopyFrom(onnx.numpy_helper.from_array(packed, name + "_fp16_storage"))
            casts.append(onnx.helper.make_node("Cast", [tensor.name], [name], to=onnx.TensorProto.FLOAT))
    graph.graph.node.extend(casts)
    ordered = OnnxModel(graph)
    ordered.topological_sort()
    return ordered.model


def feeds(tokenizer, text):
    ids = tokenizer.encode(text).ids
    truncated = len(ids) > 512
    if truncated:
        ids = ids[:511] + ids[-1:]
    return {"input_ids": np.array([ids], dtype=np.int64), "attention_mask": np.ones((1, len(ids)), dtype=np.int64)}, truncated


def fixture_texts():
    texts = ["", "error", "java.lang.NoSuchMethodError: api.run(int)\napp.Main.call",
             "TypeError: Cannot read properties of undefined\n    at render (app.js:12:3)",
             "ValueError: invalid literal\n  File app.py, line 20, in parse", "错误: café 🚀"]
    for kind in ["IllegalArgumentException", "NullPointerException", "NoSuchMethodError"]:
        for value in ["user", "account", "session", "request", "response"]:
            texts.append(f"java.lang.{kind}: missing {value}\napp.Service.run\napp.Main.call")
    texts.extend("frame " * n for n in [509, 510, 511, 512, 700])
    return texts


def validate(directory, wrapper, tokenizer, precisions, texts, rust_binary):
    samples = [feeds(tokenizer, text)[0] for text in texts]
    with torch.inference_mode():
        expected = np.concatenate([wrapper(**{k: torch.from_numpy(v) for k, v in feed.items()}).numpy() for feed in samples])
    normalized_expected = expected / np.linalg.norm(expected, axis=1, keepdims=True)
    expected_cosines = normalized_expected @ normalized_expected.T
    report = {"revision": REVISION, "samples": len(samples), "threshold": 0.986472, "runtime": ort.__version__, "precisions": {}}
    for precision in precisions:
        path = directory / f"model-{precision}.onnx"
        try:
            options = ort.SessionOptions()
            options.intra_op_num_threads = 2
            if precision == "fp16":
                options.graph_optimization_level = ort.GraphOptimizationLevel.ORT_DISABLE_ALL
            session = ort.InferenceSession(str(path), sess_options=options, providers=["CPUExecutionProvider"])
            actual = np.concatenate([session.run(["embedding"], feed)[0] for feed in samples])
            finite = bool(np.isfinite(actual).all())
            cosine = np.sum(actual * expected, axis=1) / (np.linalg.norm(actual, axis=1) * np.linalg.norm(expected, axis=1))
            normalized_actual = actual / np.linalg.norm(actual, axis=1, keepdims=True)
            flips = int(np.count_nonzero(np.triu((normalized_actual @ normalized_actual.T >= 0.986472) != (expected_cosines >= 0.986472), 1)))
            mixed_flips = int(np.count_nonzero((normalized_actual @ normalized_expected.T >= 0.986472) != (expected_cosines >= 0.986472)))
            error = float(np.max(np.abs(actual - expected)))
            result = {"max_component_error": error, "min_reference_cosine": float(cosine.min()), "threshold_flips": flips, "mixed_precision_threshold_flips": mixed_flips, "finite": finite,
                      "passed": bool(finite and np.allclose(np.linalg.norm(actual, axis=1), 1, atol=0.002) and cosine.min() >= 0.999 and flips == 0 and mixed_flips == 0 and error < 1e-5)}
            del session
            if rust_binary:
                inputs = [{"project_id": "00000000-0000-0000-0000-000000000000", "language": "python", "error_type": "Error", "error_message": "", "stacktrace": text} for text in texts]
                run = subprocess.run([str(rust_binary.resolve()), "embed"], input="".join(json.dumps(i) + "\n" for i in inputs), text=True, capture_output=True, check=True,
                                     env={**os.environ, "EMBED_MODEL_DIR": str(directory.resolve()), "EMBED_PRECISION": precision})
                rows = [json.loads(line) for line in run.stdout.splitlines()]
                assert len(rows) == len(inputs)
                with torch.inference_mode():
                    rust_reference = np.concatenate([wrapper(**{k: torch.from_numpy(v) for k, v in feeds(tokenizer, row["text"])[0].items()}).numpy() for row in rows])
                rust_vectors = np.array([row["embedding"] for row in rows])
                result["rust_max_component_error"] = float(np.max(np.abs(rust_vectors - rust_reference)))
                result["rust_truncation_matches"] = all(row["truncated"] == feeds(tokenizer, row["text"])[1] for row in rows)
                result["passed"] &= result["rust_truncation_matches"] and bool(np.isfinite(rust_vectors).all()) and result["rust_max_component_error"] < 1e-5
            report["precisions"][precision] = result
        except Exception as error:
            report["precisions"][precision] = {"passed": False, "error": str(error)}
    (directory / "validation.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))
    return all(result["passed"] for result in report["precisions"].values())


def main():
    parser = argparse.ArgumentParser(__doc__)
    parser.add_argument("--directory", type=Path, default=Path(__file__).parent / "model")
    parser.add_argument("--precisions", nargs="+", choices=["fp32", "fp16"], default=["fp16"])
    parser.add_argument("--validate-only", action="store_true")
    parser.add_argument("--texts", type=Path, help="Additional JSON array of canonical production texts for threshold validation")
    parser.add_argument("--rust-binary", type=Path)
    args = parser.parse_args()
    directory = args.directory.resolve()
    download(directory)
    torch.set_num_threads(2)
    wrapper = Embedding(reference(directory)).eval()
    tokenizer = Tokenizer.from_file(str(directory / "tokenizer.json"))
    tokenizer.no_truncation()
    tokenizer.no_padding()
    if not args.validate_only:
        fp32 = directory / "model-fp32.onnx"
        feed, _ = feeds(tokenizer, "java.lang.Error: example\napp.Main.call")
        with torch.inference_mode():
            torch.onnx.export(wrapper, tuple(torch.from_numpy(v) for v in feed.values()), str(fp32), input_names=list(feed), output_names=["embedding"],
                              dynamic_axes={"input_ids": {0: "batch", 1: "tokens"}, "attention_mask": {0: "batch", 1: "tokens"}, "embedding": {0: "batch"}}, opset_version=17, dynamo=False)
        if "fp16" in args.precisions:
            onnx.save(fp16_storage(onnx.load(fp32)), directory / "model-fp16.onnx")
        for precision in set(args.precisions) | {"fp32"}:
            path = directory / f"model-{precision}.onnx"
            onnx.checker.check_model(str(path))
            manifest = {"format": "fp16-storage-v1" if precision == "fp16" else "onnx-v1", "revision": REVISION, "code_revision": CODE_REVISION, "precision": precision, "model_sha256": checksum(path), "tokenizer_sha256": checksum(directory / "tokenizer.json")}
            (directory / f"{precision}.json").write_text(json.dumps(manifest, indent=2) + "\n")
    texts = fixture_texts() + (json.loads(args.texts.read_text()) if args.texts else [])
    if not validate(directory, wrapper, tokenizer, args.precisions, texts, args.rust_binary):
        raise SystemExit("Precision validation failed; inspect model/validation.json. Do not promote failed artifacts.")


if __name__ == "__main__":
    main()
