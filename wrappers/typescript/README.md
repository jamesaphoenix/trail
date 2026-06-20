# trail (TypeScript / Node wrapper)

A thin wrapper that shells out to the `trail` binary and parses its JSON. The
CLI is the source of truth; this just makes it ergonomic from Node. Shipped as
ESM with `.d.ts` types, no build step required.

Requires the `trail` binary on `PATH` (or set `TRAIL_BIN=/path/to/trail`).

```js
import * as trail from "@trail/cli-wrapper";

trail.init("/repo");
for (const folder of trail.folders("refine", { agent: "worker-1", root: "/repo" })) {
  investigate(folder.path);
  trail.done("refine", folder.path, { agent: "worker-1", root: "/repo" });
}

console.log(trail.status("refine", { root: "/repo" }));
```

- `claim(task, opts)` returns the next folder, blocks/retries while folders are
  only leased elsewhere, and returns `null` when the sweep is complete.
- `folders(task, opts)` is a generator over `claim` until the sweep completes.
- `done` / `skip` / `status` / `newSweep` map to the matching CLI commands.
