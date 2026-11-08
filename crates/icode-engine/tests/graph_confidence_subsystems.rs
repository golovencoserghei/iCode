//! Edge certainty on call chains, and the `map` subsystem partition.
//!
//! (a) a chain is as trustworthy as its weakest hop, and `min_certainty` walks
//!     only edges at or above the level;
//! (b) a file graph with two obvious groups is split into exactly those two;
//! (c) the partition is deterministic and cached until the graph generation moves.

use std::fs;

use icode_core::model::Certainty;
use icode_engine::{index_path, SqliteCodeStore};

#[test]
fn a_chain_is_as_strong_as_its_weakest_hop() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("svc.rs"),
        r#"
pub struct Svc;
impl Svc {
    pub fn run() -> u32 { helper() }
}
pub fn helper() -> u32 { 1 }
pub fn entry() -> u32 { Svc::run() }
"#,
    )
    .unwrap();
    let store = SqliteCodeStore::open(dir.path()).unwrap();
    index_path(dir.path(), &store).unwrap();

    let hops = store
        .call_chain_hops("entry", "helper", 10, Certainty::Ambiguous)
        .unwrap()
        .expect("entry reaches helper");
    let names: Vec<_> = hops.iter().map(|h| (h.from.as_str(), h.to.as_str(), h.certainty)).collect();
    assert_eq!(
        names,
        vec![("entry", "run", Certainty::Exact), ("run", "helper", Certainty::Inferred)],
        "Svc::run() is a qualified unique target (exact), helper() a bare unique one (inferred)"
    );
    let weakest = icode_core::model::weakest_link(&hops.iter().map(|h| h.confidence).collect::<Vec<_>>());
    assert_eq!(Certainty::from_confidence(weakest), Certainty::Inferred);

    // Only exact edges: the inferred hop is not walked, so no trusted path exists.
    assert!(store.call_chain_hops("entry", "helper", 10, Certainty::Exact).unwrap().is_none());
    assert!(store.call_chain_hops("entry", "run", 10, Certainty::Exact).unwrap().is_some());
    // Same name both ends; unreachable direction; zero depth.
    assert!(store.call_chain_hops("helper", "helper", 10, Certainty::Ambiguous).unwrap().is_some_and(|h| h.is_empty()));
    assert!(store.call_chain_hops("helper", "entry", 10, Certainty::Ambiguous).unwrap().is_none());
    assert!(store.call_chain_hops("entry", "helper", 0, Certainty::Ambiguous).unwrap().is_none());
}

/// Files of two groups interleaved in ONE directory, so the directory prior cannot
/// separate them: only the calls can.
fn two_group_tree(root: &std::path::Path) {
    for g in ["alpha", "beta"] {
        for i in 0..3 {
            let next = (i + 1) % 3;
            let prev = (i + 2) % 3;
            let body = format!(
                "pub fn {g}_{i}_a() -> u32 {{ {g}_{next}_a() + {g}_{next}_b() + {g}_{prev}_a() + {g}_{prev}_b() }}\n\
                 pub fn {g}_{i}_b() -> u32 {{ {g}_{next}_a() + {g}_{prev}_b() }}\n"
            );
            // Interleave names: a_alpha0, a_beta0, b_alpha1, ...
            fs::write(root.join(format!("f{i}_{g}.rs")), body).unwrap();
        }
    }
    // One thin bridge between the groups.
    fs::write(root.join("f9_bridge.rs"), "pub fn bridge() -> u32 { alpha_0_a() }\n").unwrap();
}

#[test]
fn two_obvious_groups_become_two_subsystems_deterministically() {
    let dir = tempfile::tempdir().unwrap();
    two_group_tree(dir.path());
    let store = SqliteCodeStore::open(dir.path()).unwrap();
    index_path(dir.path(), &store).unwrap();

    let map = store.subsystems(None).unwrap();
    let groups: Vec<Vec<String>> = map
        .subsystems
        .iter()
        .map(|s| {
            let mut f = s.key_files.clone();
            f.sort();
            f
        })
        .collect();
    assert_eq!(map.subsystems.len(), 2, "{:#?}", map.subsystems);
    for s in &map.subsystems {
        assert!(s.files >= 3, "{s:?}");
        let alpha = s.key_files.iter().filter(|f| f.contains("alpha")).count();
        let beta = s.key_files.iter().filter(|f| f.contains("beta")).count();
        assert!(alpha == 0 || beta == 0, "a subsystem mixes the groups: {groups:?}");
        assert!(s.top_symbol.is_some(), "{s:?}");
        assert_eq!(s.links.len(), 1, "the two groups touch: {s:?}");
    }
    assert!(!map.hubs.is_empty());
    assert!(map.hubs.iter().all(|h| h.callers > 0));

    // Cached: same answer without recomputing; recomputed after the generation moves
    // (compute_symbol_ranks runs after every re-index batch) and still identical.
    let again = store.subsystems(None).unwrap();
    assert_eq!(serde_json::to_string(&map).unwrap(), serde_json::to_string(&again).unwrap());
    store.compute_symbol_ranks().unwrap();
    let recomputed = store.subsystems(None).unwrap();
    assert_eq!(serde_json::to_string(&map).unwrap(), serde_json::to_string(&recomputed).unwrap());

    // An independent store over the same tree gets the same partition.
    let dir2 = tempfile::tempdir().unwrap();
    two_group_tree(dir2.path());
    let store2 = SqliteCodeStore::open(dir2.path()).unwrap();
    index_path(dir2.path(), &store2).unwrap();
    let other = store2.subsystems(None).unwrap();
    assert_eq!(serde_json::to_string(&map).unwrap(), serde_json::to_string(&other).unwrap());
}

#[test]
fn an_empty_index_has_no_subsystems() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteCodeStore::open(dir.path()).unwrap();
    let map = store.subsystems(None).unwrap();
    assert!(map.subsystems.is_empty() && map.hubs.is_empty());
}
