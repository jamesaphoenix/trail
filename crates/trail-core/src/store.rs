//! SQLite-backed state: the folder snapshot, per-task sweeps, the one-pass
//! coverage board, and the append-only visit history that gives a task-name its
//! memory. Claiming is atomic under a write transaction so parallel agents
//! never get the same folder, and expired leases are reclaimed automatically.

use crate::config::Config;
use crate::error::{Error, Result};
use crate::model::*;
use crate::scheduler;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::path::Path;
use std::time::Duration;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS folders (
  path       TEXT PRIMARY KEY,
  file_count INTEGER NOT NULL,
  size_bytes INTEGER NOT NULL,
  churn      INTEGER NOT NULL DEFAULT 0,
  scanned_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS tasks (
  name          TEXT PRIMARY KEY,
  strategy      TEXT NOT NULL,
  seed          INTEGER NOT NULL,
  current_sweep INTEGER NOT NULL DEFAULT 0,
  created_at    INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS sweeps (
  task_name    TEXT NOT NULL,
  sweep        INTEGER NOT NULL,
  status       TEXT NOT NULL,
  started_at   INTEGER NOT NULL,
  completed_at INTEGER,
  -- Maintained counters so 'remaining', the sweep-complete check, and status()
  -- are O(1) reads instead of O(pending) COUNT(*) scans on every claim/complete.
  total        INTEGER NOT NULL DEFAULT 0,
  pending      INTEGER NOT NULL DEFAULT 0,
  leased       INTEGER NOT NULL DEFAULT 0,
  done         INTEGER NOT NULL DEFAULT 0,
  skipped      INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (task_name, sweep)
);
CREATE TABLE IF NOT EXISTS work_items (
  task_name        TEXT NOT NULL,
  sweep            INTEGER NOT NULL,
  path             TEXT NOT NULL,
  status           TEXT NOT NULL,
  score            REAL NOT NULL,
  lease_owner      TEXT,
  lease_expires_at INTEGER,
  visited_at       INTEGER,
  PRIMARY KEY (task_name, sweep, path)
);
CREATE INDEX IF NOT EXISTS idx_claim ON work_items (task_name, sweep, status, score);
-- Lets complete() seek a folder by (task, path) instead of scanning every
-- work_item for the task across all accumulated sweeps.
CREATE INDEX IF NOT EXISTS idx_work_path ON work_items (task_name, path);
CREATE TABLE IF NOT EXISTS visits (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  task_name  TEXT NOT NULL,
  path       TEXT NOT NULL,
  sweep      INTEGER NOT NULL,
  visited_at INTEGER NOT NULL,
  agent_id   TEXT,
  status     TEXT NOT NULL,
  reason     TEXT,
  outcome    INTEGER
);
CREATE INDEX IF NOT EXISTS idx_visits_recency ON visits (task_name, path, visited_at);
"#;

/// Recompute every sweep's counters from the work_items board. Used by the
/// schema-v2 backfill. (`gc()` recomputes only the sweeps it actually touched.)
const RECOMPUTE_SWEEP_COUNTERS: &str = "
UPDATE sweeps SET
  total   = (SELECT count(*) FROM work_items w
              WHERE w.task_name = sweeps.task_name AND w.sweep = sweeps.sweep),
  pending = (SELECT count(*) FROM work_items w
              WHERE w.task_name = sweeps.task_name AND w.sweep = sweeps.sweep AND w.status = 'pending'),
  leased  = (SELECT count(*) FROM work_items w
              WHERE w.task_name = sweeps.task_name AND w.sweep = sweeps.sweep AND w.status = 'leased'),
  done    = (SELECT count(*) FROM work_items w
              WHERE w.task_name = sweeps.task_name AND w.sweep = sweeps.sweep AND w.status = 'done'),
  skipped = (SELECT count(*) FROM work_items w
              WHERE w.task_name = sweeps.task_name AND w.sweep = sweeps.sweep AND w.status = 'skipped');
";

/// One row fed into scoring at sweep open:
/// (path, file_count, size_bytes, churn, last_visit, last_outcome).
type FolderRow = (String, i64, i64, i64, Option<i64>, Option<i64>);

/// Internal outcome of a single atomic claim attempt.
enum ClaimOutcome {
    Leased {
        path: String,
        score: f64,
        expires: i64,
        remaining: i64,
    },
    /// Nothing pending, but other agents hold leases.
    NoneAvailable { leased: i64 },
    /// Nothing pending and nothing leased: the sweep is drained.
    Empty,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open (creating if needed) the state DB at `db_path`, e.g. `.trail/state.db`.
    pub fn open(db_path: &Path) -> Result<Store> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(db_path)?;
        Self::from_conn(conn)
    }

    /// In-memory store for tests.
    pub fn open_in_memory() -> Result<Store> {
        Self::from_conn(Connection::open_in_memory()?)
    }

    fn from_conn(mut conn: Connection) -> Result<Store> {
        conn.busy_timeout(Duration::from_secs(10))?;
        // Cache enough prepared statements to cover the hot path (claim/complete
        // issue ~10 distinct statements) without thrashing the default 16-slot
        // cache, so SQL is compiled once per process, not per call.
        conn.set_prepared_statement_cache_capacity(32);
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;",
        )?;
        conn.execute_batch(SCHEMA)?;
        // Idempotent migrations for DBs created before a column existed.
        // CREATE TABLE IF NOT EXISTS will not add a column to an existing table,
        // so add each explicitly and ignore the duplicate-column error.
        let _ = conn.execute("ALTER TABLE visits ADD COLUMN reason TEXT", []);
        let _ = conn.execute("ALTER TABLE visits ADD COLUMN outcome INTEGER", []);

        // Schema v2: add the sweep counters and backfill them once from the
        // work_items board. Run the whole upgrade under one write transaction and
        // re-check user_version inside it, so concurrent openers single-flight it
        // (exactly one performs ALTER + backfill; the rest see v>=2 and skip).
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version < 2 {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let v: i64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            if v < 2 {
                for col in ["total", "pending", "leased", "done", "skipped"] {
                    // Tolerate the duplicate-column error on a partially-migrated
                    // DB; a failed statement does not abort the transaction.
                    let _ = tx.execute(
                        &format!("ALTER TABLE sweeps ADD COLUMN {col} INTEGER NOT NULL DEFAULT 0"),
                        [],
                    );
                }
                tx.execute_batch(RECOMPUTE_SWEEP_COUNTERS)?;
                tx.pragma_update(None, "user_version", 2)?;
            }
            tx.commit()?;
        }
        Ok(Store { conn })
    }

    // --- folder snapshot ---------------------------------------------------

    /// Replace the entire folder snapshot (called by `init` / rescan).
    pub fn replace_folders(&mut self, folders: &[FolderStat], now: i64) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM folders", [])?;
        {
            let mut ins = tx.prepare(
                "INSERT INTO folders (path, file_count, size_bytes, churn, scanned_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for f in folders {
                ins.execute(params![f.path, f.file_count, f.size_bytes, f.churn, now])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn folder_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT count(*) FROM folders", [], |r| r.get(0))?)
    }

    // --- tasks / sweeps ----------------------------------------------------

    fn ensure_task(&self, task: &str, strategy: Strategy, seed: u64, now: i64) -> Result<()> {
        // Reject an empty/whitespace task name (almost always a shell-quoting
        // slip or an unset env var in an agent loop) rather than silently
        // opening a phantom task + sweep.
        if task.trim().is_empty() {
            return Err(Error::Config("--task must not be empty".to_string()));
        }
        self.conn.execute(
            "INSERT OR IGNORE INTO tasks (name, strategy, seed, current_sweep, created_at)
             VALUES (?1, ?2, ?3, 0, ?4)",
            params![task, strategy.as_str(), seed as i64, now],
        )?;
        Ok(())
    }

    fn set_task_strategy(&self, task: &str, strategy: Strategy) -> Result<()> {
        self.conn.execute(
            "UPDATE tasks SET strategy = ?2 WHERE name = ?1",
            params![task, strategy.as_str()],
        )?;
        Ok(())
    }

    /// (sweep_no, status) of the most recent sweep for a task, if any.
    fn latest_sweep(&self, task: &str) -> Result<Option<(i64, String)>> {
        self.conn
            .query_row(
                "SELECT sweep, status FROM sweeps WHERE task_name = ?1 ORDER BY sweep DESC LIMIT 1",
                params![task],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(Into::into)
    }

    /// Open the next sweep for a task: freeze a priority score per folder from
    /// prior-sweep staleness + static weight, and seed the coverage board.
    /// Returns (sweep_no, total_folders).
    ///
    /// The task row (incl. `current_sweep`) is read INSIDE the write
    /// transaction so two concurrent openers serialize: the loser sees the
    /// winner's committed sweep and is refused with [`Error::SweepActive`]
    /// rather than colliding on the sweeps primary key.
    fn open_sweep(&mut self, task: &str, cfg: &Config, now: i64) -> Result<(i64, i64)> {
        let signal = cfg.strategy.static_signal;

        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

        let (strategy, seed, current) = tx
            .query_row(
                "SELECT strategy, seed, current_sweep FROM tasks WHERE name = ?1",
                params![task],
                |r| {
                    let s: String = r.get(0)?;
                    let seed: i64 = r.get(1)?;
                    let cur: i64 = r.get(2)?;
                    Ok((Strategy::from_db(&s), seed as u64, cur))
                },
            )
            .optional()?
            .ok_or_else(|| Error::Other(format!("task {task:?} does not exist")))?;

        // Refuse if a sweep is already active (the winner of a concurrent race
        // already opened one). Prevents two overlapping active sweeps.
        let active: Option<i64> = tx
            .query_row(
                "SELECT sweep FROM sweeps WHERE task_name = ?1 AND status = 'active'
                 ORDER BY sweep DESC LIMIT 1",
                params![task],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(n) = active {
            return Err(Error::SweepActive(format!(
                "sweep {n} for task {task:?} is still active; \
                 finish or reset it before opening a new sweep"
            )));
        }
        let new_sweep = current + 1;

        // Folder + its most recent visit time and most recent reported outcome
        // under this task (both None if never visited / never reported).
        let rows: Vec<FolderRow> = {
            let mut stmt = tx.prepare(
                "SELECT f.path, f.file_count, f.size_bytes, f.churn,
                        (SELECT max(v.visited_at) FROM visits v
                          WHERE v.task_name = ?1 AND v.path = f.path),
                        (SELECT v.outcome FROM visits v
                          WHERE v.task_name = ?1 AND v.path = f.path
                            AND v.outcome IS NOT NULL
                          ORDER BY v.visited_at DESC, v.id DESC LIMIT 1)
                 FROM folders f",
            )?;
            let it = stmt.query_map(params![task], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })?;
            it.collect::<rusqlite::Result<Vec<_>>>()?
        };

        let max_signal = rows
            .iter()
            .map(|(_, fc, sz, ch, _, _)| pick_signal(signal, *fc, *sz, *ch))
            .max()
            .unwrap_or(0);
        let max_outcome = rows
            .iter()
            .filter_map(|(_, _, _, _, _, oc)| *oc)
            .max()
            .unwrap_or(0);

        let total = rows.len() as i64;
        tx.execute(
            "INSERT INTO sweeps (task_name, sweep, status, started_at, total, pending)
             VALUES (?1, ?2, 'active', ?3, ?4, ?4)",
            params![task, new_sweep, now, total],
        )?;
        {
            let mut ins = tx.prepare(
                "INSERT INTO work_items (task_name, sweep, path, status, score)
                 VALUES (?1, ?2, ?3, 'pending', ?4)",
            )?;
            for (path, fc, sz, ch, last_visit, last_outcome) in &rows {
                let weight = scheduler::normalize(pick_signal(signal, *fc, *sz, *ch), max_signal);
                let recency = match last_visit {
                    Some(lv) => scheduler::recency_priority(now - lv, cfg.strategy.half_life_secs),
                    None => 1.0, // never visited under this task = maximally stale
                };
                let outcome = scheduler::normalize(last_outcome.unwrap_or(0), max_outcome);
                let score = scheduler::score(
                    strategy,
                    &scheduler::ScoreInputs {
                        recency,
                        weight,
                        outcome,
                        alpha: cfg.strategy.alpha,
                        outcome_weight: cfg.strategy.outcome_weight,
                        seed,
                        sweep: new_sweep,
                        path,
                    },
                );
                ins.execute(params![task, new_sweep, path, score])?;
            }
        }
        tx.execute(
            "UPDATE tasks SET current_sweep = ?2 WHERE name = ?1",
            params![task, new_sweep],
        )?;
        tx.commit()?;
        Ok((new_sweep, total))
    }

    fn mark_sweep_complete(&self, task: &str, sweep: i64, now: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE sweeps SET status = 'complete', completed_at = ?3
             WHERE task_name = ?1 AND sweep = ?2 AND status = 'active'",
            params![task, sweep, now],
        )?;
        Ok(())
    }

    fn sweep_complete_result(&self, task: &str, sweep: i64) -> Result<NextResult> {
        let (covered, total): (i64, i64) = self.conn.query_row(
            "SELECT done, total FROM sweeps WHERE task_name = ?1 AND sweep = ?2",
            params![task, sweep],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        // A zero-folder sweep is almost always a missing `init` or an exclude
        // that matched everything, not real completion. Flag it.
        let note = if total == 0 {
            Some("no folders registered for this sweep - did you run `trail init`?".to_string())
        } else {
            None
        };
        Ok(NextResult::SweepComplete {
            task: task.to_string(),
            sweep,
            covered,
            total,
            note,
        })
    }

    // --- the main entry point: claim the next folder -----------------------

    /// Lease and return the next folder for `task`. Bootstraps the first sweep
    /// automatically; rolls into a fresh sweep only when `auto_sweep` is set.
    pub fn next(
        &mut self,
        task: &str,
        cfg: &Config,
        agent: Option<&str>,
        strategy_override: Option<Strategy>,
        auto_sweep: bool,
        now: i64,
    ) -> Result<NextResult> {
        let strategy = strategy_override.unwrap_or(cfg.strategy.default);
        self.ensure_task(task, strategy, cfg.strategy.seed, now)?;
        if let Some(s) = strategy_override {
            self.set_task_strategy(task, s)?;
        }

        // Open at most one fresh sweep per call (besides bootstrap) to keep the
        // loop bounded even if a sweep turns out empty. `guard` is a hard
        // backstop against any pathological transition loop.
        let mut opened = 0u8;
        let mut guard = 0u8;
        loop {
            guard += 1;
            if guard > 16 {
                return Err(Error::Other(format!(
                    "next: too many sweep transitions for task {task:?}"
                )));
            }

            let latest = self.latest_sweep(task)?;
            let want_open = match &latest {
                Some((_, st)) if st == "active" => false,
                Some((n, _complete)) => {
                    if opened == 0 && !auto_sweep {
                        // Finished, and not asked to auto-advance: report it.
                        return self.sweep_complete_result(task, *n);
                    }
                    true
                }
                None => true, // first sweep ever: bootstrap
            };

            let sweep = if want_open {
                // On losing a concurrent open race (SweepActive), loop back and
                // claim from the winner's now-active sweep.
                match self.open_sweep(task, cfg, now) {
                    Ok((s, total)) => {
                        opened += 1;
                        if total == 0 {
                            self.mark_sweep_complete(task, s, now)?;
                            return self.sweep_complete_result(task, s);
                        }
                        s
                    }
                    Err(Error::SweepActive(_)) => continue,
                    Err(e) => return Err(e),
                }
            } else {
                latest.unwrap().0
            };

            match self.claim(task, sweep, agent, cfg.lease.ttl_secs, now)? {
                ClaimOutcome::Leased {
                    path,
                    score,
                    expires,
                    remaining,
                } => {
                    return Ok(NextResult::Ok {
                        task: task.to_string(),
                        sweep,
                        path,
                        score,
                        lease_expires_at: expires,
                        remaining,
                    })
                }
                ClaimOutcome::NoneAvailable { leased } => {
                    return Ok(NextResult::NoneAvailable {
                        task: task.to_string(),
                        sweep,
                        leased_outstanding: leased,
                    })
                }
                ClaimOutcome::Empty => {
                    self.mark_sweep_complete(task, sweep, now)?;
                    if auto_sweep && opened < 2 {
                        continue; // roll into a new sweep and try again
                    }
                    return self.sweep_complete_result(task, sweep);
                }
            }
        }
    }

    /// Atomic claim: reclaim expired leases, then lease the top-priority pending
    /// row in a single `UPDATE ... RETURNING` under an immediate write lock.
    fn claim(
        &mut self,
        task: &str,
        sweep: i64,
        agent: Option<&str>,
        ttl: i64,
        now: i64,
    ) -> Result<ClaimOutcome> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

        let reclaimed = tx
            .prepare_cached(
                "UPDATE work_items
                    SET status = 'pending', lease_owner = NULL, lease_expires_at = NULL
                  WHERE task_name = ?1 AND sweep = ?2 AND status = 'leased'
                    AND lease_expires_at < ?3",
            )?
            .execute(params![task, sweep, now])?;
        if reclaimed > 0 {
            tx.prepare_cached(
                "UPDATE sweeps SET leased = leased - ?3, pending = pending + ?3
                  WHERE task_name = ?1 AND sweep = ?2",
            )?
            .execute(params![task, sweep, reclaimed as i64])?;
        }

        // Saturate rather than overflow-panic (debug) or wrap negative (release,
        // which would make the lease look already expired). `path ASC` is omitted
        // from the ORDER BY: score already carries a path-seeded tie-break so the
        // order is total, and dropping it lets the claim use idx_claim without a
        // temp b-tree sort.
        let expires = now.checked_add(ttl).unwrap_or(i64::MAX);
        let claimed: Option<(String, f64, i64)> = tx
            .prepare_cached(
                "UPDATE work_items
                    SET status = 'leased', lease_owner = ?3, lease_expires_at = ?4
                  WHERE rowid = (
                      SELECT rowid FROM work_items
                       WHERE task_name = ?1 AND sweep = ?2 AND status = 'pending'
                       ORDER BY score DESC
                       LIMIT 1)
                RETURNING path, score, lease_expires_at",
            )?
            .query_row(params![task, sweep, agent, expires], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .optional()?;

        let outcome = match claimed {
            Some((path, score, expires)) => {
                // pending -> leased: O(1) counter update, then read remaining.
                tx.prepare_cached(
                    "UPDATE sweeps SET pending = pending - 1, leased = leased + 1
                      WHERE task_name = ?1 AND sweep = ?2",
                )?
                .execute(params![task, sweep])?;
                let remaining: i64 = tx
                    .prepare_cached(
                        "SELECT pending FROM sweeps WHERE task_name = ?1 AND sweep = ?2",
                    )?
                    .query_row(params![task, sweep], |r| r.get(0))?;
                ClaimOutcome::Leased {
                    path,
                    score,
                    expires,
                    remaining,
                }
            }
            None => {
                let leased: i64 = tx
                    .prepare_cached(
                        "SELECT leased FROM sweeps WHERE task_name = ?1 AND sweep = ?2",
                    )?
                    .query_row(params![task, sweep], |r| r.get(0))?;
                if leased > 0 {
                    ClaimOutcome::NoneAvailable { leased }
                } else {
                    ClaimOutcome::Empty
                }
            }
        };
        tx.commit()?;
        Ok(outcome)
    }

    // --- completion --------------------------------------------------------

    /// Mark a folder done/skipped and append it to the task's history.
    ///
    /// The path is normalized to the stored form, then resolved to the sweep
    /// where it is actually outstanding (preferring the sweep this `agent`
    /// leased it in), rather than blindly the newest sweep. Re-completing an
    /// already-terminal folder is idempotent. A path that is not a work item in
    /// any sweep is an error, so a typo or a missed `init` is not silently
    /// reported as success.
    #[allow(clippy::too_many_arguments)]
    pub fn complete(
        &mut self,
        task: &str,
        path: &str,
        agent: Option<&str>,
        status: WorkStatus,
        reason: Option<&str>,
        found: Option<i64>,
        now: i64,
    ) -> Result<CompleteResult> {
        // complete() only moves a folder to a terminal state. Reject Pending/
        // Leased so the counter move (SET <from>=<from>-1, <to>=<to>+1) can never
        // collapse to a self-cancelling double-assignment (from == to).
        if !matches!(status, WorkStatus::Done | WorkStatus::Skipped) {
            return Err(Error::Other(format!(
                "complete() requires a terminal status (done/skipped), got {status:?}"
            )));
        }
        let path = crate::walk::normalize_rel(path);
        let st = status.as_str();

        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

        // Pick the most relevant row for this path: an outstanding one first
        // (preferring this agent's lease), else the newest terminal one so a
        // re-completion is idempotent. None at all = a genuine miss.
        let target: Option<(i64, String)> = tx
            .query_row(
                "SELECT sweep, status FROM work_items
                  WHERE task_name = ?1 AND path = ?2
                  ORDER BY (status IN ('pending', 'leased')) DESC,
                           coalesce(lease_owner = ?3, 0) DESC,
                           sweep DESC
                  LIMIT 1",
                params![task, path, agent],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;

        let (sweep, cur_status) = match target {
            Some(t) => t,
            None => {
                return Err(Error::NotInSweep(format!(
                    "path {path:?} is not a work item in any sweep of task {task:?} \
                     (typo, wrong --task, or excluded by config?)"
                )));
            }
        };

        let already_terminal = cur_status == "done" || cur_status == "skipped";
        let result_status = if already_terminal {
            cur_status
        } else {
            tx.prepare_cached(
                "UPDATE work_items
                    SET status = ?4, visited_at = ?5, lease_owner = NULL, lease_expires_at = NULL
                  WHERE task_name = ?1 AND sweep = ?2 AND path = ?3
                    AND status NOT IN ('done', 'skipped')",
            )?
            .execute(params![task, sweep, path, st, now])?;
            tx.prepare_cached(
                "INSERT INTO visits
                   (task_name, path, sweep, visited_at, agent_id, status, reason, outcome)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?
            .execute(params![task, path, sweep, now, agent, st, reason, found])?;
            // Move the counter from the row's previous state (pending/leased) to
            // its new terminal state (done/skipped). Both names come from a fixed
            // internal set, never user input.
            let from = cur_status.as_str(); // "pending" or "leased"
            tx.prepare_cached(&format!(
                "UPDATE sweeps SET {from} = {from} - 1, {st} = {st} + 1
                  WHERE task_name = ?1 AND sweep = ?2"
            ))?
            .execute(params![task, sweep])?;
            st.to_string()
        };

        let remaining: i64 = tx
            .prepare_cached(
                "SELECT pending + leased FROM sweeps WHERE task_name = ?1 AND sweep = ?2",
            )?
            .query_row(params![task, sweep], |r| r.get(0))?;
        // In debug/test builds, catch any future counter-maintenance bug by
        // cross-checking the maintained counter against the live board. Free in
        // release builds.
        #[cfg(debug_assertions)]
        {
            let live: i64 = tx.query_row(
                "SELECT count(*) FROM work_items
                  WHERE task_name = ?1 AND sweep = ?2 AND status IN ('pending', 'leased')",
                params![task, sweep],
                |r| r.get(0),
            )?;
            debug_assert_eq!(remaining, live, "sweep {sweep}: remaining counter drift");
        }
        let sweep_complete = remaining == 0;
        if sweep_complete {
            tx.execute(
                "UPDATE sweeps SET status = 'complete', completed_at = ?3
                  WHERE task_name = ?1 AND sweep = ?2 AND status = 'active'",
                params![task, sweep, now],
            )?;
        }
        tx.commit()?;

        Ok(CompleteResult {
            status: result_status,
            task: task.to_string(),
            sweep,
            path,
            remaining,
            sweep_complete,
        })
    }

    // --- reporting / control ----------------------------------------------

    pub fn status(&self, task: &str) -> Result<StatusReport> {
        match self.latest_sweep(task)? {
            None => Ok(StatusReport {
                task: task.to_string(),
                sweep: 0,
                sweep_status: "none".to_string(),
                total: 0,
                done: 0,
                leased: 0,
                pending: 0,
                skipped: 0,
                percent: 0.0,
            }),
            Some((sweep, st)) => {
                // O(1): read the maintained counters off the sweep row.
                let (total, done, leased, pending, skipped): (i64, i64, i64, i64, i64) =
                    self.conn.query_row(
                        "SELECT total, done, leased, pending, skipped
                           FROM sweeps WHERE task_name = ?1 AND sweep = ?2",
                        params![task, sweep],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                    )?;
                let percent = if total > 0 {
                    100.0 * done as f64 / total as f64
                } else {
                    0.0
                };
                Ok(StatusReport {
                    task: task.to_string(),
                    sweep,
                    sweep_status: st,
                    total,
                    done,
                    leased,
                    pending,
                    skipped,
                    percent,
                })
            }
        }
    }

    pub fn list(&self, task: &str, filter: Option<WorkStatus>) -> Result<Vec<ListRow>> {
        let sweep = match self.latest_sweep(task)? {
            Some((s, _)) => s,
            None => return Ok(Vec::new()),
        };
        let map = |r: &rusqlite::Row| {
            Ok(ListRow {
                path: r.get(0)?,
                status: r.get(1)?,
                score: r.get(2)?,
                lease_owner: r.get(3)?,
                lease_expires_at: r.get(4)?,
                visited_at: r.get(5)?,
            })
        };
        let base = "SELECT path, status, score, lease_owner, lease_expires_at, visited_at
                    FROM work_items WHERE task_name = ?1 AND sweep = ?2";
        let rows = match filter {
            Some(f) => {
                let sql = format!("{base} AND status = ?3 ORDER BY score DESC, path ASC");
                let mut stmt = self.conn.prepare(&sql)?;
                let rows = stmt
                    .query_map(params![task, sweep, f.as_str()], map)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                rows
            }
            None => {
                let sql = format!("{base} ORDER BY score DESC, path ASC");
                let mut stmt = self.conn.prepare(&sql)?;
                let rows = stmt
                    .query_map(params![task, sweep], map)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                rows
            }
        };
        Ok(rows)
    }

    pub fn sweep_info(&self, task: &str) -> Result<SweepInfo> {
        match self.latest_sweep(task)? {
            None => Ok(SweepInfo {
                task: task.to_string(),
                sweep: 0,
                sweep_status: "none".to_string(),
                total: 0,
                started_at: None,
                completed_at: None,
            }),
            Some((sweep, st)) => {
                // One O(1) read off the sweep row (total is the maintained counter).
                let (total, started, completed): (i64, Option<i64>, Option<i64>) =
                    self.conn.query_row(
                        "SELECT total, started_at, completed_at
                           FROM sweeps WHERE task_name = ?1 AND sweep = ?2",
                        params![task, sweep],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )?;
                Ok(SweepInfo {
                    task: task.to_string(),
                    sweep,
                    sweep_status: st,
                    total,
                    started_at: started,
                    completed_at: completed,
                })
            }
        }
    }

    /// Explicitly open a fresh sweep (`trail sweep new`). `open_sweep` refuses
    /// transactionally while a sweep is still active, so an in-progress sweep is
    /// never orphaned even under concurrent callers.
    pub fn open_new_sweep(&mut self, task: &str, cfg: &Config, now: i64) -> Result<SweepInfo> {
        self.ensure_task(task, cfg.strategy.default, cfg.strategy.seed, now)?;
        self.open_sweep(task, cfg, now)?;
        self.sweep_info(task)
    }

    pub fn reset(&mut self, task: &str, all: bool) -> Result<ResetResult> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let cleared_sweeps: i64 = tx.query_row(
            "SELECT count(*) FROM sweeps WHERE task_name = ?1",
            params![task],
            |r| r.get(0),
        )?;
        tx.execute("DELETE FROM work_items WHERE task_name = ?1", params![task])?;
        tx.execute("DELETE FROM sweeps WHERE task_name = ?1", params![task])?;
        tx.execute(
            "UPDATE tasks SET current_sweep = 0 WHERE name = ?1",
            params![task],
        )?;
        if all {
            tx.execute("DELETE FROM visits WHERE task_name = ?1", params![task])?;
        }
        tx.commit()?;
        Ok(ResetResult {
            task: task.to_string(),
            cleared_sweeps,
            cleared_history: all,
        })
    }

    /// Reclaim expired leases across all sweeps. Reclaiming is the always-safe
    /// primary action; `VACUUM` (only when `vacuum` is set) can return
    /// SQLITE_BUSY under concurrent connections, so a failure there is logged
    /// rather than failing the whole command.
    pub fn gc(&mut self, now: i64, vacuum: bool) -> Result<GcResult> {
        let reclaimed = {
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            // Per-sweep counts of about-to-be-reclaimed leases, so we can move
            // the counters with a scoped delta instead of recomputing the whole
            // (never-pruned) board.
            let deltas: Vec<(String, i64, i64)> = {
                let mut stmt = tx.prepare(
                    "SELECT task_name, sweep, count(*) FROM work_items
                      WHERE status = 'leased' AND lease_expires_at < ?1
                      GROUP BY task_name, sweep",
                )?;
                let rows = stmt
                    .query_map(params![now], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                rows
            };
            let n = tx.execute(
                "UPDATE work_items
                    SET status = 'pending', lease_owner = NULL, lease_expires_at = NULL
                  WHERE status = 'leased' AND lease_expires_at < ?1",
                params![now],
            )?;
            for (t, sweep, k) in &deltas {
                tx.execute(
                    "UPDATE sweeps SET leased = leased - ?3, pending = pending + ?3
                      WHERE task_name = ?1 AND sweep = ?2",
                    params![t, sweep, k],
                )?;
            }
            tx.commit()?;
            n as i64
        };
        if vacuum {
            if let Err(e) = self.conn.execute_batch("VACUUM;") {
                log::warn!("trail gc: VACUUM skipped ({e})");
            }
        }
        Ok(GcResult {
            reclaimed_leases: reclaimed,
        })
    }

    /// Recompute every sweep's counters from the work_items board, repairing any
    /// drift. A safety/repair path (the counters are maintained correctly on the
    /// hot path); exposed via `trail gc --reconcile`.
    pub fn reconcile(&mut self) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(RECOMPUTE_SWEEP_COUNTERS)?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
impl Store {
    /// Latest recorded visit reason for a (task, path), for tests.
    fn last_visit_reason(&self, task: &str, path: &str) -> Option<String> {
        self.conn
            .query_row(
                "SELECT reason FROM visits WHERE task_name = ?1 AND path = ?2
                 ORDER BY id DESC LIMIT 1",
                params![task, path],
                |r| r.get(0),
            )
            .optional()
            .unwrap()
            .flatten()
    }

    /// Number of recorded visits for a task, for tests.
    fn visit_count(&self, task: &str) -> i64 {
        self.conn
            .query_row(
                "SELECT count(*) FROM visits WHERE task_name = ?1",
                params![task],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// (strategy, seed, current_sweep) for a task, if it exists, for tests.
    fn task_row(&self, task: &str) -> Result<Option<(Strategy, u64, i64)>> {
        self.conn
            .query_row(
                "SELECT strategy, seed, current_sweep FROM tasks WHERE name = ?1",
                params![task],
                |r| {
                    let s: String = r.get(0)?;
                    let seed: i64 = r.get(1)?;
                    let cur: i64 = r.get(2)?;
                    Ok((Strategy::from_db(&s), seed as u64, cur))
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// Number of active sweeps for a task (should always be 0 or 1), for tests.
    fn active_sweep_count(&self, task: &str) -> i64 {
        self.conn
            .query_row(
                "SELECT count(*) FROM sweeps WHERE task_name = ?1 AND status = 'active'",
                params![task],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// Assert the maintained sweep counters match the actual work_items board.
    fn assert_counters_consistent(&self, task: &str) {
        let sweeps: Vec<(i64, i64, i64, i64, i64, i64)> = self
            .conn
            .prepare(
                "SELECT sweep, total, pending, leased, done, skipped
                   FROM sweeps WHERE task_name = ?1",
            )
            .unwrap()
            .query_map(params![task], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })
            .unwrap()
            .map(|x| x.unwrap())
            .collect();
        for (sweep, total, pending, leased, done, skipped) in sweeps {
            let actual = |st: &str| -> i64 {
                self.conn
                    .query_row(
                        "SELECT count(*) FROM work_items
                          WHERE task_name = ?1 AND sweep = ?2 AND status = ?3",
                        params![task, sweep, st],
                        |r| r.get(0),
                    )
                    .unwrap()
            };
            let act_total: i64 = self
                .conn
                .query_row(
                    "SELECT count(*) FROM work_items WHERE task_name = ?1 AND sweep = ?2",
                    params![task, sweep],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(total, act_total, "sweep {sweep}: total counter drift");
            assert_eq!(pending, actual("pending"), "sweep {sweep}: pending drift");
            assert_eq!(leased, actual("leased"), "sweep {sweep}: leased drift");
            assert_eq!(done, actual("done"), "sweep {sweep}: done drift");
            assert_eq!(skipped, actual("skipped"), "sweep {sweep}: skipped drift");
        }
    }
}

fn pick_signal(signal: StaticSignal, file_count: i64, size_bytes: i64, churn: i64) -> i64 {
    match signal {
        StaticSignal::FileCount => file_count,
        StaticSignal::SizeBytes => size_bytes,
        StaticSignal::Churn => churn,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    const NOW: i64 = 1_000_000;

    fn folders(n: usize) -> Vec<FolderStat> {
        (0..n)
            .map(|i| FolderStat {
                path: format!("dir{i:03}"),
                file_count: (i as i64 % 5) + 1,
                size_bytes: 100,
                churn: 0,
            })
            .collect()
    }

    fn seeded(n: usize) -> (Store, Config) {
        let mut s = Store::open_in_memory().unwrap();
        s.replace_folders(&folders(n), NOW).unwrap();
        (s, Config::default())
    }

    #[test]
    fn full_sweep_lifecycle() {
        let (mut s, cfg) = seeded(3);
        let mut claimed = Vec::new();
        loop {
            match s.next("t", &cfg, Some("a1"), None, false, NOW).unwrap() {
                NextResult::Ok { path, .. } => {
                    claimed.push(path.clone());
                    let r = s
                        .complete("t", &path, Some("a1"), WorkStatus::Done, None, None, NOW)
                        .unwrap();
                    assert_eq!(r.status, "done");
                }
                NextResult::SweepComplete { covered, .. } => {
                    assert_eq!(covered, 3);
                    break;
                }
                NextResult::NoneAvailable { .. } => panic!("unexpected with one agent"),
            }
        }
        claimed.sort();
        assert_eq!(claimed, vec!["dir000", "dir001", "dir002"]);
        let st = s.status("t").unwrap();
        assert_eq!(st.done, 3);
        assert!((st.percent - 100.0).abs() < 1e-9);
    }

    #[test]
    fn empty_repo_completes_immediately() {
        let mut s = Store::open_in_memory().unwrap();
        let cfg = Config::default();
        match s.next("t", &cfg, None, None, false, NOW).unwrap() {
            NextResult::SweepComplete { covered, .. } => assert_eq!(covered, 0),
            other => panic!("expected sweep-complete, got {other:?}"),
        }
    }

    #[test]
    fn recency_orders_the_next_sweep() {
        // Equal weights so recency dominates. Visit the folders in sweep 1 at
        // increasing times, then open sweep 2: the folder visited *earliest*
        // (most stale) must be handed out first.
        let mut s = Store::open_in_memory().unwrap();
        let mut cfg = Config::default();
        cfg.strategy.default = Strategy::RoundRobin;
        let fs: Vec<_> = ["a", "b", "c"]
            .iter()
            .map(|p| FolderStat {
                path: p.to_string(),
                file_count: 1,
                size_bytes: 1,
                churn: 0,
            })
            .collect();
        s.replace_folders(&fs, 0).unwrap();

        // Sweep 1: visit a at t=100, b at t=200, c at t=300.
        for (p, t) in [("a", 100), ("b", 200), ("c", 300)] {
            // claim something then complete the specific path at time t
            let _ = s.next("t", &cfg, Some("x"), None, false, t).unwrap();
            s.complete("t", p, Some("x"), WorkStatus::Done, None, None, t)
                .unwrap();
        }

        // Sweep 2 at t=1000: stalest first => a (visited at 100) before c (300).
        let first = match s.next("t", &cfg, Some("x"), None, true, 1000).unwrap() {
            NextResult::Ok { path, sweep, .. } => {
                assert_eq!(sweep, 2);
                path
            }
            other => panic!("expected ok, got {other:?}"),
        };
        assert_eq!(first, "a", "most-stale folder should lead the new sweep");
    }

    #[test]
    fn no_auto_sweep_reports_complete_then_explicit_new_sweep() {
        let (mut s, cfg) = seeded(2);
        // Drain sweep 1.
        loop {
            match s.next("t", &cfg, Some("a"), None, false, NOW).unwrap() {
                NextResult::Ok { path, .. } => {
                    s.complete("t", &path, Some("a"), WorkStatus::Done, None, None, NOW)
                        .unwrap();
                }
                NextResult::SweepComplete { sweep, .. } => {
                    assert_eq!(sweep, 1);
                    break;
                }
                _ => unreachable!(),
            }
        }
        // Without auto-sweep, next keeps reporting complete.
        assert!(matches!(
            s.next("t", &cfg, Some("a"), None, false, NOW).unwrap(),
            NextResult::SweepComplete { .. }
        ));
        // Explicit new sweep opens sweep 2.
        let info = s.open_new_sweep("t", &cfg, NOW).unwrap();
        assert_eq!(info.sweep, 2);
        assert_eq!(info.total, 2);
    }

    #[test]
    fn expired_lease_is_reclaimed() {
        let (mut s, mut cfg) = seeded(1);
        cfg.lease.ttl_secs = 10;
        // Claim the only folder at t=NOW (lease expires at NOW+10).
        let path = match s.next("t", &cfg, Some("a1"), None, false, NOW).unwrap() {
            NextResult::Ok { path, .. } => path,
            other => panic!("expected ok, got {other:?}"),
        };
        // Before expiry, another agent finds nothing pending but a live lease.
        match s.next("t", &cfg, Some("a2"), None, false, NOW + 5).unwrap() {
            NextResult::NoneAvailable {
                leased_outstanding, ..
            } => {
                assert_eq!(leased_outstanding, 1)
            }
            other => panic!("expected none-available, got {other:?}"),
        }
        // After expiry, the folder is reclaimed and handed to a2.
        match s
            .next("t", &cfg, Some("a2"), None, false, NOW + 100)
            .unwrap()
        {
            NextResult::Ok { path: p2, .. } => assert_eq!(p2, path),
            other => panic!("expected reclaim, got {other:?}"),
        }
    }

    #[test]
    fn parallel_agents_cover_every_folder_exactly_once() {
        // File-backed DB shared across threads, large TTL so nothing expires.
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("state.db");
        let mut setup = Store::open(&db).unwrap();
        let n = 60usize;
        setup.replace_folders(&folders(n), NOW).unwrap();
        let mut cfg = Config::default();
        cfg.lease.ttl_secs = 3600;
        // Bootstrap sweep 1 so all threads claim from the same sweep.
        setup.open_new_sweep("t", &cfg, NOW).unwrap();
        drop(setup);

        let claimed = Arc::new(Mutex::new(Vec::<String>::new()));
        let mut handles = Vec::new();
        for tid in 0..8 {
            let db = db.clone();
            let cfg = cfg.clone();
            let claimed = Arc::clone(&claimed);
            handles.push(std::thread::spawn(move || {
                let mut store = Store::open(&db).unwrap();
                let agent = format!("a{tid}");
                loop {
                    match store
                        .next("t", &cfg, Some(&agent), None, false, NOW)
                        .unwrap()
                    {
                        NextResult::Ok { path, .. } => {
                            claimed.lock().unwrap().push(path.clone());
                            store
                                .complete(
                                    "t",
                                    &path,
                                    Some(&agent),
                                    WorkStatus::Done,
                                    None,
                                    None,
                                    NOW,
                                )
                                .unwrap();
                        }
                        NextResult::SweepComplete { .. } => break,
                        NextResult::NoneAvailable { .. } => {
                            std::thread::yield_now();
                        }
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        let mut got = Arc::try_unwrap(claimed).unwrap().into_inner().unwrap();
        got.sort();
        let dupes = {
            let mut d = got.clone();
            d.dedup();
            got.len() - d.len()
        };
        assert_eq!(dupes, 0, "no folder claimed twice");
        assert_eq!(got.len(), n, "every folder covered exactly once");
    }

    #[test]
    fn complete_unknown_path_errors_and_recompletion_is_idempotent() {
        let (mut s, cfg) = seeded(2);
        let path = match s.next("t", &cfg, Some("a1"), None, false, NOW).unwrap() {
            NextResult::Ok { path, .. } => path,
            other => panic!("expected ok, got {other:?}"),
        };
        s.complete("t", &path, Some("a1"), WorkStatus::Done, None, None, NOW)
            .unwrap();

        // A bogus path is a real miss, not silent success.
        assert!(matches!(
            s.complete(
                "t",
                "does/not/exist",
                Some("a1"),
                WorkStatus::Done,
                None,
                None,
                NOW
            ),
            Err(Error::NotInSweep(_))
        ));

        // Re-completing the same real path stays Ok and reports done.
        let again = s
            .complete("t", &path, Some("a1"), WorkStatus::Done, None, None, NOW)
            .unwrap();
        assert_eq!(again.status, "done");

        // Neither the miss nor the idempotent redo advanced coverage past 1.
        let st = s.status("t").unwrap();
        assert_eq!(st.done, 1);
        assert_eq!(st.skipped, 0);
    }

    #[test]
    fn complete_normalizes_slashes_and_dot_prefix() {
        let mut s = Store::open_in_memory().unwrap();
        let cfg = Config::default();
        s.replace_folders(
            &[FolderStat {
                path: "src/api".into(),
                file_count: 1,
                size_bytes: 1,
                churn: 0,
            }],
            NOW,
        )
        .unwrap();
        s.next("t", &cfg, Some("a1"), None, false, NOW).unwrap();
        // A ./ prefix and backslashes still match the stored "src/api".
        let r = s
            .complete(
                "t",
                ".\\src\\api",
                Some("a1"),
                WorkStatus::Done,
                None,
                None,
                NOW,
            )
            .unwrap();
        assert_eq!(r.path, "src/api");
        assert_eq!(r.status, "done");
        assert!(r.sweep_complete);
    }

    #[test]
    fn sweep_new_refused_while_active() {
        let (mut s, cfg) = seeded(2);
        // Bootstrap sweep 1 and leave it active.
        s.next("t", &cfg, Some("a1"), None, false, NOW).unwrap();
        assert!(matches!(
            s.open_new_sweep("t", &cfg, NOW),
            Err(Error::SweepActive(_))
        ));
        // Drain it, then a new sweep is allowed.
        for p in ["dir000", "dir001"] {
            let _ = s.complete("t", p, Some("a1"), WorkStatus::Done, None, None, NOW);
        }
        let info = s.open_new_sweep("t", &cfg, NOW).unwrap();
        assert_eq!(info.sweep, 2);
    }

    #[test]
    fn huge_ttl_does_not_overflow_or_self_reclaim() {
        let (mut s, mut cfg) = seeded(1);
        cfg.lease.ttl_secs = i64::MAX;
        match s.next("t", &cfg, Some("a1"), None, false, NOW).unwrap() {
            NextResult::Ok {
                lease_expires_at, ..
            } => assert_eq!(lease_expires_at, i64::MAX),
            other => panic!("expected ok, got {other:?}"),
        }
        // The lease is not spuriously reclaimed by overflow wrap.
        assert!(matches!(
            s.next("t", &cfg, Some("a2"), None, false, NOW + 5).unwrap(),
            NextResult::NoneAvailable { .. }
        ));
    }

    #[test]
    fn skip_reason_is_stored() {
        let (mut s, cfg) = seeded(1);
        let path = match s.next("t", &cfg, Some("a1"), None, false, NOW).unwrap() {
            NextResult::Ok { path, .. } => path,
            other => panic!("expected ok, got {other:?}"),
        };
        let r = s
            .complete(
                "t",
                &path,
                Some("a1"),
                WorkStatus::Skipped,
                Some("flaky build"),
                None,
                NOW,
            )
            .unwrap();
        assert_eq!(r.status, "skipped");
        assert_eq!(
            s.last_visit_reason("t", &path).as_deref(),
            Some("flaky build")
        );
    }

    #[test]
    fn empty_sweep_reports_total_zero_with_note() {
        let mut s = Store::open_in_memory().unwrap();
        let cfg = Config::default();
        match s.next("t", &cfg, None, None, false, NOW).unwrap() {
            NextResult::SweepComplete {
                covered,
                total,
                note,
                ..
            } => {
                assert_eq!(covered, 0);
                assert_eq!(total, 0);
                assert!(note.is_some(), "empty sweep should carry a note");
            }
            other => panic!("expected sweep-complete, got {other:?}"),
        }
    }

    /// Drain a single sweep with one agent, completing as we go.
    fn drain(s: &mut Store, task: &str, status: WorkStatus, now: i64) -> Vec<String> {
        let mut got = Vec::new();
        let cfg = Config::default();
        while let NextResult::Ok { path, .. } =
            s.next(task, &cfg, Some("a"), None, false, now).unwrap()
        {
            got.push(path.clone());
            s.complete(task, &path, Some("a"), status, None, None, now)
                .unwrap();
        }
        got
    }

    #[test]
    fn skip_heavy_sweep_completes_with_zero_percent() {
        let (mut s, _cfg) = seeded(2);
        let covered = drain(&mut s, "t", WorkStatus::Skipped, NOW);
        assert_eq!(covered.len(), 2);
        let st = s.status("t").unwrap();
        assert_eq!(st.skipped, 2);
        assert_eq!(st.done, 0);
        assert_eq!(st.percent, 0.0);
        assert_eq!(st.sweep_status, "complete");
    }

    #[test]
    fn reset_retains_history_unless_all() {
        let (mut s, _cfg) = seeded(2);
        drain(&mut s, "t", WorkStatus::Done, 100);
        assert!(s.visit_count("t") > 0);
        // Plain reset clears the board but keeps the task's memory.
        let r = s.reset("t", false).unwrap();
        assert!(!r.cleared_history);
        assert!(s.visit_count("t") > 0, "history retained");
        // reset --all wipes the memory too.
        let r2 = s.reset("t", true).unwrap();
        assert!(r2.cleared_history);
        assert_eq!(s.visit_count("t"), 0);
    }

    #[test]
    fn multi_task_isolation_on_one_db() {
        let (mut s, _cfg) = seeded(3);
        drain(&mut s, "A", WorkStatus::Done, NOW); // task A fully covered
                                                   // Task B is independent: its own pending sweep over the same folders.
        let remaining = match s
            .next("B", &Config::default(), Some("b"), None, false, NOW)
            .unwrap()
        {
            NextResult::Ok { remaining, .. } => remaining,
            other => panic!("expected ok, got {other:?}"),
        };
        assert_eq!(remaining, 2);
        assert_eq!(s.status("A").unwrap().done, 3);
        let b = s.status("B").unwrap();
        assert_eq!(b.done, 0);
        assert_eq!(b.leased, 1);
    }

    #[test]
    fn gc_reclaims_expired_lease_across_sweeps() {
        let (mut s, mut cfg) = seeded(1);
        cfg.lease.ttl_secs = 10;
        s.next("t", &cfg, Some("a1"), None, false, NOW).unwrap();
        // Before expiry: nothing to reclaim.
        assert_eq!(s.gc(NOW + 5, false).unwrap().reclaimed_leases, 0);
        // After expiry: reclaimed, and the folder is claimable again.
        assert_eq!(s.gc(NOW + 100, false).unwrap().reclaimed_leases, 1);
        assert!(matches!(
            s.next("t", &cfg, Some("a2"), None, false, NOW + 101)
                .unwrap(),
            NextResult::Ok { .. }
        ));
    }

    #[test]
    fn list_orders_by_score_and_filters_by_state() {
        let (mut s, cfg) = seeded(3);
        s.next("t", &cfg, Some("a1"), None, false, NOW).unwrap(); // leases one
        let all = s.list("t", None).unwrap();
        assert_eq!(all.len(), 3);
        for w in all.windows(2) {
            assert!(w[0].score >= w[1].score, "list not sorted by score desc");
        }
        assert_eq!(s.list("t", Some(WorkStatus::Leased)).unwrap().len(), 1);
        assert_eq!(s.list("t", Some(WorkStatus::Pending)).unwrap().len(), 2);
        assert!(s.list("unknown", None).unwrap().is_empty());
    }

    #[test]
    fn status_edge_cases() {
        let (mut s, cfg) = seeded(4);
        // No sweep yet for this task name.
        let none = s.status("never").unwrap();
        assert_eq!(none.sweep_status, "none");
        assert_eq!(none.total, 0);
        assert_eq!(none.percent, 0.0);
        // Partial: 1 of 4 done -> 25%.
        s.next("t", &cfg, Some("a"), None, false, NOW).unwrap();
        s.complete("t", "dir000", Some("a"), WorkStatus::Done, None, None, NOW)
            .unwrap();
        let st = s.status("t").unwrap();
        assert_eq!(st.done, 1);
        assert!((st.percent - 25.0).abs() < 1e-9);
        assert_eq!(st.sweep_status, "active");
    }

    #[test]
    fn tree_drift_adds_new_folder_on_rescan() {
        let mut s = Store::open_in_memory().unwrap();
        let cfg = Config::default();
        s.replace_folders(&folders(2), NOW).unwrap();
        drain(&mut s, "t", WorkStatus::Done, NOW);
        // The tree grows; a rescan + new sweep includes the newcomer.
        s.replace_folders(&folders(3), NOW + 1).unwrap();
        let info = s.open_new_sweep("t", &cfg, NOW + 1).unwrap();
        assert_eq!(info.total, 3);
        let paths: Vec<_> = s
            .list("t", None)
            .unwrap()
            .into_iter()
            .map(|r| r.path)
            .collect();
        assert!(paths.contains(&"dir002".to_string()));
    }

    #[test]
    fn random_strategy_is_reproducible_across_stores() {
        let order = || {
            let mut s = Store::open_in_memory().unwrap();
            let mut cfg = Config::default();
            cfg.strategy.default = Strategy::Random;
            cfg.strategy.seed = 12_345;
            s.replace_folders(&folders(6), NOW).unwrap();
            let mut got = Vec::new();
            while let NextResult::Ok { path, .. } =
                s.next("t", &cfg, Some("a"), None, false, NOW).unwrap()
            {
                got.push(path.clone());
                s.complete("t", &path, Some("a"), WorkStatus::Done, None, None, NOW)
                    .unwrap();
            }
            got
        };
        assert_eq!(order(), order(), "same seed => same drain order");
    }

    #[test]
    fn strategy_override_persists_on_task() {
        let (mut s, cfg) = seeded(2);
        s.next("t", &cfg, Some("a"), Some(Strategy::Random), false, NOW)
            .unwrap();
        let (strat, _seed, _cur) = s.task_row("t").unwrap().unwrap();
        assert_eq!(strat, Strategy::Random);
    }

    #[test]
    fn concurrent_auto_sweep_open_is_race_free() {
        // After a drained sweep, many agents calling next(auto_sweep=true) race
        // to open the next sweep. Exactly one opens it; the rest claim from it.
        // No agent errors (no PK collision), and two overlapping active sweeps
        // never exist. (Regression test for the open_sweep TOCTOU.)
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("state.db");
        let n = 20usize;
        let mut cfg = Config::default();
        cfg.lease.ttl_secs = 3600;
        {
            let mut setup = Store::open(&db).unwrap();
            setup.replace_folders(&folders(n), NOW).unwrap();
            setup.open_new_sweep("t", &cfg, NOW).unwrap();
            drain(&mut setup, "t", WorkStatus::Done, NOW); // fully drain sweep 1
            assert_eq!(setup.status("t").unwrap().sweep_status, "complete");
        }

        let threads = 8usize;
        let barrier = Arc::new(std::sync::Barrier::new(threads));
        let results = Arc::new(Mutex::new(Vec::<Result<String>>::new()));
        let mut handles = Vec::new();
        for tid in 0..threads {
            let db = db.clone();
            let cfg = cfg.clone();
            let barrier = Arc::clone(&barrier);
            let results = Arc::clone(&results);
            handles.push(std::thread::spawn(move || {
                let mut store = Store::open(&db).unwrap();
                let agent = format!("a{tid}");
                barrier.wait(); // maximize the open-race overlap
                let r = store
                    .next("t", &cfg, Some(&agent), None, true, NOW)
                    .map(|res| match res {
                        NextResult::Ok { path, sweep, .. } => {
                            assert_eq!(sweep, 2, "claims must come from the one new sweep");
                            path
                        }
                        _ => String::new(), // NoneAvailable / SweepComplete: no claim
                    });
                results.lock().unwrap().push(r);
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        let results = Arc::try_unwrap(results).unwrap().into_inner().unwrap();
        for r in &results {
            assert!(
                r.is_ok(),
                "agent errored on open race: {:?}",
                r.as_ref().err()
            );
        }
        let store = Store::open(&db).unwrap();
        assert_eq!(
            store.active_sweep_count("t"),
            1,
            "exactly one active sweep, never two overlapping"
        );
        let mut claimed: Vec<String> = results
            .into_iter()
            .filter_map(|r| r.ok())
            .filter(|p| !p.is_empty())
            .collect();
        let before = claimed.len();
        claimed.sort();
        claimed.dedup();
        assert_eq!(claimed.len(), before, "a folder was leased to two agents");
    }

    #[test]
    fn outcome_feedback_resurfaces_hot_folders() {
        // With outcome_weight > 0 and equal recency/weight, the folder that
        // reported the most findings leads the next sweep.
        let mut s = Store::open_in_memory().unwrap();
        let mut cfg = Config::default();
        cfg.strategy.outcome_weight = 0.6;
        let fs: Vec<_> = ["a", "b", "c"]
            .iter()
            .map(|p| FolderStat {
                path: p.to_string(),
                file_count: 1,
                size_bytes: 1,
                churn: 0,
            })
            .collect();
        s.replace_folders(&fs, 0).unwrap();

        // Sweep 1 at t=100: a is hot (10 findings), b and c are clean.
        s.next("t", &cfg, Some("x"), None, false, 100).unwrap();
        s.complete("t", "a", Some("x"), WorkStatus::Done, None, Some(10), 100)
            .unwrap();
        s.complete("t", "b", Some("x"), WorkStatus::Done, None, Some(0), 100)
            .unwrap();
        s.complete("t", "c", Some("x"), WorkStatus::Done, None, Some(0), 100)
            .unwrap();

        // Sweep 2 at the same instant (recency equal): the hot folder leads.
        let first = match s.next("t", &cfg, Some("x"), None, true, 100).unwrap() {
            NextResult::Ok { path, sweep, .. } => {
                assert_eq!(sweep, 2);
                path
            }
            other => panic!("expected ok, got {other:?}"),
        };
        assert_eq!(
            first, "a",
            "the folder with the most findings should resurface first"
        );
    }

    #[test]
    fn complete_lookup_seeks_via_path_index() {
        // Guard against the per-completion full table scan: the target-resolution
        // query must seek on idx_work_path, not scan every row for the task.
        let s = Store::open_in_memory().unwrap();
        let plan: Vec<String> = s
            .conn
            .prepare(
                "EXPLAIN QUERY PLAN
                 SELECT sweep, status FROM work_items
                   WHERE task_name = ?1 AND path = ?2
                   ORDER BY (status IN ('pending', 'leased')) DESC,
                            coalesce(lease_owner = ?3, 0) DESC, sweep DESC
                   LIMIT 1",
            )
            .unwrap()
            .query_map(params!["t", "p", "a"], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        let detail = plan.join(" | ");
        assert!(
            detail.contains("idx_work_path"),
            "complete() query did not use idx_work_path: {detail}"
        );
    }

    #[test]
    fn sweep_counters_stay_consistent_through_transitions() {
        let (mut s, mut cfg) = seeded(6);
        cfg.lease.ttl_secs = 10;
        // done + skipped transitions.
        let p1 = claim_path(&mut s, &cfg, "a", NOW);
        s.assert_counters_consistent("t");
        s.complete("t", &p1, Some("a"), WorkStatus::Done, None, Some(3), NOW)
            .unwrap();
        let p2 = claim_path(&mut s, &cfg, "a", NOW);
        s.complete("t", &p2, Some("a"), WorkStatus::Skipped, None, None, NOW)
            .unwrap();
        s.assert_counters_consistent("t");
        // Leave a lease, let it expire, reclaim it via next().
        claim_path(&mut s, &cfg, "a", NOW);
        claim_path(&mut s, &cfg, "b", NOW + 100); // reclaims the expired one
        s.assert_counters_consistent("t");
        // gc reclaim path.
        claim_path(&mut s, &cfg, "c", NOW + 200);
        s.gc(NOW + 1000, false).unwrap();
        s.assert_counters_consistent("t");
        // Drain the rest, counters still consistent.
        drain(&mut s, "t", WorkStatus::Done, NOW + 2000);
        s.assert_counters_consistent("t");
        let st = s.status("t").unwrap();
        assert_eq!(st.pending, 0);
        assert_eq!(st.leased, 0);
        assert_eq!(st.done + st.skipped, 6);
    }

    fn claim_path(s: &mut Store, cfg: &Config, agent: &str, now: i64) -> String {
        match s.next("t", cfg, Some(agent), None, false, now).unwrap() {
            NextResult::Ok { path, .. } => path,
            other => panic!("expected ok, got {other:?}"),
        }
    }

    #[test]
    fn complete_rejects_non_terminal_status() {
        let (mut s, cfg) = seeded(2);
        let p = claim_path(&mut s, &cfg, "a", NOW);
        // A non-terminal status would make the counter move self-cancel; reject it.
        assert!(matches!(
            s.complete("t", &p, Some("a"), WorkStatus::Leased, None, None, NOW),
            Err(Error::Other(_))
        ));
        assert!(s
            .complete("t", &p, Some("a"), WorkStatus::Pending, None, None, NOW)
            .is_err());
        s.assert_counters_consistent("t"); // rejected calls left no drift
    }

    #[test]
    fn empty_or_whitespace_task_is_rejected() {
        let mut s = Store::open_in_memory().unwrap();
        let cfg = Config::default();
        s.replace_folders(&folders(1), NOW).unwrap();
        assert!(matches!(
            s.next("", &cfg, Some("a"), None, false, NOW),
            Err(Error::Config(_))
        ));
        assert!(matches!(
            s.next("   ", &cfg, Some("a"), None, false, NOW),
            Err(Error::Config(_))
        ));
        let n: i64 = s
            .conn
            .query_row("SELECT count(*) FROM tasks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "no phantom task created");
    }

    #[test]
    fn reconcile_repairs_injected_counter_drift() {
        let (mut s, cfg) = seeded(4);
        claim_path(&mut s, &cfg, "a", NOW);
        s.conn
            .execute("UPDATE sweeps SET pending = 0, leased = 99", [])
            .unwrap();
        s.reconcile().unwrap();
        s.assert_counters_consistent("t");
    }

    #[test]
    fn v2_migration_backfills_counters_on_legacy_db() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("legacy.db");
        {
            // A v1-shaped DB: sweeps WITHOUT counter columns + a mixed board.
            let c = Connection::open(&db).unwrap();
            c.execute_batch(
                "CREATE TABLE sweeps (task_name TEXT, sweep INTEGER, status TEXT,
                     started_at INTEGER, completed_at INTEGER, PRIMARY KEY(task_name, sweep));
                 CREATE TABLE work_items (task_name TEXT, sweep INTEGER, path TEXT, status TEXT,
                     score REAL, lease_owner TEXT, lease_expires_at INTEGER, visited_at INTEGER,
                     PRIMARY KEY(task_name, sweep, path));
                 INSERT INTO sweeps VALUES ('t', 1, 'active', 0, NULL);
                 INSERT INTO work_items VALUES
                   ('t',1,'a','pending',1.0,NULL,NULL,NULL),
                   ('t',1,'b','leased',1.0,'x',999,NULL),
                   ('t',1,'c','done',1.0,NULL,NULL,0);",
            )
            .unwrap();
            c.pragma_update(None, "user_version", 0).unwrap();
        }
        let s = Store::open(&db).unwrap(); // runs the v2 backfill
        s.assert_counters_consistent("t");
        let st = s.status("t").unwrap();
        assert_eq!((st.total, st.pending, st.leased, st.done), (3, 1, 1, 1));
        // Re-opening is a no-op (user_version is now 2).
        Store::open(&db).unwrap().assert_counters_consistent("t");
    }

    #[test]
    fn claim_query_uses_index_without_temp_btree() {
        // Perf-regression guard: the claim subselect must seek idx_claim and
        // NOT sort via a temp b-tree (the dropped `path ASC` win). Re-adding a
        // secondary ORDER BY term or losing the index would fail this.
        let s = Store::open_in_memory().unwrap();
        let plan: Vec<String> = s
            .conn
            .prepare(
                "EXPLAIN QUERY PLAN
                 SELECT rowid FROM work_items
                   WHERE task_name = ?1 AND sweep = ?2 AND status = 'pending'
                   ORDER BY score DESC LIMIT 1",
            )
            .unwrap()
            .query_map(params!["t", 1i64], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        let detail = plan.join(" | ");
        assert!(
            detail.contains("idx_claim"),
            "claim must use idx_claim: {detail}"
        );
        assert!(
            !detail.to_uppercase().contains("TEMP B-TREE"),
            "claim must not sort via a temp b-tree: {detail}"
        );
    }

    #[test]
    fn concurrent_claim_complete_gc_keeps_counters_consistent() {
        // Stress claim + complete + gc concurrently and assert the invariants
        // hold: every folder covered once, counters consistent, correct totals,
        // no deadlock between the IMMEDIATE transactions.
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("state.db");
        let n = 80usize;
        let mut cfg = Config::default();
        cfg.lease.ttl_secs = 3600;
        {
            let mut setup = Store::open(&db).unwrap();
            setup.replace_folders(&folders(n), NOW).unwrap();
            setup.open_new_sweep("t", &cfg, NOW).unwrap();
        }

        let claimed = Arc::new(Mutex::new(Vec::<String>::new()));
        let mut handles = Vec::new();
        for tid in 0..6 {
            let db = db.clone();
            let cfg = cfg.clone();
            let claimed = Arc::clone(&claimed);
            handles.push(std::thread::spawn(move || {
                let mut store = Store::open(&db).unwrap();
                let agent = format!("a{tid}");
                loop {
                    match store
                        .next("t", &cfg, Some(&agent), None, false, NOW)
                        .unwrap()
                    {
                        NextResult::Ok { path, .. } => {
                            claimed.lock().unwrap().push(path.clone());
                            store
                                .complete(
                                    "t",
                                    &path,
                                    Some(&agent),
                                    WorkStatus::Done,
                                    None,
                                    None,
                                    NOW,
                                )
                                .unwrap();
                        }
                        NextResult::SweepComplete { .. } => break,
                        NextResult::NoneAvailable { .. } => std::thread::yield_now(),
                    }
                }
            }));
        }
        // A gc thread hammering the maintenance path under contention.
        {
            let db = db.clone();
            handles.push(std::thread::spawn(move || {
                let mut store = Store::open(&db).unwrap();
                for _ in 0..50 {
                    let _ = store.gc(NOW, false);
                    std::thread::yield_now();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        let store = Store::open(&db).unwrap();
        store.assert_counters_consistent("t");
        let st = store.status("t").unwrap();
        assert_eq!(st.done, n as i64);
        assert_eq!(st.pending, 0);
        assert_eq!(st.leased, 0);
        let mut got = Arc::try_unwrap(claimed).unwrap().into_inner().unwrap();
        got.sort();
        let before = got.len();
        got.dedup();
        assert_eq!(got.len(), before, "a folder was claimed twice");
        assert_eq!(got.len(), n, "every folder covered exactly once");
    }

    proptest::proptest! {
        #[test]
        fn draining_a_sweep_covers_every_folder_exactly_once(n in 1usize..40) {
            let mut s = Store::open_in_memory().unwrap();
            let cfg = Config::default();
            s.replace_folders(&folders(n), NOW).unwrap();
            let mut seen = std::collections::BTreeSet::new();
            loop {
                match s.next("t", &cfg, Some("a"), None, false, NOW).unwrap() {
                    NextResult::Ok { path, .. } => {
                        proptest::prop_assert!(seen.insert(path.clone()), "folder claimed twice: {}", path);
                        s.complete("t", &path, Some("a"), WorkStatus::Done, None, None, NOW).unwrap();
                    }
                    NextResult::SweepComplete { covered, total, .. } => {
                        proptest::prop_assert_eq!(covered, n as i64);
                        proptest::prop_assert_eq!(total, n as i64);
                        break;
                    }
                    NextResult::NoneAvailable { .. } => proptest::prop_assert!(false, "single agent saw none-available"),
                }
            }
            proptest::prop_assert_eq!(seen.len(), n);
            s.assert_counters_consistent("t");
        }
    }
}
