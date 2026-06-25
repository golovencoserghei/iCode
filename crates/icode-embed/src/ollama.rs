//! `OllamaEmbedder` — default `Embedder`, talks to a local Ollama daemon over
//! HTTP (`POST /api/embed`). Validated against `qwen3-embedding:0.6b` (native
//! dim 1024). Blocking client: the `Embedder` contract is sync (callers reach it
//! via `spawn_blocking`), so no async runtime is pulled in here.

use icode_core::config::EmbedConfig;
use icode_core::error::{Error, Result};
use icode_core::traits::Embedder;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Request body for `POST /api/embed`. Ollama accepts an array `input`, so one
/// call embeds a whole batch.
#[derive(Serialize)]
struct EmbedRequest<'a> {
    model: &'a str,
    input: &'a [&'a str],
}

/// Response body from `/api/embed`: one vector per input, in input order.
#[derive(Deserialize)]
struct EmbedResponse {
    #[serde(default)]
    embeddings: Vec<Vec<f32>>,
}

/// Minimal shape of `/api/tags` (only the model names matter for `health`).
#[derive(Deserialize)]
struct TagsResponse {
    #[serde(default)]
    models: Vec<TagModel>,
}

#[derive(Deserialize)]
struct TagModel {
    #[serde(default)]
    name: String,
}

/// Default `Embedder` backed by a local Ollama daemon.
pub struct OllamaEmbedder {
    client: reqwest::blocking::Client,
    /// Base URL, no trailing slash (e.g. `http://127.0.0.1:11434`).
    url: String,
    model: String,
    /// Target output dim. If the backend returns a larger native dim we apply
    /// MRL truncation + L2 renorm down to this width.
    dim: usize,
}

impl OllamaEmbedder {
    /// Build from config; constructs the blocking HTTP client with the
    /// per-request timeout from `cfg`.
    pub fn new(cfg: &EmbedConfig) -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_millis(cfg.timeout_ms))
            .build()
            .map_err(|e| Error::Embed(format!("failed to build HTTP client: {e}")))?;
        Ok(Self {
            client,
            url: cfg.ollama_url.trim_end_matches('/').to_string(),
            model: cfg.model.clone(),
            dim: cfg.dim,
        })
    }

    /// Apply MRL truncation + L2 renormalisation to one raw vector, matching the
    /// rule documented on `embed`. `index` is the position in the batch (for a
    /// precise `DimMismatch` on too-short vectors).
    fn fit_dim(&self, mut v: Vec<f32>) -> Result<Vec<f32>> {
        use std::cmp::Ordering;
        match v.len().cmp(&self.dim) {
            Ordering::Equal => Ok(v),
            Ordering::Greater => {
                // MRL: keep the first `dim` components, then renormalise to unit
                // norm so cosine/distance stays comparable to native vectors.
                v.truncate(self.dim);
                l2_normalize(&mut v);
                Ok(v)
            }
            Ordering::Less => Err(Error::DimMismatch {
                index: self.dim,
                got: v.len(),
            }),
        }
    }
}

impl Embedder for OllamaEmbedder {
    fn model_id(&self) -> &str {
        &self.model
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn backend(&self) -> &str {
        "ollama"
    }

    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }

        let endpoint = format!("{}/api/embed", self.url);
        let body = EmbedRequest {
            model: &self.model,
            input: texts,
        };

        let resp = self
            .client
            .post(&endpoint)
            .json(&body)
            .send()
            .map_err(|e| {
                Error::Embed(format!(
                    "POST {endpoint} failed: {e} (is ollama running at {}?)",
                    self.url
                ))
            })?;

        let status = resp.status();
        if !status.is_success() {
            let detail = resp.text().unwrap_or_default();
            return Err(Error::Embed(format!(
                "ollama /api/embed returned HTTP {status}: {detail} \
                 (model `{}` — try: ollama pull {})",
                self.model, self.model
            )));
        }

        let parsed: EmbedResponse = resp
            .json()
            .map_err(|e| Error::Embed(format!("failed to parse /api/embed response: {e}")))?;

        if parsed.embeddings.len() != texts.len() {
            return Err(Error::Embed(format!(
                "ollama returned {} embeddings for {} inputs (count mismatch)",
                parsed.embeddings.len(),
                texts.len()
            )));
        }

        // Preserve input order; MRL-fit each vector to the target dim.
        parsed
            .embeddings
            .into_iter()
            .map(|v| self.fit_dim(v))
            .collect()
    }

    fn health(&self) -> Result<()> {
        let endpoint = format!("{}/api/tags", self.url);
        let resp = self.client.get(&endpoint).send().map_err(|e| {
            Error::Embed(format!(
                "GET {endpoint} failed: {e} (is ollama running at {}?)",
                self.url
            ))
        })?;

        let status = resp.status();
        if !status.is_success() {
            return Err(Error::Embed(format!(
                "ollama /api/tags returned HTTP {status} (is ollama running at {}?)",
                self.url
            )));
        }

        let tags: TagsResponse = resp
            .json()
            .map_err(|e| Error::Embed(format!("failed to parse /api/tags response: {e}")))?;

        // Ollama tags carry the full `name:tag`; match exact, or by the
        // family prefix before the first `:` so `qwen3-embedding` also matches
        // `qwen3-embedding:0.6b` and vice versa.
        let wanted_family = self.model.split(':').next().unwrap_or(&self.model);
        let present = tags.models.iter().any(|m| {
            m.name == self.model
                || m.name.split(':').next().unwrap_or(&m.name) == wanted_family
        });

        if present {
            Ok(())
        } else {
            Err(Error::Embed(format!(
                "model `{}` not found in ollama (run: ollama pull {})",
                self.model, self.model
            )))
        }
    }
}

/// In-place L2 normalisation to unit norm. A zero vector is left untouched
/// (no division by zero).
fn l2_normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn embedder(dim: usize) -> OllamaEmbedder {
        let cfg = EmbedConfig {
            dim,
            ..Default::default()
        };
        // No request is made in these unit tests; client build must succeed.
        OllamaEmbedder::new(&cfg).expect("client builds")
    }

    #[test]
    fn fit_dim_equal_passthrough() {
        let e = embedder(4);
        let out = e.fit_dim(vec![1.0, 2.0, 3.0, 4.0]).unwrap();
        assert_eq!(out, vec![1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn fit_dim_truncate_and_renormalize() {
        let e = embedder(2);
        // native dim 4 -> keep first 2 [3,4], renorm to unit length.
        let out = e.fit_dim(vec![3.0, 4.0, 99.0, -7.0]).unwrap();
        assert_eq!(out.len(), 2);
        let norm: f32 = out.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-6, "norm was {norm}");
        // direction preserved: 3/5, 4/5.
        assert!((out[0] - 0.6).abs() < 1e-6);
        assert!((out[1] - 0.8).abs() < 1e-6);
    }

    #[test]
    fn fit_dim_too_short_is_mismatch() {
        let e = embedder(8);
        match e.fit_dim(vec![1.0, 2.0]) {
            Err(Error::DimMismatch { index, got }) => {
                assert_eq!(index, 8);
                assert_eq!(got, 2);
            }
            other => panic!("expected DimMismatch, got {other:?}"),
        }
    }

    #[test]
    fn empty_input_is_empty_output() {
        let e = embedder(1024);
        // empty input short-circuits before any network call.
        let out = e.embed(&[]).unwrap();
        assert!(out.is_empty());
    }
}
