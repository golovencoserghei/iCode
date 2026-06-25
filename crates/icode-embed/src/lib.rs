//! `icode-embed` — concrete `Embedder` implementations.
//!
//! M2: `OllamaEmbedder` (default, validated against qwen3-embedding:0.6b → dim
//! 1024). `FastEmbedder` (ONNX, offline fallback) lands in a later task.
//! Both implement `icode_core::traits::Embedder`.

pub mod ollama;

pub use ollama::OllamaEmbedder;

use icode_core::config::EmbedConfig;
use icode_core::error::{Error, Result};
use icode_core::traits::Embedder;

/// Build the configured embedder behind a trait object.
///
/// Dispatches on `cfg.backend`. Only `"ollama"` is wired up today; the
/// fastembed fallback is a separate future task and returns a clear config
/// error until then.
pub fn build_embedder(cfg: &EmbedConfig) -> Result<Box<dyn Embedder>> {
    match cfg.backend.as_str() {
        "ollama" => Ok(Box::new(OllamaEmbedder::new(cfg)?)),
        other => Err(Error::Config(format!(
            "embed backend not supported yet: {other} (fastembed fallback lands later)"
        ))),
    }
}
