"""Regression tests for lossless FP16 storage, independent of the checkpoint."""
import unittest

import numpy as np
import onnx
import onnxruntime as ort
from onnx import TensorProto, helper, numpy_helper

from pathlib import Path
from runpy import run_path

fp16_storage = run_path(str(Path(__file__).with_name("export-model.py")))["fp16_storage"]


class StorageTests(unittest.TestCase):
    def graph(self, value=0.5):
        graph = helper.make_graph([
            helper.make_node('Gather', ['words', 'ids'], ['selected']),
            helper.make_node('MatMul', ['selected', 'weight'], ['output']),
        ], 'storage', [helper.make_tensor_value_info('ids', TensorProto.INT64, ['n'])],
            [helper.make_tensor_value_info('output', TensorProto.FLOAT, ['n', 2])], [
                numpy_helper.from_array(np.array([[1, -2], [0.25, 0], [-1, 4]], dtype=np.float32), 'words'),
                numpy_helper.from_array(np.array([[value, 1], [2, -1]], dtype=np.float32), 'weight'),
            ])
        return helper.make_model(graph, opset_imports=[helper.make_opsetid('', 17)], ir_version=10)

    def test_gather_and_matmul_preserve_outputs_and_pack_weights(self):
        original = self.graph()
        packed = fp16_storage(self.graph())
        onnx.checker.check_model(packed)
        self.assertTrue(all(t.data_type == TensorProto.FLOAT16 for t in packed.graph.initializer))
        options = ort.SessionOptions()
        options.graph_optimization_level = ort.GraphOptimizationLevel.ORT_DISABLE_ALL
        sessions = [ort.InferenceSession(m.SerializeToString(), options, providers=['CPUExecutionProvider']) for m in [original, packed]]
        for ids in [[0], [0, 2, 1, 0], [2, 2]]:
            outputs = [s.run(None, {'ids': np.array(ids, dtype=np.int64)})[0] for s in sessions]
            np.testing.assert_array_equal(*outputs)

    def test_rejects_weights_that_would_lose_precision(self):
        with self.assertRaisesRegex(ValueError, 'lose weight precision'):
            fp16_storage(self.graph(value=0.1))


if __name__ == '__main__':
    unittest.main()
