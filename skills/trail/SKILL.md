---
name: trail
description: Coverage scheduler with memory for agentic loops. Use when an agent (or a fleet of agents in a Ralph-style loop) needs to systematically cover a codebase folder-by-folder for a named task (e.g. "find P0/P1/P2 bugs", "refine the codebase"), without re-treading folders, colliding with other agents, or losing track of what was visited across runs.
---

# trail

`trail` hands you the next **folder** to work on for a named **task**, leases it
so parallel agents never get the same one, records that you visited it, and uses
that history to order future work (stale and heavier folders first). It replaces
hand-written "go look at feature X" JSON files.

You drive it entirely through the `trail` CLI. Every command prints one JSON
object to stdout; the **exit code** tells you what to do next.

## Agent Guidance

- Use `trail` when you are in a loop covering a codebase for a task, especially
  when other agents may be working the same task in parallel.
- The loop is always: **claim with `next` → investigate the folder → report with
  `done` (or `skip`)**. Only ever work on what `next` handed you. Never pick a
  folder yourself.
- Always close the loop on a claimed folder with `done` or `skip`. If you crash,
  the lease expires and the folder is re-handed-out, but completing keeps
  coverage moving.
- Branch on the **exit code**, not by parsing where you can avoid it:
  - `0` ok: a folder was leased to you (`.path` in the JSON).
  - `3` sweep-complete: every folder is covered. Stop, or open a new sweep.
  - `4` none-available: nothing pending right now, but other agents hold leases.
    Wait a moment and retry. Do **not** treat this as "done".
  - `1` error (stderr has a JSON error); `2` usage error.
- Choose a stable, meaningful `--task` name. History accumulates under it, so
  reuse the same name across loop iterations to benefit from memory. Use
  different names for genuinely different campaigns (e.g. `p0-bug-sweep` vs
  `test-coverage`).
- Pass a stable `--agent <id>` so leases are attributable.

## Quick start

```bash
trail init                       # scan the tree once, register the folder set
trail next --task refine --agent a1   # claim a folder
trail done --task refine --path src/api --agent a1   # report it covered
trail status --task refine       # how much of the sweep is left
```

## The loop pattern (correct, parallel-safe)

A naive `while out=$(trail next ...)` stops on every non-zero code, which wrongly
treats exit 4 (none-available) as "finished". Branch on the code instead:

```bash
AGENT="finder-$$"
TASK="refine"
trail init                       # once per repo (or to rescan)

while :; do
  out=$(trail next --task "$TASK" --agent "$AGENT"); code=$?
  case $code in
    0)  path=$(jq -r .path <<<"$out")
        # ... investigate "$path" for this task ...
        trail done --task "$TASK" --path "$path" --agent "$AGENT" >/dev/null ;;
    3)  echo "sweep complete"; break ;;        # all folders covered
    4)  sleep 2; continue ;;                    # leased elsewhere; retry
    *)  echo "trail error: $out" >&2; break ;;
  esac
done
```

Single-agent loops never see exit 4, so the short form
`while out=$(trail next ...); do ...; done` is fine there. The form above is
required only when multiple agents share a task.

## Command reference

| Command | What it does | Exit |
|---------|--------------|------|
| `trail init` | Scan the tree, register the folder snapshot, write `.trail.toml.example`. | 0 |
| `trail next --task <t> [--agent <id>] [--strategy <s>] [--auto-sweep]` | Claim + lease the next folder. Bootstraps the first sweep automatically. `--auto-sweep` rolls into a new sweep instead of reporting complete. | 0 / 3 / 4 |
| `trail done --task <t> --path <p> [--agent <id>]` | Mark the folder covered; append to history. Errors (exit 1) if the path is not an active work item; re-doing a finished one is a no-op. | 0/1 |
| `trail skip --task <t> --path <p> [--agent <id>] [--reason <r>]` | Mark covered-but-skipped; append to history (with reason). Same miss-errors as `done`. | 0/1 |
| `trail status --task <t>` | Coverage of the latest sweep. | 0 |
| `trail list --task <t> [--state pending\|leased\|done\|skipped]` | Work items in the latest sweep, ordered by score. | 0 |
| `trail sweep new --task <t> [--rescan]` | Open a fresh sweep (the outer loop owns re-running). Errors if a sweep is still active. | 0/1 |
| `trail sweep show --task <t>` | Show the latest sweep. | 0 |
| `trail reset --task <t> [--all]` | Clear sweeps; `--all` also wipes visit history. | 0 |
| `trail gc [--vacuum]` | Reclaim expired leases; `--vacuum` also compacts the DB (best-effort). | 0 |

`--root <dir>` is global and sets the project root (defaults to the cwd). State
lives at `<root>/.trail/state.db`; config at `<root>/.trail.toml`.

### Output shapes

```jsonc
// init
{"folders":42,"excluded":3,"wrote_example_config":true}
// next (ok)
{"status":"ok","task":"refine","sweep":1,"path":"src/api","score":0.81,"lease_expires_at":1781990000,"remaining":7}
// next (sweep complete) — exit 3. total==0 (+ a note) means nothing was registered.
{"status":"sweep-complete","task":"refine","sweep":1,"covered":8,"total":8}
// next (none available) — exit 4
{"status":"none-available","task":"refine","sweep":1,"leased_outstanding":2}
// done / skip
{"status":"done","task":"refine","sweep":1,"path":"src/api","remaining":6,"sweep_complete":false}
// status
{"task":"refine","sweep":1,"sweep_status":"active","total":8,"done":2,"leased":1,"pending":5,"skipped":0,"percent":25.0}
// list (array of rows)
[{"path":"src/api","status":"pending","score":0.81,"lease_owner":null,"lease_expires_at":null,"visited_at":null}]
// sweep show
{"task":"refine","sweep":1,"sweep_status":"active","total":8,"started_at":1781989000,"completed_at":null}
// reset
{"task":"refine","cleared_sweeps":2,"cleared_history":false}
// gc
{"reclaimed_leases":1}
```

## How it works (just enough)

- A **sweep** is one-pass coverage: each folder is handed out exactly once, then
  the sweep is complete. The outer loop owns starting the next sweep
  (`trail sweep new`, or `next --auto-sweep`).
- **Memory is per task-name and lives across sweeps.** When a sweep opens, each
  folder's priority is frozen from how long ago it was last visited under this
  task (staleness) plus a static weight (file count / size / churn). So if a
  sweep is cut short, the stalest and heaviest folders were surfaced first.
- **Strategies** only change the order of the one-pass drain: `weighted`
  (default), `round-robin` (pure least-recently-visited), `random` (seeded,
  reproducible).
- **Leases** make parallel work safe: a claimed folder is held for
  `lease.ttl_secs`; if the agent dies, the lease expires and the folder returns
  to the pool, so a sweep only completes when every folder is genuinely covered.

## Notes

- Excludes are sane by default: `.gitignore`/`.ignore` are honored and hidden
  dirs (`.git`, `.trail`) are skipped. Add extra include/exclude globs in
  `.trail.toml`.
- Commit `.trail.toml`; gitignore `.trail/` (the state DB).
- Re-`init` (or `sweep new --rescan`) after the tree changes a lot; new folders
  are treated as maximally stale and surface early.
- A stalled sweep waits up to `lease.ttl_secs` (default 900s) for a crashed
  agent's lease to expire. For fast loops, lower `lease.ttl_secs` in
  `.trail.toml` so the exit-4 wait is short; `trail gc` reclaims expired leases
  on demand.
