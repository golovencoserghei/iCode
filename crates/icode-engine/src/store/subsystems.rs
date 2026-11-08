//! Subsystems + hubs for `map`: Louvain over the FILE graph, cached in `meta`.
//!
//! A flat list of directories (`modules` in the map) says where code lives, not
//! what works together. Files that call each other heavily form a subsystem whether
//! or not they share a folder, and the boundaries between those groups are what an
//! agent needs before it edits across them.
//!
//! The file graph: one node per indexed NON-TEST file; an edge for every call
//! whose target resolves to ONE definition in another file, weighted by the edge's confidence
//! (so a guessed homonym counts for less than an exact receiver match); an edge per
//! import resolved to a unique file; and a light "same directory" edge between
//! neighbouring files of a folder, which lets a file with no resolved calls join
//! its siblings instead of standing alone. Deterministic end to end (see
//! [`crate::cluster`]).
//!
//! Tests are kept out of the partition: an integration test calls into every
//! layer, so letting test files (or `#[cfg(test)]` functions) vote glues unrelated
//! subsystems together through them — measured on iCode's own engine, the test
//! directory became the name of two of three clusters. Each test file is attached
//! afterwards to the subsystem it calls most, and only counted there.
//!
//! A method-syntax call never reaches a FREE function (`is_method`, which the
//! resolve pass also sets for `Math.round()` / `out.push()` in JS/TS/Python/Go —
//! on FlowCode those were 92 and 300+ false edges and topped the hub list).
//!
//! Cost (release, loaded machine): ~7 ms on iCode (99 files), ~13 ms on FlowCode
//! (373 files), ~400 ms on a synthetic 8.2k-file / 776k-call tree — paid once per
//! graph generation, then a cache hit is well under 1 ms.
//!
//! Cached in `meta` under the graph generation that `compute_symbol_ranks` bumps
//! after every re-index batch, so a `map` on an unchanged graph pays one point
//! query, and the first `map` after a re-index recomputes.

use std::collections::{BTreeMap, HashMap};

use icode_core::error::Result;
use icode_core::model::{Hub, Subsystem, SubsystemLink, SubsystemMap};
use rusqlite::{Connection, OptionalExtension};

use super::{store_err, SqliteCodeStore};

/// Weight of a resolved import edge (a call edge weighs its confidence, 0.3..0.9).
const IMPORT_WEIGHT: f64 = 0.5;
/// Weight of the "neighbouring file in the same directory" prior.
const SIBLING_WEIGHT: f64 = 0.25;
/// Bump to invalidate every cached partition when the algorithm changes.
const ALGO_VERSION: u32 = 6;
/// How many hubs are cached (the reply trims to the caller's limit).
const HUBS_CACHED: usize = 50;
/// A cross-file call edge: (caller file, callee file, weight, from test code).
type FileEdge = (usize, usize, f64, bool);

pub(crate) const GRAPH_GEN_KEY: &str = "graph_gen";

impl SqliteCodeStore {
    /// Subsystems and hubs for `scope` (a monorepo member; `None` = whole index).
    /// Served from the `meta` cache when the graph generation has not moved.
    pub fn subsystems(&self, scope: Option<&str>) -> Result<SubsystemMap> {
        let scope = scope.unwrap_or("");
        let key = format!("subsystems:v{ALGO_VERSION}:{scope}");
        let conn = self.conn.lock().map_err(store_err)?;
        let generation = graph_generation(&conn)?;
        if let Some(raw) = conn
            .query_row("SELECT value FROM meta WHERE key = ?1", rusqlite::params![key], |r| {
                r.get::<_, String>(0)
            })
            .optional()
            .map_err(store_err)?
        {
            if let Ok(cached) = serde_json::from_str::<CachedMap>(&raw) {
                if cached.generation == generation {
                    return Ok(cached.map);
                }
            }
        }
        let map = compute(&conn, scope)?;
        // Best-effort: a read-only handle or a busy writer just skips the cache.
        if let Ok(raw) = serde_json::to_string(&CachedMap { generation: generation.clone(), map: map.clone() }) {
            let _ = conn.execute(
                "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
                rusqlite::params![key, raw],
            );
        }
        Ok(map)
    }

