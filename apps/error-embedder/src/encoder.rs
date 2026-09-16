use crate::{Embedding, Occurrence, cache, model};
use anyhow::{Context, Result};
use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};

const LOCAL_CAPACITY: usize = 1024;

pub struct Encoder {
    model: Arc<model::Model>,
    redis: Option<cache::Cache>,
    local: HashMap<String, Vec<f32>>,
    order: VecDeque<String>,
}

impl Encoder {
    pub fn new(model: Arc<model::Model>) -> Result<Self> {
        Ok(Self {
            model,
            redis: cache::Cache::from_env()?,
            local: HashMap::new(),
            order: VecDeque::new(),
        })
    }

    fn remember(&mut self, key: String, vector: Vec<f32>) {
        if self.local.contains_key(&key) {
            return;
        }
        if self.local.len() == LOCAL_CAPACITY
            && let Some(oldest) = self.order.pop_front()
        {
            self.local.remove(&oldest);
        }
        self.order.push_back(key.clone());
        self.local.insert(key, vector);
    }

    pub async fn encode(&mut self, input: Occurrence) -> Result<Embedding> {
        self.encode_batch(vec![input])
            .await?
            .pop()
            .context("Missing embedding")
    }

    pub async fn encode_batch(&mut self, inputs: Vec<Occurrence>) -> Result<Vec<Embedding>> {
        let mut keys = Vec::with_capacity(inputs.len());
        let mut missing = Vec::new();
        let mut prepared = HashMap::new();
        for input in &inputs {
            input.validate()?;
            let text = input.input.text();
            let key = cache::key(model::VERSION, &text);
            keys.push(key.clone());
            // Deduplicate prepared text before any cache lookup or inference.
            if prepared.contains_key(&key) {
                continue;
            }
            let cached = if let Some(vector) = self.local.get(&key) {
                Some(vector.clone())
            } else {
                match &mut self.redis {
                    Some(cache) => cache.get(&key).await,
                    None => None,
                }
            };
            if let Some(vector) = cached {
                self.remember(key.clone(), vector.clone());
                prepared.insert(key, Some(vector));
            } else {
                prepared.insert(key.clone(), None);
                missing.push((key, text));
            }
        }
        if !missing.is_empty() {
            let model = self.model.clone();
            let texts = missing
                .iter()
                .map(|(_, text)| text.clone())
                .collect::<Vec<_>>();
            let vectors = tokio::task::spawn_blocking(move || model.embed_batch(&texts)).await??;
            for ((key, _), (vector, _)) in missing.into_iter().zip(vectors) {
                if let Some(cache) = &mut self.redis {
                    cache.set(&key, &vector).await;
                }
                self.remember(key.clone(), vector.clone());
                prepared.insert(key, Some(vector));
            }
        }
        inputs
            .into_iter()
            .zip(keys)
            .map(|(input, key)| {
                let embedding = prepared
                    .get(&key)
                    .and_then(Option::as_ref)
                    .context("Missing prepared vector")?
                    .clone();
                Ok(Embedding {
                    project_id: input.project_id,
                    exact_hash: input.exact_hash,
                    timestamp: input.timestamp,
                    model_version: model::VERSION,
                    embedding,
                })
            })
            .collect()
    }
}
