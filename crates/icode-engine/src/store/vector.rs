//! `Vec0Index` — the sqlite-vec (vec0) implementation of the frozen
//! `VectorIndex` contract.
//!
//! It shares the SAME `Connection` as the owning `SqliteCodeStore` (via a cloned
//! `Arc<Mutex<Connection>>`), so there is exactly ONE writer to the per-project
//! db — the vector rows and their `code_chunks` rows live in one SQLite file and
//! one serialization point. `vec_code.rowid` maps 1:1 onto `code_chunks.id`.
//!
//! Vectors are stored as f32 little-endian blobs (the format proven by the M0
//! `vec_spike`): a dim-`N` vector is `4*N` bytes. Cosine distance (lower = closer)
//! is used for KNN, matching the `vec_code` column definition.

use std::sync::{Arc, Mutex};

use icode_core::error::{Error, Result};
use icode_core::traits::{Neighbor, VectorIndex};
use rusqlite::{params, Connection};

/// Map any rusqlite error into the framework-free `Error::Vector` variant
/// (distinct from `Error::Store` so a vector-path failure is attributable).
fn vec_err<E: std::fmt::Display>(e: E) -> Error {
    Error::Vector(e.to_string())
}

/// Encode an f32 slice as a little-endian byte blob (vec0's expected format).
/// `pub(crate)` so the embed-cache (in `store::mod`) stores vectors in the EXACT
/// same wire format vec0 uses — one encoding, no drift between the cache and the
/// live index.
pub(crate) fn f32_blob(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for f in v {
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}

/// Decode a little-endian f32 byte blob back into a vector (inverse of
/// [`f32_blob`]). Trailing bytes that don't form a full f32 are ignored — a
/// well-formed blob is always a multiple of 4 bytes.
pub(crate) fn blob_to_f32(blob: &[u8]) -> Vec<f32> {
    blob.chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect()
}

/// sqlite-vec (vec0) vector index over the per-project `vec_code` table.
///
/// Cheap to clone (just an `Arc` bump + a `usize`); each clone talks to the same
/// underlying connection.
#[derive(Clone)]
pub struct Vec0Index {
    conn: Arc<Mutex<Connection>>,
    dim: usize,
}

impl Vec0Index {
    /// Build an index bound to a shared connection. `dim` MUST equal the width of
    /// the `vec_code` vec0 column (see `schema::VEC_DIM`) — a mismatch surfaces at
    /// the first upsert as `Error::DimMismatch`.
    pub(crate) fn new(conn: Arc<Mutex<Connection>>, dim: usize) -> Self {
        Self { conn, dim }
    }

    /// Validate a vector's length against the index dim, returning `DimMismatch`
    /// with both sides so the caller can report the rebuild it needs.
    fn check_dim(&self, len: usize) -> Result<()> {
        if len != self.dim {
            return Err(Error::DimMismatch {
                index: self.dim,
                got: len,
            });
        }
        Ok(())
    }
}

impl VectorIndex for Vec0Index {
    fn dim(&self) -> usize {
        self.dim
    }

    fn upsert(&self, rowid: i64, embedding: &[f32]) -> Result<()> {
        self.check_dim(embedding.len())?;
        let conn = self.conn.lock().map_err(vec_err)?;
        // INSERT OR REPLACE keyed on rowid: re-embedding a chunk overwrites its
        // vector in place (vec0 supports rowid as the conflict target).
        conn.execute(
            "INSERT OR REPLACE INTO vec_code(rowid, embedding) VALUES (?1, ?2)",
            params![rowid, f32_blob(embedding)],
        )
        .map_err(vec_err)?;
        Ok(())
    }

    fn upsert_batch(&self, items: &[(i64, Vec<f32>)]) -> Result<()> {
        // Validate every vector BEFORE touching the db so a bad item aborts the
        // whole batch without a partial write.
        for (_, emb) in items {
            self.check_dim(emb.len())?;
        }
        let mut conn = self.conn.lock().map_err(vec_err)?;
        let tx = conn.transaction().map_err(vec_err)?;
        {
            let mut stmt = tx
                .prepare("INSERT OR REPLACE INTO vec_code(rowid, embedding) VALUES (?1, ?2)")
                .map_err(vec_err)?;
            for (rowid, emb) in items {
                stmt.execute(params![rowid, f32_blob(emb)]).map_err(vec_err)?;
            }
        }
        tx.commit().map_err(vec_err)?;
        Ok(())
    }

    fn delete(&self, rowid: i64) -> Result<()> {
        let conn = self.conn.lock().map_err(vec_err)?;
        conn.execute("DELETE FROM vec_code WHERE rowid = ?1", params![rowid])
            .map_err(vec_err)?;
        Ok(())
    }

    fn count(&self) -> Result<u64> {
        let conn = self.conn.lock().map_err(vec_err)?;
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM vec_code", [], |r| r.get(0))
            .map_err(vec_err)?;
        Ok(n as u64)
    }

    fn clear(&self) -> Result<()> {
        let conn = self.conn.lock().map_err(vec_err)?;
        conn.execute("DELETE FROM vec_code", []).map_err(vec_err)?;
        Ok(())
    }

    fn knn(&self, query: &[f32], k: usize) -> Result<Vec<Neighbor>> {
        self.check_dim(query.len())?;
        if k == 0 {
            return Ok(vec![]);
        }
        let conn = self.conn.lock().map_err(vec_err)?;
        // vec0 KNN: `embedding MATCH ?1 AND k=?2`, ascending by distance.
        let mut stmt = conn
            .prepare(
                "SELECT rowid, distance FROM vec_code \
                 WHERE embedding MATCH ?1 AND k = ?2 ORDER BY distance",
            )
            .map_err(vec_err)?;
        let rows = stmt
            .query_map(params![f32_blob(query), k as i64], |row| {
                Ok(Neighbor {
                    rowid: row.get::<_, i64>(0)?,
                    // vec0 returns distance as f64; narrow to the contract's f32.
                    distance: row.get::<_, f64>(1)? as f32,
                })
            })
            .map_err(vec_err)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(vec_err)?);
        }
        Ok(out)
    }
}