    /// Advance the graph generation (call after the graph or the ranks changed).
    pub(crate) fn bump_graph_generation(&self) -> Result<()> {
        let conn = self.conn.lock().map_err(store_err)?;
        conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, '1') \
             ON CONFLICT(key) DO UPDATE SET value = CAST(COALESCE(CAST(value AS INTEGER), 0) + 1 AS TEXT)",
            rusqlite::params![GRAPH_GEN_KEY],
        )
        .map_err(store_err)?;
        Ok(())
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CachedMap {
    generation: String,
    map: SubsystemMap,
}

fn graph_generation(conn: &Connection) -> Result<String> {
    Ok(conn
        .query_row("SELECT value FROM meta WHERE key = ?1", rusqlite::params![GRAPH_GEN_KEY], |r| {
            r.get::<_, Option<String>>(0)
        })
        .optional()
        .map_err(store_err)?
        .flatten()
        .unwrap_or_else(|| "0".into()))
}

struct Func {
    qualified: String,
    file: usize,
    rank: f64,
    is_test: bool,
    free: bool,
}

fn compute(conn: &Connection, scope: &str) -> Result<SubsystemMap> {
    // ── nodes: files, in path order (the order the clustering visits them) ──
    let mut stmt = conn
        .prepare("SELECT path, language, id FROM files WHERE (?1 = '' OR module = ?1) ORDER BY path")
        .map_err(store_err)?;
    let file_rows: Vec<(String, String, i64)> = stmt
        .query_map(rusqlite::params![scope], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .map_err(store_err)?
        .collect::<rusqlite::Result<_>>()
        .map_err(store_err)?;
    drop(stmt);
    let id_ix: HashMap<i64, usize> = file_rows.iter().enumerate().map(|(i, (_, _, id))| (*id, i)).collect();
    let paths: Vec<String> = file_rows.into_iter().map(|(p, _, _)| p).collect();
    if paths.is_empty() {
        return Ok(SubsystemMap::default());
    }
    let root = common_dir(paths.iter().map(|s| s.as_str()));
    let rel: Vec<String> = paths.iter().map(|p| relative(p, &root)).collect();

    // ── functions ── (ORDER BY id: the primary key, no sort pass)
    let mut stmt = conn
        .prepare(
            "SELECT name, qualified_name, file_id, rank, is_test FROM functions \
             WHERE (?1 = '' OR module = ?1) ORDER BY id",
        )
        .map_err(store_err)?;
    let mut rows = stmt.query(rusqlite::params![scope]).map_err(store_err)?;
    let mut funcs: Vec<Func> = Vec::new();
    let mut by_qn: HashMap<String, Vec<usize>> = HashMap::new();
    let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
    while let Some(r) = rows.next().map_err(store_err)? {
        let file_id: i64 = r.get(2).map_err(store_err)?;
        let Some(&file) = id_ix.get(&file_id) else { continue };
        let name = text(r, 0)?.unwrap_or("");
        let qn = text(r, 1)?.unwrap_or("");
        let id = funcs.len();
        let free = !qn.contains(':') && !qn.contains('.');
        match by_qn.get_mut(qn) {
            Some(v) => v.push(id),
            None => {
                by_qn.insert(qn.to_string(), vec![id]);
            }
        }
        match by_name.get_mut(name) {
            Some(v) => v.push(id),
            None => {
                by_name.insert(name.to_string(), vec![id]);
            }
        }
        funcs.push(Func {
            qualified: qn.to_string(),
            file,
            rank: r.get(3).map_err(store_err)?,
            is_test: r.get::<_, i64>(4).map_err(store_err)? != 0,
            free,
        });
    }
    drop(rows);
    drop(stmt);

    let n = paths.len();
    let test_file: Vec<bool> = rel.iter().map(|p| crate::verdict::is_test_path(p)).collect();
    // `#[cfg(test)]` functions inside src files: their calls are test wiring too.
    let test_fn: std::collections::HashSet<(usize, &str)> = funcs
        .iter()
        .filter(|f| f.is_test)
        .map(|f| (f.file, f.qualified.as_str()))
        .collect();
    let mut in_degree: Vec<usize> = vec![0; funcs.len()];

    // ── resolved import edges ──
    let mut by_suffix: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, p) in rel.iter().enumerate() {
        let stem = p.rsplit_once('.').map(|(s, _)| s).unwrap_or(p);
        let segs: Vec<&str> = stem.split('/').filter(|s| !s.is_empty()).collect();
        for k in 0..segs.len() {
            by_suffix.entry(segs[k..].join("/")).or_default().push(i);
        }
    }
    let mut import_edges: Vec<FileEdge> = Vec::new();
    // Many files import the same module: resolve each distinct spelling once.
    let mut resolved_import: HashMap<String, Option<usize>> = HashMap::new();
    let mut stmt = conn
        .prepare(
            "SELECT i.file_id, i.module FROM imports i JOIN files f ON f.id = i.file_id \
             WHERE (?1 = '' OR f.module = ?1)",
        )
        .map_err(store_err)?;
    let mut rows = stmt.query(rusqlite::params![scope]).map_err(store_err)?;
    while let Some(r) = rows.next().map_err(store_err)? {
        let file_id: i64 = r.get(0).map_err(store_err)?;
        let Some(&src) = id_ix.get(&file_id) else { continue };
        let module = text(r, 1)?.unwrap_or("");
        let dst = match resolved_import.get(module) {
            Some(d) => *d,
            None => {
                let d = resolve_import(module, &by_suffix);
                resolved_import.insert(module.to_string(), d);
                d
            }
        };
        if let Some(dst) = dst.filter(|&d| d != src) {
            import_edges.push((src, dst, IMPORT_WEIGHT, test_file[src]));
        }
    }
    drop(rows);
    drop(stmt);

    // ── call edges ── (the hot loop: ~100 rows per file, so borrow, never allocate)
    let mut weights: HashMap<(usize, usize, bool), f64> = HashMap::new();
    let mut stmt = conn
        .prepare(
            "SELECT file_id, caller, callee, resolved_callee, is_method, confidence FROM calls \
             WHERE (?1 = '' OR module = ?1)",
        )
        .map_err(store_err)?;
    let mut rows = stmt.query(rusqlite::params![scope]).map_err(store_err)?;
    while let Some(r) = rows.next().map_err(store_err)? {
        let file_id: i64 = r.get(0).map_err(store_err)?;
        let Some(&src) = id_ix.get(&file_id) else { continue };
        let callee = text(r, 2)?.unwrap_or("");
        let resolved = text(r, 3)?;
        let is_method = r.get::<_, i64>(4).map_err(store_err)? != 0;
        let target = match resolved.and_then(|q| by_qn.get(q)) {
            Some(ids) => single(ids),
            None => by_name.get(callee).and_then(|ids| {
                let id = single(ids)?;
                // The soundness rule of `get_callers`: method syntax never reaches a
                // free function.
                (!(is_method && funcs[id].free)).then_some(id)
            }),
        };
        let Some(t) = target else { continue };
        let from_test = test_file[src] || test_fn.contains(&(src, text(r, 1)?.unwrap_or("")));
        if !from_test {
            in_degree[t] += 1;
        }
        let dst = funcs[t].file;
        if dst != src {
            let conf: f64 = r.get(5).map_err(store_err)?;
            *weights.entry((src, dst, from_test)).or_insert(0.0) += conf.max(0.3);
        }
    }
    drop(rows);
    drop(stmt);
    // Summed per file pair (row order is table order, so the sums are reproducible),
    // then sorted so everything downstream sees one fixed edge order.
    let mut edges: Vec<FileEdge> = weights.into_iter().map(|((a, b, t), w)| (a, b, w, t)).collect();
    edges.sort_by_key(|e| (e.0, e.1, e.3));

    edges.extend(import_edges);

    // ── sibling prior: chain consecutive production files of each directory ──
    let prod: Vec<usize> = (0..n).filter(|&i| !test_file[i]).collect();
    for w in prod.windows(2) {
        if parent(&rel[w[0]]) == parent(&rel[w[1]]) {
            edges.push((w[0], w[1], SIBLING_WEIGHT, false));
        }
    }

    // ── partition the production files ──
    let mut node_of: Vec<Option<usize>> = vec![None; n];
    for (k, &f) in prod.iter().enumerate() {
        node_of[f] = Some(k);
    }
    let prod_edges: Vec<(usize, usize, f64)> = edges
        .iter()
        .filter(|e| !e.3)
        .filter_map(|&(a, b, w, _)| Some((node_of[a]?, node_of[b]?, w)))
        .collect();
    let part = crate::cluster::louvain(prod.len(), &prod_edges);
    let mut comm: Vec<Option<usize>> = vec![None; n];
    for (k, &f) in prod.iter().enumerate() {
        comm[f] = Some(part[k]);
    }
    // ── attach each test file to the community it calls most (ties: lowest id) ──
    let mut pull: BTreeMap<(usize, usize), f64> = BTreeMap::new();
    for &(a, b, w, _) in &edges {
        if test_file[a] {
            if let Some(c) = comm[b] {
                *pull.entry((a, c)).or_insert(0.0) += w;
            }
        }
    }
    let mut test_home: Vec<Option<usize>> = vec![None; n];
    for (&(f, c), &w) in &pull {
        let better = match test_home[f] {
            None => true,
            Some(cur) => w > pull[&(f, cur)] + 1e-12,
        };
        if better {
            test_home[f] = Some(c);
        }
    }
    Ok(describe(&comm, &test_home, &edges, &rel, &funcs, &in_degree))
}

