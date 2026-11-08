//! `FakeEmbedder` — a deterministic, offline embedder for tests and CI.
//!
//! Selected with backend `"fake"` (the `icode host` subcommand honours
//! `ICODE_EMBED_BACKEND=fake`). It hashes each lower-cased word into one of `dim`
//! buckets and L2-normalises the counts, so texts that share words are close and
//! unrelated texts are far — enough for write paths and dedup to behave like the
//! real thing, with no model and no network. Never use it against a real memory
//! base: its vectors are not comparable with a real model's.

use icode_core::error::Result;
use icode_core::traits::Embedder;

/// Bag-of-words hashing embedder (see the module docs).
pub struct FakeEmbedder {
    dim: usize,
}

impl FakeEmbedder {
    pub fn new(dim: usize) -> Self {
        Self { dim: dim.max(1) }
    }

    fn vector(&self, text: &str) -> Vec<f32> {
        let mut v = vec![0f32; self.dim];
        for word in text.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()) {
            // FNV-1a: stable across runs and platforms (unlike `DefaultHasher`).
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            for b in word.to_lowercase().bytes() {
                h ^= u64::from(b);
                h = h.wrapping_mul(0x0100_0000_01b3);
            }
            v[(h % self.dim as u64) as usize] += 1.0;
        }
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            v.iter_mut().for_each(|x| *x /= norm);
        } else {
            v[0] = 1.0;
        }
        v
    }
}

impl Embedder for FakeEmbedder {
    fn model_id(&self) -> &str {
        "fake-bow"
    }
    fn dim(&self) -> usize {
        self.dim
    }
    fn backend(&self) -> &str {
        "fake"
    }
    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| self.vector(t)).collect())
    }
    fn health(&self) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dist(a: &[f32], b: &[f32]) -> f32 {
        1.0 - a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>()
    }

    #[test]
    fn shared_words_are_close_and_output_is_deterministic() {
        let e = FakeEmbedder::new(1024);
        let v = e.embed(&["демон следит за файлами", "демон следит за файлами проекта", "pizza"]).unwrap();
        assert_eq!(v[0].len(), 1024);
        assert!(dist(&v[0], &v[1]) < 0.2);
        assert!(dist(&v[0], &v[2]) > 0.9);
        assert_eq!(e.embed(&["демон следит за файлами"]).unwrap()[0], v[0]);
    }
}
