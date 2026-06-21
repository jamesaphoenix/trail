# trail

A tiny coverage scheduler with memory, for AI coding agents running in loops.

`trail` hands an agent the next **folder** to work on for a named **task**,
leases it so parallel agents never collide, records the visit, and uses that
history to order future work. It replaces hand-written "go look for P0/P1/P2
bugs in feature X" JSON files that stop scaling as a codebase grows.

> Name note: `trail` is a working name (the tool leaves a trail of where agents
> have been). Rename later via the workspace package if you like.

## Why

When you run a Ralph-style loop that spawns a fresh agent each iteration, the
agents have no shared memory of where prior iterations already looked, so they
collide or re-tread the same folders. `trail` owns that coverage state as a
single small SQLite file and exposes it through a CLI (plus thin Python/Node
wrappers), so any harness that can run a shell command can coordinate cleanly.

## Model

- **Folder** is the unit of work.
- A **sweep** is one-pass coverage: each folder is handed out exactly once, then
  the sweep is complete. The outer loop owns starting the next sweep.
- **Memory is per task-name and persists across sweeps.** When a sweep opens,
  each folder's priority is frozen from staleness (time since it was last
  visited under this task) plus a static weight (file count / size / churn). If
  a sweep is cut short, the stalest and heaviest folders were surfaced first.
- **Strategies** only reorder the one-pass drain: `weighted` (default),
  `round-robin` (pure least-recently-visited), `random` (seeded, reproducible).
- **Outcome feedback** (opt-in via `strategy.outcome_weight`): agents report
  findings with `done --found N` / `--clean`, and folders that recently found
  more are surfaced earlier in future sweeps. Off by default.
- **Leases** make parallel work safe: a claimed folder is held for a TTL; if the
  agent dies the lease expires and the folder returns to the pool, so a sweep
  completes only when every folder is genuinely covered.

## Build / install

```bash
cargo build --release          # binary at target/release/trail

# install the `trail` binary onto your PATH:
cargo install --path crates/trail-cli
# (the crate is `trail-cli` but its [[bin]] is named `trail`, so the installed
#  command is `trail`.)

# optional: enable the git-churn static signal (needs cmake for vendored libgit2)
cargo build --release --features churn
```

The language wrappers locate the binary via `PATH`, or via the `TRAIL_BIN`
environment variable (e.g. `TRAIL_BIN=/path/to/trail`).

Tagged releases (`v*`) build prebuilt binaries for Linux, macOS (Intel + Apple
Silicon), and Windows via GitHub Actions.

Shell completions:

```bash
trail completions zsh  > ~/.zfunc/_trail      # or bash / fish / powershell / elvish
```

## Use

```bash
trail init                              # scan the tree, register folders
trail next --task refine --agent a1     # claim a folder (JSON on stdout)
trail done --task refine --path src/api --agent a1
trail status --task refine
```

`trail init` also writes a sample `.trail.toml.example` when none exists (the
`wrote_example_config` field reports whether it did).

Exit codes carry the loop outcome: `0` ok, `3` sweep-complete, `4`
none-available (leased elsewhere, retry), `1` error, `2` usage. See
[`skills/trail/SKILL.md`](skills/trail/SKILL.md) for the full command reference
and the correct parallel-safe loop.

## Config (`.trail.toml`)

Commit `.trail.toml`; gitignore `.trail/` (the state DB). `.gitignore`/`.ignore`
and hidden dirs are excluded for free; config only layers extra globs. See
[`.trail.toml.example`](.trail.toml.example).

## Wrappers and bindings

Thin wrappers (shell out to the binary, parse JSON):

- Python: [`wrappers/python`](wrappers/python)
- TypeScript / Node: [`wrappers/typescript`](wrappers/typescript)
- Go: [`wrappers/go`](wrappers/go)
- Ruby: [`wrappers/ruby`](wrappers/ruby)

Native, in-process bindings (no subprocess; same shapes as the CLI):

- Python via pyo3: [`bindings/python`](bindings/python)
- Node via napi-rs: [`bindings/node`](bindings/node)

The shell-out + JSON + exit-code contract makes a thin wrapper in any language
trivial; the native bindings run the scheduler directly for hot loops.

## Layout

```
crates/trail-core   library: walk, scoring, SQLite store + atomic claim/lease
crates/trail-cli    the `trail` binary
wrappers/           thin Python + TypeScript wrappers (shell out)
bindings/           native in-process bindings (pyo3 Python, napi-rs Node)
skills/trail        SKILL.md for agents
```

## Performance

State lives in one SQLite file and the hot path is built to stay flat as a
codebase grows: `claim`, `complete`, and `status` are O(1) (per-sweep counters,
not `COUNT(*)` scans), and SQL is compiled once per process (`prepare_cached`).
Draining a 50k-folder sweep takes ~1.1s (~22us per claim+complete cycle, flat
from 1k to 50k folders). Run the micro-benchmark with:

```bash
cargo run --release --example bench -p trail-core
```

For tight loops, the native [`bindings/`](bindings) avoid per-call process
spawn entirely. If counters ever drift, `trail gc --reconcile` rebuilds them.

## Test

```bash
cargo test          # unit, scoring, lifecycle, concurrency, CLI e2e
```
