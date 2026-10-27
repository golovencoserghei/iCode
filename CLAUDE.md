# Versioning rule (mandatory)

Versions are bumped by CI, not by hand. On every push to the main branch (a merged PR),
`.github/workflows/release.yml` runs `node scripts/bump-version.mjs auto`, commits
`chore(release): vX.Y.Z [skip ci]`, tags, builds the binaries and publishes a GitHub release.

The level is taken from the commit messages merged since the last `v*` tag, so write
Conventional Commit subjects (in English) — this is how the size of a change is declared:

- **major** (0.4.0 → 1.0.0) — `type!:` (e.g. `feat!: ...`) or a `BREAKING CHANGE:` line in the body.
- **minor** (0.3.1 → 0.4.0) — any `feat:` / `feat(scope):` commit: a new feature, dialog, setting, endpoint.
- **patch** (0.3.0 → 0.3.1) — everything else: `fix:`, `refactor:`, `style:`, `docs:`, untagged messages.

Rules for the agent:
- Do NOT edit version numbers manually and do not run the bump script on a feature branch;
  CI owns them. Local build: `node scripts/bump-version.mjs auto --dry` only previews the next version.
- Pick the commit prefix deliberately (`feat:` for medium changes, `!` for large ones).
- Tell the user which version the merge is expected to produce.
