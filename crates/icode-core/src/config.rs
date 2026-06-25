//! Configuration. Precedence (highest first): explicit overrides → env vars →
//! config file (`~/.icode/config.toml` or `<project>/.icode/config.toml`) →
//! these defaults. Defaults match the M0-validated environment.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct EmbedConfig {
    /// "ollama" | "fastembed".
    pub backend: String,
    /// Ollama base URL (locality invariant: must stay localhost).
    pub ollama_url: String,
    pub model: String,
    /// Output dim after optional MRL truncation. Used only to CREATE vec0 tables;
    /// the runtime source of truth is `embed_dim` in each db's meta table.
    pub dim: usize,
    /// Batch size for `/api/embed`.
    pub batch: usize,
    /// Per-request HTTP timeout (ms) to the embedding backend.
    pub timeout_ms: u64,
}

impl Default for EmbedConfig {
    fn default() -> Self {
        Self {
            backend: "ollama".into(),
            ollama_url: "http://127.0.0.1:11434".into(),
            model: "qwen3-embedding:0.6b".into(),
            dim: 1024,
            batch: 32,
            timeout_ms: 60_000,
        }
    }
}

/// Filesystem locations. Central db is the durable, backed-up store; per-project
/// dbs live under each project root.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct PathsConfig {
    /// Central db (memory + profile + KG). Default: ~/.icode/icode.db
    pub central_db: String,
    /// Per-project index dir name (joined to the project root). Default: .icode
    pub project_dir: String,
}

impl Default for PathsConfig {
    fn default() -> Self {
        Self {
            central_db: "~/.icode/icode.db".into(),
            project_dir: ".icode".into(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchConfig {
    /// "rrf" (default) | "weighted".
    pub fusion: String,
    pub rrf_k: u32,
    /// Weighted-mode coefficients (memory only).
    pub w_vec: f32,
    pub w_bm25: f32,
    pub oversample: usize,
    pub oversample_cap: usize,
    /// Enable cross-encoder rerank for code/recall.
    pub rerank: bool,
    /// Per-project chunk count above which the vector index switches to ANN.
    pub ann_threshold: usize,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            fusion: "rrf".into(),
            rrf_k: 60,
            w_vec: 0.6,
            w_bm25: 0.4,
            oversample: 4,
            oversample_cap: 200,
            rerank: true,
            ann_threshold: 50_000,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct RankingConfig {
    pub importance_weight: f32,
    pub access_boost: f32,
    pub access_boost_cap: f32,
    pub decay_per_hour: f32,
    pub l0_min_importance: f32,
}

impl Default for RankingConfig {
    fn default() -> Self {
        Self {
            importance_weight: 1.0,
            access_boost: 0.03,
            access_boost_cap: 1.0,
            decay_per_hour: 0.005,
            l0_min_importance: 4.0,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct IndexConfig {
    pub exclude_dirs: Vec<String>,
    pub dependency_dirs: Vec<String>,
    pub languages: Vec<String>,
    pub max_file_size: usize,
    pub bulk_threshold: usize,
    /// Near-duplicate gate distance (calibrated per embedding model).
    pub dedup_distance: f32,
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            exclude_dirs: ["\\.git", "node_modules", "target", "vendor", "dist", "build", ".venv"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            dependency_dirs: vec![],
            languages: vec![],
            max_file_size: 2 * 1024 * 1024,
            bulk_threshold: 1000,
            dedup_distance: 0.12,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct IcodeConfig {
    pub embed: EmbedConfig,
    pub search: SearchConfig,
    pub ranking: RankingConfig,
    pub index: IndexConfig,
    pub paths: PathsConfig,
}