/// Borrow a TEXT column (no allocation); NULL reads as `None`.
fn text<'r>(r: &'r rusqlite::Row<'_>, i: usize) -> Result<Option<&'r str>> {
    r.get_ref(i).map_err(store_err)?.as_str_or_null().map_err(store_err)
}

/// The id when `ids` names exactly one definition.
fn single(ids: &[usize]) -> Option<usize> {
    (ids.len() == 1).then(|| ids[0])
}

fn resolve_import(module: &str, by_suffix: &HashMap<String, Vec<usize>>) -> Option<usize> {
    let norm = module.replace("::", "/").replace(['.', '\\'], "/");
    let mut segs: Vec<&str> = norm
        .split('/')
        .filter(|s| !s.is_empty() && !matches!(*s, "crate" | "self" | "super" | "@"))
        .collect();
    // `a::b::Item` names a symbol inside `a/b`: try the full path, then without the
    // last segment. Longest unique suffix wins.
    for _ in 0..2 {
        for k in 0..segs.len() {
            if let Some(ids) = by_suffix.get(&segs[k..].join("/")) {
                if ids.len() == 1 {
                    return Some(ids[0]);
                }
                break; // a shorter suffix only gets more ambiguous
            }
        }
        segs.pop();
        if segs.is_empty() {
            break;
        }
    }
    None
}

