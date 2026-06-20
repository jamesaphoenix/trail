// Thin TypeScript/JavaScript wrapper around the `trail` CLI.
//
// Shells out to the `trail` binary and parses its JSON. No native logic: the
// CLI is the source of truth. Shipped as runnable ESM with companion .d.ts
// types, so it needs no build step.
//
// Binary discovery: the TRAIL_BIN env var, else `trail` on PATH.

import { spawnSync } from "node:child_process";

export const EXIT_OK = 0;
export const EXIT_ERROR = 1;
export const EXIT_SWEEP_COMPLETE = 3;
export const EXIT_NONE_AVAILABLE = 4;

export class TrailError extends Error {}

function bin() {
  return process.env.TRAIL_BIN || "trail";
}

function run(args, root) {
  const argv = [];
  if (root) argv.push("--root", root);
  argv.push(...args);
  const res = spawnSync(bin(), argv, { encoding: "utf8" });
  if (res.error) throw new TrailError(`failed to run trail: ${res.error.message}`);
  const out = (res.stdout || "").trim();
  let data = {};
  if (out) {
    try {
      data = JSON.parse(out.split("\n").pop());
    } catch {
      data = {};
    }
  }
  const code = res.status ?? EXIT_ERROR;
  if (code === EXIT_ERROR) {
    throw new TrailError(data.error || (res.stderr || "").trim() || "trail error");
  }
  return { code, data };
}

// Synchronous sleep so the blocking claim/loop stays simple.
function sleepSync(ms) {
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);
}

export function init(root) {
  return run(["init"], root).data;
}

/**
 * Claim the next folder. Returns the folder object on success, or null when the
 * sweep is complete. Blocks and retries while folders are only leased elsewhere.
 *
 * The exit-4 retry is unbounded by default (a crashed agent's lease frees up
 * after lease.ttl_secs). Set opts.maxAttempts to cap it and throw instead.
 */
export function claim(task, opts = {}) {
  const { agent, root, strategy, autoSweep = false, pollMs = 2000, maxAttempts } = opts;
  const args = ["next", "--task", task];
  if (agent) args.push("--agent", agent);
  if (strategy) args.push("--strategy", strategy);
  if (autoSweep) args.push("--auto-sweep");
  let attempts = 0;
  for (;;) {
    const { code, data } = run(args, root);
    if (code === EXIT_OK) return data;
    if (code === EXIT_SWEEP_COMPLETE) return null;
    if (code === EXIT_NONE_AVAILABLE) {
      attempts += 1;
      if (maxAttempts != null && attempts >= maxAttempts) {
        throw new TrailError(
          `no folder available after ${attempts} attempts (all leased elsewhere); ` +
            "consider a shorter lease.ttl_secs"
        );
      }
      sleepSync(pollMs);
      continue;
    }
    throw new TrailError(data.error || `unexpected exit code ${code}`);
  }
}

/** Iterate folders until the sweep completes. Remember to call done/skip. */
export function* folders(task, opts = {}) {
  for (;;) {
    const folder = claim(task, opts);
    if (folder === null) return;
    yield folder;
  }
}

export function done(task, path, opts = {}) {
  const args = ["done", "--task", task, "--path", path];
  if (opts.agent) args.push("--agent", opts.agent);
  return run(args, opts.root).data;
}

export function skip(task, path, opts = {}) {
  const args = ["skip", "--task", task, "--path", path];
  if (opts.agent) args.push("--agent", opts.agent);
  if (opts.reason) args.push("--reason", opts.reason);
  return run(args, opts.root).data;
}

export function status(task, opts = {}) {
  return run(["status", "--task", task], opts.root).data;
}

export function newSweep(task, opts = {}) {
  const args = ["sweep", "new", "--task", task];
  if (opts.rescan) args.push("--rescan");
  return run(args, opts.root).data;
}

export function list(task, opts = {}) {
  const args = ["list", "--task", task];
  if (opts.state) args.push("--state", opts.state);
  const data = run(args, opts.root).data;
  return Array.isArray(data) ? data : [];
}

export function reset(task, opts = {}) {
  const args = ["reset", "--task", task];
  if (opts.all) args.push("--all");
  return run(args, opts.root).data;
}

export function gc(opts = {}) {
  const args = ["gc"];
  if (opts.vacuum) args.push("--vacuum");
  return run(args, opts.root).data;
}
