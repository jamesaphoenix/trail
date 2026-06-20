export const EXIT_OK: 0;
export const EXIT_ERROR: 1;
export const EXIT_SWEEP_COMPLETE: 3;
export const EXIT_NONE_AVAILABLE: 4;

export class TrailError extends Error {}

export type Strategy = "round-robin" | "weighted" | "random";

export interface ClaimOpts {
  agent?: string;
  root?: string;
  strategy?: Strategy;
  autoSweep?: boolean;
  /** Poll interval (ms) while folders are only leased elsewhere. */
  pollMs?: number;
}

export interface Folder {
  status: "ok";
  task: string;
  sweep: number;
  path: string;
  score: number;
  lease_expires_at: number;
  remaining: number;
}

export interface CompleteResult {
  status: "done" | "skipped";
  task: string;
  sweep: number;
  path: string;
  remaining: number;
  sweep_complete: boolean;
}

export interface StatusReport {
  task: string;
  sweep: number;
  sweep_status: "active" | "complete" | "none";
  total: number;
  done: number;
  leased: number;
  pending: number;
  skipped: number;
  percent: number;
}

export interface InitResult {
  folders: number;
  excluded: number;
  wrote_example_config: boolean;
}

export interface SweepInfo {
  task: string;
  sweep: number;
  sweep_status: "active" | "complete" | "none";
  total: number;
  started_at: number | null;
  completed_at: number | null;
}

export interface CmdOpts {
  agent?: string;
  root?: string;
  reason?: string;
  rescan?: boolean;
}

export function init(root?: string): InitResult;
/** Returns the folder, or null when the sweep is complete. */
export function claim(task: string, opts?: ClaimOpts): Folder | null;
export function folders(task: string, opts?: ClaimOpts): Generator<Folder>;
export function done(task: string, path: string, opts?: CmdOpts): CompleteResult;
export function skip(task: string, path: string, opts?: CmdOpts): CompleteResult;
export function status(task: string, opts?: CmdOpts): StatusReport;
export function newSweep(task: string, opts?: CmdOpts): SweepInfo;
