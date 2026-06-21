# trail (native Node.js bindings)

In-process Node bindings via [napi-rs](https://napi.rs) - no subprocess. A
`Trail` class drives the coverage scheduler directly; methods return the same
object shapes as the CLI / the thin wrapper.

This is a standalone Cargo workspace (it path-depends on `../../crates/trail-core`)
so the main workspace and its CI are unaffected.

## Build

```bash
npm install                # or: npx -y @napi-rs/cli build --release
npm run build              # -> trail-node.<platform>.node (+ index.d.ts)
```

## Use

```js
const { Trail } = require("./trail-node.node"); // or the generated index.js

const t = new Trail("/repo");
t.init();
let r;
while ((r = t.next("refine", "a1")).status === "ok") {
  investigate(r.path);
  t.done("refine", r.path, "a1", 3); // task, path, agent, found
}
console.log(t.status("refine"));
```

Methods: `init()`, `next(task, agent?, autoSweep?)`, `done(task, path, agent?,
found?, reason?)`, `skip(...)`, `status(task)`, `list(task, state?)`,
`sweepNew(task)`, `reset(task, all?)`, `gc(vacuum?)`.
