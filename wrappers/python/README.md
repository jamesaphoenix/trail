# trail (Python wrapper)

A ~5kb wrapper that shells out to the `trail` binary and parses its JSON. The
CLI is the source of truth; this just makes it ergonomic from Python.

Requires the `trail` binary on `PATH` (or set `TRAIL_BIN=/path/to/trail`).

```python
import trail

trail.init(root="/repo")
for folder in trail.folders("refine", agent="worker-1", root="/repo"):
    investigate(folder["path"])
    trail.done("refine", folder["path"], agent="worker-1", root="/repo")

print(trail.status("refine", root="/repo"))
```

- `claim(task, ...)` returns the next folder dict, blocks/retries while folders
  are only leased elsewhere, and returns `None` when the sweep is complete.
- `folders(task, ...)` is a generator over `claim` until the sweep completes.
- `done` / `skip` / `status` / `new_sweep` map to the matching CLI commands.
