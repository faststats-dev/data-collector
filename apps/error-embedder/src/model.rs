//! The exported graph owns the architecture, mean pooling, and normalization.
use anyhow::{Context, Result, ensure};
use ort::{
    session::{Session, builder::GraphOptimizationLevel},
    value::Tensor,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{io::Read, path::Path, sync::Mutex};
use tokenizers::Tokenizer;

pub const VERSION: &str = "jina-code-516f4baf-adaptive-v1";
pub const REVISION: &str = "516f4baf13dec4ddddda8631e019b5737c8bc250";
const WIDTH: usize = 768;
const MAX_TOKENS: usize = 512;

#[derive(Deserialize)]
struct Manifest {
    #[serde(default)]
    format: String,
    revision: String,
    precision: String,
    model_sha256: String,
    tokenizer_sha256: String,
}

pub struct Model {
    tokenizer: Tokenizer,
    session: Mutex<Session>,
}

impl Model {
    pub fn load(path: &Path) -> Result<Self> {
        let precision = std::env::var("EMBED_PRECISION").unwrap_or_else(|_| "fp16".into());
        ensure!(
            matches!(precision.as_str(), "fp32" | "fp16"),
            "Use fp16 or fp32 for EMBED_PRECISION"
        );
        let manifest: Manifest = serde_json::from_slice(
            &std::fs::read(path.join(format!("{precision}.json")))
                .context("Export the model with apps/error-embedder/model/export-model.py first")?,
        )?;
        ensure!(
            manifest.revision == REVISION && manifest.precision == precision,
            "Unexpected model artifact"
        );
        ensure!(
            precision != "fp16" || manifest.format == "fp16-storage-v1",
            "Re-export FP16: expected lossless FP16 storage with FP32 computation"
        );
        let model_path = path.join(format!("model-{precision}.onnx"));
        for (file, expected) in [
            (model_path.clone(), manifest.model_sha256),
            (path.join("tokenizer.json"), manifest.tokenizer_sha256),
        ] {
            let mut reader = std::fs::File::open(&file)?;
            let mut digest = Sha256::new();
            let mut buffer = [0u8; 65536];
            loop {
                let n = reader.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                digest.update(&buffer[..n]);
            }
            ensure!(
                hex::encode(digest.finalize()) == expected,
                "Checksum mismatch: {}",
                file.display()
            );
        }
        let mut tokenizer =
            Tokenizer::from_file(path.join("tokenizer.json")).map_err(anyhow::Error::msg)?;
        // Detect truncation ourselves; hidden tokenizer truncation would bypass isolation.
        tokenizer
            .with_truncation(None)
            .map_err(anyhow::Error::msg)?;
        tokenizer.with_padding(None);
        let threads: usize = std::env::var("EMBED_THREADS")
            .unwrap_or_else(|_| "2".into())
            .parse()?;
        ensure!(threads > 0, "EMBED_THREADS must be positive");
        let mut builder = Session::builder()?.with_intra_threads(threads)?;
        if precision == "fp16" {
            // Keep weights compressed until use; constant folding would expand
            // FP16 casts at startup, defeating the memory saving on CPU.
            builder = builder.with_optimization_level(GraphOptimizationLevel::Disable)?;
        }
        let session = builder.commit_from_file(model_path)?;
        Ok(Self {
            tokenizer,
            session: Mutex::new(session),
        })
    }

    pub fn embed(&self, text: &str) -> Result<(Vec<f32>, bool)> {
        let tokens = self
            .tokenizer
            .encode(text, true)
            .map_err(anyhow::Error::msg)?;
        let (ids, truncated) = bounded_ids(tokens.get_ids())?;
        let n = ids.len();
        let mut session = self
            .session
            .lock()
            .map_err(|_| anyhow::anyhow!("Inference mutex poisoned"))?;
        let output = session.run(ort::inputs![
            "input_ids" => Tensor::from_array(([1, n], ids))?,
            "attention_mask" => Tensor::from_array(([1, n], vec![1i64; n]))?,
        ])?;
        let (shape, vector) = output["embedding"].try_extract_tensor::<f32>()?;
        ensure!(
            shape.as_ref() == [1, WIDTH as i64],
            "Unexpected embedding shape"
        );
        validate_vector(vector)?;
        Ok((vector.to_vec(), truncated))
    }
}

fn bounded_ids(tokens: &[u32]) -> Result<(Vec<i64>, bool)> {
    ensure!(!tokens.is_empty(), "Tokenizer returned no tokens");
    let truncated = tokens.len() > MAX_TOKENS;
    let mut ids: Vec<_> = tokens
        .iter()
        .take(MAX_TOKENS)
        .map(|&id| i64::from(id))
        .collect();
    if truncated {
        ids[MAX_TOKENS - 1] = i64::from(*tokens.last().context("No separator")?);
    }
    Ok((ids, truncated))
}

fn validate_vector(vector: &[f32]) -> Result<()> {
    ensure!(
        vector.len() == WIDTH && vector.iter().all(|v| v.is_finite()),
        "Invalid embedding"
    );
    ensure!(
        (vector.iter().map(|v| v * v).sum::<f32>() - 1.0).abs() < 0.002,
        "Embedding is not normalized"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_preserves_separator_and_boundary() -> Result<()> {
        for n in [2, 511, 512, 513, 1024] {
            let tokens: Vec<_> = (0..n).collect();
            let (ids, truncated) = bounded_ids(&tokens)?;
            assert_eq!(truncated, n > 512);
            assert_eq!(ids.len(), (n as usize).min(512));
            assert_eq!(ids.last().copied(), Some(i64::from(n - 1)));
        }
        assert!(bounded_ids(&[]).is_err());
        Ok(())
    }

    #[test]
    fn rejects_invalid_vectors() {
        for v in [
            vec![],
            vec![0.; WIDTH],
            vec![f32::NAN; WIDTH],
            vec![f32::INFINITY; WIDTH],
            vec![1.; WIDTH],
        ] {
            assert!(validate_vector(&v).is_err());
        }
        let mut valid = vec![0.; WIDTH];
        valid[0] = 1.;
        assert!(validate_vector(&valid).is_ok());
    }

    #[test]
    #[ignore = "requires exported checkpoint and ORT_DYLIB_PATH"]
    fn checkpoint_matches_reference() -> Result<()> {
        let model = Model::load(Path::new(&std::env::var("EMBED_MODEL_DIR")?))?;
        let (vector, truncated) =
            model.embed("java.lang.NoSuchMethodError: api.run(int)\napp.Main.call")?;
        assert!(!truncated);
        let expected = [
            0.025230072,
            -0.045794018,
            -0.020183181,
            0.068440124,
            0.0026602545,
            -0.03454094,
            0.006129094,
            0.025885846,
        ];
        for (actual, expected) in vector.iter().zip(expected) {
            assert!((actual - expected).abs() < 1e-5, "{actual} != {expected}");
        }
        Ok(())
    }
}