fn parent(rel: &str) -> &str {
    rel.rsplit_once('/').map(|(d, _)| d).unwrap_or("")
}

fn relative(path: &str, root: &str) -> String {
    if root.is_empty() {
        return path.to_string();
    }
    path.strip_prefix(root).map(|s| s.trim_start_matches('/').to_string()).unwrap_or_else(|| path.to_string())
}

/// Longest common DIRECTORY prefix of `paths` (never cuts inside a segment).
fn common_dir<'a>(mut paths: impl Iterator<Item = &'a str>) -> String {
    let Some(first) = paths.next() else { return String::new() };
    let mut prefix: Vec<&str> = parent(first).split('/').collect();
    for p in paths {
        let segs: Vec<&str> = parent(p).split('/').collect();
        let k = prefix.iter().zip(&segs).take_while(|(a, b)| a == b).count();
        prefix.truncate(k);
        if prefix.is_empty() {
            break;
        }
    }
    prefix.join("/")
}

fn describe(
    comm: &[Option<usize>],
    test_home: &[Option<usize>],
    edges: &[FileEdge],
    rel: &[String],
    funcs: &[Func],
    in_degree: &[usize],
) -> SubsystemMap {
    let k = comm.iter().flatten().copied().max().map(|m| m + 1).unwrap_or(0);
    let mut members: Vec<Vec<usize>> = vec![Vec::new(); k];
    for (f, c) in comm.iter().enumerate() {
        if let Some(c) = c {
            members[*c].push(f);
        }
    }
    let mut tests: Vec<usize> = vec![0; k];
    for c in test_home.iter().flatten() {
        tests[*c] += 1;
    }
    // Per-file centrality mass and function count.
    let mut file_rank = vec![0.0f64; rel.len()];
    let mut file_fns = vec![0usize; rel.len()];
    for f in funcs {
        file_rank[f.file] += f.rank;
        file_fns[f.file] += 1;
    }
    // Inter-community weights (production edges only).
    let mut between: BTreeMap<(usize, usize), f64> = BTreeMap::new();
    let mut degree = vec![0.0f64; rel.len()];
    for &(a, b, w, from_test) in edges {
        if from_test {
            continue;
        }
        let (Some(ca), Some(cb)) = (comm[a], comm[b]) else { continue };
        degree[a] += w;
        degree[b] += w;
        if ca != cb {
            *between.entry((ca.min(cb), ca.max(cb))).or_insert(0.0) += w;
        }
    }

    // Real subsystems have ≥ 2 files; ordered by size, then name.
    let mut order: Vec<usize> = (0..k).filter(|&c| members[c].len() >= 2).collect();
    let unclustered_files = (0..k).filter(|&c| members[c].len() < 2).map(|c| members[c].len()).sum();
    let mut names: Vec<String> = (0..k).map(|c| subsystem_name(&members[c], rel)).collect();
    // A directory shared by several subsystems (`src` in a flat crate) names none of
    // them: qualify those with their two most central files.
    let mut seen: HashMap<&str, usize> = HashMap::new();
    for &c in &order {
        *seen.entry(names[c].as_str()).or_insert(0) += 1;
    }
    let clash: std::collections::HashSet<String> =
        seen.into_iter().filter(|(_, n)| *n > 1).map(|(s, _)| s.to_string()).collect();
    for &c in &order {
        if clash.contains(&names[c]) {
            let mut files = members[c].clone();
            files.sort_by(|&a, &b| {
                file_rank[b].total_cmp(&file_rank[a]).then_with(|| rel[a].cmp(&rel[b]))
            });
            let base = common_dir(files.iter().map(|&f| rel[f].as_str()));
            let stems: Vec<String> = files.iter().take(2).map(|&f| file_label(&rel[f], &base)).collect();
            names[c] = format!("{}: {}", names[c], stems.join(", "));
        }
    }
    order.sort_by(|&a, &b| members[b].len().cmp(&members[a].len()).then_with(|| names[a].cmp(&names[b])));
    let public_id: HashMap<usize, usize> = order.iter().enumerate().map(|(i, &c)| (c, i)).collect();

    // The top symbol names the subsystem, so it must be unambiguous: skip names
    // defined more than once (every parser here has its own `walk`).
    let mut name_count: HashMap<&str, usize> = HashMap::new();
    for f in funcs {
        *name_count.entry(f.qualified.as_str()).or_insert(0) += 1;
    }
    let mut top_symbol: Vec<Option<(f64, &str)>> = vec![None; k];
    for f in funcs.iter().filter(|f| !f.is_test && name_count[f.qualified.as_str()] == 1) {
        let Some(c) = comm[f.file] else { continue };
        let better = match top_symbol[c] {
            None => true,
            Some((r, q)) => f.rank > r || (f.rank == r && f.qualified.as_str() < q),
        };
        if better && f.rank > 0.0 {
            top_symbol[c] = Some((f.rank, f.qualified.as_str()));
        }
    }

    let subsystems = order
        .iter()
        .map(|&c| {
            let mut files = members[c].clone();
            files.sort_by(|&a, &b| {
                file_rank[b]
                    .total_cmp(&file_rank[a])
                    .then_with(|| degree[b].total_cmp(&degree[a]))
                    .then_with(|| rel[a].cmp(&rel[b]))
            });
            let mut links: Vec<SubsystemLink> = between
                .iter()
                .filter_map(|(&(a, b), &w)| {
                    let other = if a == c { b } else if b == c { a } else { return None };
                    public_id.get(&other).map(|&to| SubsystemLink { to, weight: round1(w) })
                })
                .collect();
            links.sort_by(|a, b| b.weight.total_cmp(&a.weight).then(a.to.cmp(&b.to)));
            links.truncate(3);
            Subsystem {
                id: public_id[&c],
                name: names[c].clone(),
                top_symbol: top_symbol[c].map(|(_, q)| q.to_string()),
                files: members[c].len(),
                functions: members[c].iter().map(|&f| file_fns[f]).sum(),
                tests: tests[c],
                key_files: files.iter().take(3).map(|&f| rel[f].clone()).collect(),
                links,
            }
        })
        .collect();

    let mut hub_ids: Vec<usize> = (0..funcs.len()).filter(|&i| !funcs[i].is_test && in_degree[i] > 0).collect();
    // Centrality alone crowns orchestrators called once (`main`'s helpers); a raw
    // caller count crowns `fail`-style one-liners. Blend: rank × (1 + ln callers).
    let hub_score = |i: usize| funcs[i].rank * (1.0 + (in_degree[i] as f64).ln());
    hub_ids.sort_by(|&a, &b| {
        hub_score(b)
            .total_cmp(&hub_score(a))
            .then(in_degree[b].cmp(&in_degree[a]))
            .then_with(|| funcs[a].qualified.cmp(&funcs[b].qualified))
    });
    let hubs = hub_ids
        .into_iter()
        .take(HUBS_CACHED)
        .map(|i| Hub {
            symbol: funcs[i].qualified.clone(),
            path: rel[funcs[i].file].clone(),
            callers: in_degree[i],
            rank: (funcs[i].rank * 1000.0).round() / 1000.0,
            subsystem: comm[funcs[i].file].and_then(|c| public_id.get(&c).copied()),
        })
        .collect();

    SubsystemMap { subsystems, unclustered_files, hubs }
}

