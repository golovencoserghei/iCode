//! Live integration tests against a running Ollama daemon (default
//! `http://127.0.0.1:11434`, model `qwen3-embedding:0.6b`). NOT `#[ignore]`:
//! the M2 environment guarantees Ollama is up with the model pulled.

use icode_core::config::EmbedConfig;
use icode_embed::build_embedder;

/// Batch embed at the native dim (1024): two distinct vectors, all finite.
#[test]
fn embed_batch_native_dim() {
    let cfg = EmbedConfig::default();
    let embedder = build_embedder(&cfg).expect("build_embedder(ollama)");

    let texts = ["hello world", "def send_email(user): pass"];
    let vecs = embedder.embed(&texts).expect("embed batch");

    assert_eq!(vecs.len(), 2, "one vector per input");
    for (i, v) in vecs.iter().enumerate() {
        assert_eq!(v.len(), 1024, "vector {i} must be native dim 1024");
        assert!(
            v.iter().all(|x| x.is_finite()),
            "vector {i} has non-finite values"
        );
    }

    // The two inputs are semantically different -> vectors must differ.
    assert_ne!(vecs[0], vecs[1], "distinct inputs gave identical vectors");

    // At native dim we do NOT renormalise; just sanity-check the norm is sane.
    let norm0: f32 = vecs[0].iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!(norm0 > 0.0 && norm0.is_finite(), "norm0 = {norm0}");

    // Surface a couple of sample components for the teamlead report.
    eprintln!(
        "[native] dim={} v0[0..3]={:?} ||v0||={norm0:.4}",
        vecs[0].len(),
        &vecs[0][..3]
    );
}

/// MRL path: request dim 256 -> backend native (1024) is truncated to 256 and
/// L2-renormalised to unit norm.
#[test]
fn embed_mrl_truncated_dim_is_unit_norm() {
    let cfg = EmbedConfig {
        dim: 256,
        ..Default::default()
    };
    let embedder = build_embedder(&cfg).expect("build_embedder(ollama, dim=256)");
    assert_eq!(embedder.dim(), 256);

    let vecs = embedder
        .embed(&["semantic search over code chunks"])
        .expect("embed single");

    assert_eq!(vecs.len(), 1);
    let v = &vecs[0];
    assert_eq!(v.len(), 256, "MRL-truncated to requested dim");
    assert!(v.iter().all(|x| x.is_finite()), "non-finite after MRL");

    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!(
        (norm - 1.0).abs() < 1e-3,
        "MRL vector must be unit norm, got {norm}"
    );

    eprintln!("[mrl] dim={} ||v||={norm:.6}", v.len());
}

/// Health probe: model present in `/api/tags` -> Ok.
#[test]
fn health_ok() {
    let cfg = EmbedConfig::default();
    let embedder = build_embedder(&cfg).expect("build_embedder(ollama)");
    embedder.health().expect("health should be Ok with ollama up");
}

/// Backend metadata reflects the contract.
#[test]
fn metadata() {
    let cfg = EmbedConfig::default();
    let embedder = build_embedder(&cfg).expect("build_embedder(ollama)");
    assert_eq!(embedder.backend(), "ollama");
    assert_eq!(embedder.model_id(), "qwen3-embedding:0.6b");
    assert_eq!(embedder.dim(), 1024);
}
