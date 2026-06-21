# trail (native Python bindings)

In-process Python bindings via [pyo3](https://pyo3.rs) - no subprocess. `import
trail` and drive the coverage scheduler directly. Methods return the same dict
shapes as the CLI / the thin wrapper, so they're drop-in compatible.

This is a standalone Cargo workspace (it path-depends on `../../crates/trail-core`)
so the main workspace and its CI are unaffected.

## Build

```bash
pip install maturin        # or: uv tool install maturin
maturin develop --release  # build + install into the active virtualenv
# or build a distributable wheel:
maturin build --release    # abi3 wheel, works on CPython >= 3.9
```

On a Python newer than the pinned pyo3 supports, set
`PYO3_USE_ABI3_FORWARD_COMPATIBILITY=1` before building (abi3 keeps it safe).

## Use

```python
import trail

t = trail.Trail("/repo")
t.init()
while (r := t.next("refine", agent="a1"))["status"] == "ok":
    investigate(r["path"])
    t.done("refine", r["path"], agent="a1", found=3)   # outcome feedback

print(t.status("refine"))
```

Methods: `init()`, `next(task, agent=None, auto_sweep=False)`, `done(...)`,
`skip(...)`, `status(task)`, `list(task, state=None)`, `sweep_new(task)`,
`reset(task, all=False)`, `gc(vacuum=False)`. `done`/`skip` accept
`found=N` / `reason=...`.