/// A file as a short label relative to `base`: no extension, and a `mod.rs` /
/// `index.ts` / `__init__.py` named by its directory.
fn file_label(rel: &str, base: &str) -> String {
    let p = if base.is_empty() { rel } else { rel.strip_prefix(base).unwrap_or(rel).trim_start_matches('/') };
    let (stem, ext) = p.rsplit_once('.').unwrap_or((p, ""));
    match stem.rsplit_once('/') {
        Some((dir, last))
            if (last == "mod" && ext == "rs")
                || (last == "__init__" && ext == "py")
                || (last == "index" && matches!(ext, "js" | "jsx" | "mjs" | "ts" | "tsx")) =>
        {
            dir.to_string()
        }
        _ => stem.to_string(),
    }
}

fn round1(w: f64) -> f64 {
    (w * 10.0).round() / 10.0
}

/// The common directory of the members; when they share none, the directory that
/// holds most of them with a `+` (it spills over).
fn subsystem_name(files: &[usize], rel: &[String]) -> String {
    let common = common_dir(files.iter().map(|&f| rel[f].as_str()));
    if !common.is_empty() || files.iter().all(|&f| parent(&rel[f]).is_empty()) {
        return if common.is_empty() { "(root)".to_string() } else { common };
    }
    let mut count: BTreeMap<&str, usize> = BTreeMap::new();
    for &f in files {
        *count.entry(parent(&rel[f])).or_insert(0) += 1;
    }
    let (dir, _) = count
        .iter()
        .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
        .map(|(d, n)| (*d, *n))
        .unwrap_or(("", 0));
    if dir.is_empty() { "(root)".to_string() } else { format!("{dir}+") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_dir_never_cuts_a_segment() {
        let p = ["/r/crates/a/src/x.rs", "/r/crates/ab/src/y.rs"];
        assert_eq!(common_dir(p.iter().copied()), "/r/crates");
        assert_eq!(common_dir(["/r/a/x.rs"].iter().copied()), "/r/a");
        assert_eq!(relative("/r/crates/a/x.rs", "/r/crates"), "a/x.rs");
        assert_eq!(file_label("src/store/mod.rs", "src"), "store");
        assert_eq!(file_label("src/verdict.rs", "src"), "verdict");
        assert_eq!(file_label("lib.rs", ""), "lib");
        assert_eq!(file_label("src/index.rs", "src"), "index", "index.rs is a real Rust module");
        assert_eq!(file_label("web/ui/index.tsx", "web"), "ui");
    }

    #[test]
    fn imports_resolve_by_unique_suffix() {
        let mut m: HashMap<String, Vec<usize>> = HashMap::new();
        m.insert("store/schema".into(), vec![3]);
        m.insert("schema".into(), vec![3]);
        m.insert("mod".into(), vec![1, 2]);
        assert_eq!(resolve_import("crate::store::schema", &m), Some(3));
        assert_eq!(resolve_import("crate::store::schema::SCHEMA", &m), Some(3));
        assert_eq!(resolve_import("x::mod", &m), None);
        assert_eq!(resolve_import("", &m), None);
    }
}
