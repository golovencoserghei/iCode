//! Sub-project (module) boundaries inside one indexed tree.
//!
//! A monorepo is one directory holding several projects that talk to each other, and
//! people open it as ONE workspace precisely so they can move between them. iCode
//! indexed it happily — and then lied about it, because the call graph resolves by
//! NAME and a monorepo is where names collide hardest.
//!
//! Measured on a real 7-project monorepo (4485 files):
//!
//! ```text
//!   `get`     defined in 4 sub-projects  -> 4122 fabricated cross-project edges
//!   `add`     defined in 5
//!   `health`  defined in 6
//! ```
//!
//! The graph confidently reported `onyx -> data-gateway x4964`. Not one of those calls
//! is real: `onyx` calls its OWN `get`, and the resolver credited it to a `get` in a
//! 29-file sibling. Every consumer inherited the fiction — impact analysis, centrality,
//! callers. An agent reading that graph is reading a map of a place that does not exist.
//!
//! The fix is a boundary. A call in `onyx/foo.py` to a name that `onyx` itself defines
//! resolves INSIDE `onyx` — it cannot mean the sibling's homonym.

use std::path::Path;

/// Files that mark a directory as the root of its own project. `.git` is checked
/// separately (a nested repo is the strongest signal of all, and the one that made this
/// monorepo's members separate projects in the first place).
const PROJECT_MARKERS: &[&str] = &[
    "pyproject.toml",
    "setup.py",
    "requirements.txt",
    "package.json",
    "Cargo.toml",
    "go.mod",
    "composer.json",
    "pom.xml",
    "build.gradle",
];

/// The module (sub-project) a file belongs to, as a path RELATIVE to `root`.
///
/// Walks up from the file to the nearest ancestor (at or below `root`) that carries a
/// project marker — a nested `.git`, a manifest — and returns that directory's path
/// relative to `root`. A file that belongs to no sub-project returns `""`, the root
/// module, so a single-project repo behaves exactly as before: one module, no
/// boundaries, nothing changes.
///
/// Relative rather than absolute so the value survives a move/worktree, like the chunk
/// hashes do.
pub fn module_of(path: &Path, root: &Path) -> String {
    let Ok(rel) = path.strip_prefix(root) else {
        return String::new();
    };

    // Walk DOWN from the root and stop at the FIRST directory that roots a project.
    //
    // The shallowest marker wins, not the deepest. The boundary we need is the PROJECT,
    // not every package inside it. Taking the deepest marker split one 4000-file service
    // into eight modules (`onyx`, `onyx/web`, `onyx/backend/onyx`, …) because each
    // carries its own `package.json`/`pyproject.toml` — and then calls between a
    // project's own front and back end counted as cross-module, which is both wrong and
    // costly: an ambiguous cross-module name gets dropped, so real edges would be lost.
    //
    // At the project level the boundary is exactly right: the sibling services each have
    // their own `.git`, and nothing inside one of them does.
    let mut cur = root.to_path_buf();
    let comps: Vec<_> = rel.components().collect();
    // The final component is the file itself — only directories can be module roots.
    for comp in comps.iter().take(comps.len().saturating_sub(1)) {
        cur.push(comp.as_os_str());
        if is_project_root(&cur) {
            return cur
                .strip_prefix(root)
                .map(|r| r.to_string_lossy().to_string())
                .unwrap_or_default();
        }
    }
    String::new()
}

/// Does this directory root its own project?
fn is_project_root(dir: &Path) -> bool {
    if dir.join(".git").exists() {
        return true;
    }
    PROJECT_MARKERS.iter().any(|m| dir.join(m).exists())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(p: &Path) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, "").unwrap();
    }

    #[test]
    fn a_single_project_repo_has_one_root_module() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        touch(&root.join("Cargo.toml"));
        touch(&root.join("src/lib.rs"));
        // Everything belongs to the root module: no boundaries, behaviour unchanged.
        assert_eq!(module_of(&root.join("src/lib.rs"), root), "");
    }

    #[test]
    fn a_monorepo_member_is_its_own_module() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        // The outer repo…
        touch(&root.join(".git/HEAD"));
        // …and two members, each a project in its own right.
        touch(&root.join("internal-agent/requirements.txt"));
        touch(&root.join("internal-agent/pm_agent/api.py"));
        touch(&root.join("data-gateway/requirements.txt"));
        touch(&root.join("data-gateway/app.py"));

        assert_eq!(
            module_of(&root.join("internal-agent/pm_agent/api.py"), root),
            "internal-agent"
        );
        assert_eq!(module_of(&root.join("data-gateway/app.py"), root), "data-gateway");
    }

    #[test]
    fn a_nested_git_repo_is_the_strongest_marker() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        touch(&root.join(".git/HEAD"));
        touch(&root.join("svc/.git/HEAD")); // a nested repo, no manifest at all
        touch(&root.join("svc/main.go"));
        assert_eq!(module_of(&root.join("svc/main.go"), root), "svc");
    }

    #[test]
    fn the_shallowest_marker_wins_so_a_project_is_not_split_into_its_packages() {
        // A service whose front and back end each carry their own manifest is still ONE
        // project. Taking the deepest marker split a real 4000-file service into eight
        // modules and made calls between its own halves "cross-module" — where an
        // ambiguous name is dropped, so real edges would have been lost.
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        touch(&root.join("svc/.git/HEAD"));
        touch(&root.join("svc/package.json"));
        touch(&root.join("svc/web/package.json"));
        touch(&root.join("svc/web/app.js"));
        assert_eq!(module_of(&root.join("svc/web/app.js"), root), "svc");
    }

    #[test]
    fn a_file_outside_any_member_falls_back_to_the_root_module() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        touch(&root.join(".git/HEAD"));
        touch(&root.join("docs/notes.py")); // `docs` carries no marker
        assert_eq!(module_of(&root.join("docs/notes.py"), root), "");
    }
}
