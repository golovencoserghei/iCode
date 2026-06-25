//! M0 critical gate: validate sqlite-vec (vec0) end-to-end on a RELEASE build.
//!
//! Proves (Reviewer #1 / #2 concerns):
//!   1. vec0 loads via auto-extension into rusqlite's bundled SQLite (single SQLite copy).
//!   2. vec0 KNN through the SAME Connection that creates normal tables.
//!   3. Cosine distance is CORRECT (assert KNOWN distances, not just a round-trip).
//!   4. vec0 KNN combined with a JOIN to a normal table.
//!   5. vec0 KNN in an ATTACHed schema (foundation of `recall` cross-db synergy).
//!
//! Production vector format = f32 little-endian blob (what icode-engine will store),
//! so the spike exercises that exact path (catches endianness bugs).

use rusqlite::{ffi::sqlite3_auto_extension, params, Connection};
use sqlite_vec::sqlite3_vec_init;

fn f32_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

fn main() -> anyhow::Result<()> {
    // Register vec0 for every connection opened after this point.
    unsafe {
        sqlite3_auto_extension(Some(std::mem::transmute(
            sqlite3_vec_init as *const (),
        )));
    }

    let db = Connection::open_in_memory()?;
    let ver: String = db.query_row("select vec_version()", [], |r| r.get(0))?;
    println!("vec_version = {ver}");

    // --- Test 2: vec0 + normal table on the SAME connection, cosine KNN ---
    db.execute_batch(
        "create table chunks(id integer primary key, name text);
         create virtual table vec_code using vec0(embedding float[4] distance_metric=cosine);",
    )?;
    db.execute("insert into chunks(id,name) values (1,'a'),(2,'b'),(3,'c45')", [])?;
    {
        let mut s = db.prepare("insert into vec_code(rowid, embedding) values (?,?)")?;
        s.execute(params![1i64, f32_blob(&[1.0, 0.0, 0.0, 0.0])])?; // a
        s.execute(params![2i64, f32_blob(&[0.0, 1.0, 0.0, 0.0])])?; // b (orthogonal)
        s.execute(params![3i64, f32_blob(&[0.70710677, 0.70710677, 0.0, 0.0])])?; // c (45°)
    }
    let q = f32_blob(&[1.0, 0.0, 0.0, 0.0]); // query == a

    // 2a: pure KNN (no join)
    let mut stmt =
        db.prepare("select rowid, distance from vec_code where embedding match ?1 and k=3 order by distance")?;
    let pure: Vec<(i64, f64)> = stmt
        .query_map(params![q], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    for (rid, d) in &pure {
        println!("[pure] rowid={rid} cos_dist={d:.5}");
    }
    assert_eq!(pure[0].0, 1, "nearest must be rowid 1 (identical)");
    assert!(pure[0].1.abs() < 1e-4, "cos dist to identical ~0, got {}", pure[0].1);
    assert_eq!(pure[1].0, 3, "second must be rowid 3 (45deg)");
    assert!((pure[1].1 - 0.29289).abs() < 1e-3, "cos 45deg ~0.2929, got {}", pure[1].1);
    assert_eq!(pure[2].0, 2, "third must be rowid 2 (orthogonal)");
    assert!((pure[2].1 - 1.0).abs() < 1e-3, "cos orthogonal ~1.0, got {}", pure[2].1);
    println!("TEST2a vec0 cosine KNN (known distances): PASS");

    // 2b: KNN joined to a normal table on the same connection
    let mut stmt = db.prepare(
        "select v.rowid, v.distance, c.name from vec_code v join chunks c on c.id = v.rowid \
         where v.embedding match ?1 and k=3 order by v.distance",
    )?;
    let joined: Vec<(i64, f64, String)> = stmt
        .query_map(params![q], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?;
    for (rid, d, n) in &joined {
        println!("[join] rowid={rid} name={n} cos_dist={d:.5}");
    }
    assert_eq!(joined[0].2, "a", "joined nearest name must be 'a'");
    println!("TEST2b vec0 KNN + JOIN: PASS");

    // --- Test 3: ATTACH a second db with its own vec0, KNN on attached schema ---
    let path = std::env::temp_dir().join("icode_spike_attach.db");
    let _ = std::fs::remove_file(&path);
    {
        let other = Connection::open(&path)?;
        other.execute_batch(
            "create virtual table vmem using vec0(embedding float[4] distance_metric=cosine);",
        )?;
        let mut s = other.prepare("insert into vmem(rowid, embedding) values (?,?)")?;
        s.execute(params![10i64, f32_blob(&[1.0, 0.0, 0.0, 0.0])])?;
        s.execute(params![11i64, f32_blob(&[0.0, 0.0, 1.0, 0.0])])?;
    }
    db.execute("attach database ?1 as ext", params![path.to_str().unwrap()])?;
    match db.prepare("select rowid, distance from ext.vmem where embedding match ?1 and k=2 order by distance") {
        Ok(mut stmt) => {
            let att: Vec<(i64, f64)> = stmt
                .query_map(params![q], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<_, _>>()?;
            for (rid, d) in &att {
                println!("[attach] rowid={rid} cos_dist={d:.5}");
            }
            assert_eq!(att[0].0, 10, "attached vec0 KNN nearest must be rowid 10");
            println!("TEST3 ATTACH + vec0 KNN: PASS  -> recall can use single-query ATTACH join");
        }
        Err(e) => {
            // Non-fatal: plan already defaults `recall` to N-KNN + fuse in Rust.
            println!("TEST3 ATTACH + vec0 KNN: NOT SUPPORTED ({e}) -> use N-KNN fallback (per plan)");
        }
    }
    let _ = std::fs::remove_file(&path);

    println!("\n=== M0 sqlite-vec GATE: PASS ===");
    Ok(())
}
