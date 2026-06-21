# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Outcome-feedback weighting: `done`/`skip` accept `--found N` (or `--clean` =
  0) to record findings. With the new `strategy.outcome_weight` config (default
  0 = off, so existing behavior is unchanged), folders that recently reported
  more findings surface earlier in future sweeps. Wrappers gained `found`/`clean`.
- Native in-process bindings (no subprocess): `bindings/python` (pyo3, abi3
  wheel via maturin) and `bindings/node` (napi-rs). Both expose a `Trail`
  handle returning the same shapes as the CLI, and live in standalone
  workspaces so the main crate's CI is unaffected.
- `trail completions <shell>` prints a shell completion script (bash, zsh, fish,
  powershell, elvish) and works with no repo/config present.
- Release workflow: tagged versions (`v*`) build prebuilt binaries for Linux,
  macOS (x86_64 + aarch64), and Windows and attach them to a GitHub release.

### Fixed

- Concurrency: opening a sweep now reads `current_sweep` inside the write
  transaction and refuses while a sweep is active, so concurrent
  `next --auto-sweep` / `sweep new` can no longer collide on the sweeps key or
  open two overlapping active sweeps. A losing opener claims from the winner's
  sweep instead of erroring.

### Packaging

- `trail-core` packages/publishes cleanly: the example config is embedded
  (no `include_str!` escaping the crate); a test keeps the committed
  `.trail.toml.example` identical to it.
- `trail-cli`'s path dependency on `trail-core` carries a version; crates.io
  metadata (repository/homepage/keywords/categories) and a declared MSRV
  (`rust-version = 1.85`, CI-verified) added.
- Removed accidentally-committed Python bytecode; `.gitignore` now covers
  `__pycache__/`, `*.py[cod]`, and `node_modules/`.

## [0.1.0]

Initial release.

### Added

- `trail-core`: folder walking with sane excludes (`.gitignore` + language
  defaults via the `ignore` crate), recency + static-weight scoring, three
  ordering strategies (`weighted`, `round-robin`, seeded `random`), and a
  SQLite-backed store with atomic claim + lease so parallel agents never get
  the same folder.
- `trail` CLI: `init`, `next`, `done`, `skip`, `status`, `list`, `sweep new`,
  `sweep show`, `reset`, `gc`. Every command prints one JSON object; exit codes
  carry the loop outcome (`0` ok, `3` sweep-complete, `4` none-available,
  `1` error, `2` usage).
- One-pass coverage per sweep with per-task-name memory across sweeps: a sweep's
  ordering is frozen from staleness (time since last visit under the task) plus
  a static weight (file count / size / optional git churn).
- Lease TTL with automatic reclamation of crashed agents' folders.
- Optional `churn` feature (git churn as the static weight, via vendored
  libgit2).
- Thin Python and TypeScript/Node wrappers that shell out to the binary.
- `SKILL.md` documenting the agent loop, and a sample `.trail.toml`.

[Unreleased]: https://example.com/compare/v0.1.0...HEAD
[0.1.0]: https://example.com/releases/tag/v0.1.0
