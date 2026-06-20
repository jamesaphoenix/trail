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
CREATE TABLE IF NOT EXISTS visits (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  task_name  TEXT NOT NULL,
  path       TEXT NOT NULL,
  sweep      INTEGER NOT NULL,
  visited_at INTEGER NOT NULL,
  agent_id   TEXT,
  status     TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_visits_recency ON visits (task_name, path, visited_at);
"#;

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

    fn from_conn(conn: Connection) -> Result<Store> {
        conn.busy_timeout(Duration::from_secs(10))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;",
        )?;
        conn.execute_batch(SCHEMA)?;
        Ok(Store { conn })
    }

    // --- folder snapshot ---------------------------------------------------

    /// Replace the entire folder snapshot (called by `init` / rescan).
    pub fn replace_folders(&mut self, folders: &[FolderStat], now: i64) -> Result<()> {
        let tx = self.conn.transaction()?;
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

    /// (strategy, seed, current_sweep) for a task, if it exists.
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
    fn open_sweep(&mut self, task: &str, cfg: &Config, now: i64) -> Result<(i64, i64)> {
        let (strategy, seed, current) = self
            .task_row(task)?
            .ok_or_else(|| Error::Other(format!("task {task:?} does not exist")))?;
        let new_sweep = current + 1;
        let signal = cfg.strategy.static_signal;

        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

        // Folder + its most recent visit time under this task (None if never).
        let rows: Vec<(String, i64, i64, i64, Option<i64>)> = {
            let mut stmt = tx.prepare(
                "SELECT f.path, f.file_count, f.size_bytes, f.churn,
                        (SELECT max(v.visited_at) FROM visits v
                          WHERE v.task_name = ?1 AND v.path = f.path)
                 FROM folders f",
            )?;
            let it = stmt.query_map(params![task], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?;
            it.collect::<rusqlite::Result<Vec<_>>>()?
        };

        let max_signal = rows
            .iter()
            .map(|(_, fc, sz, ch, _)| pick_signal(signal, *fc, *sz, *ch))
            .max()
            .unwrap_or(0);

        tx.execute(
            "INSERT INTO sweeps (task_name, sweep, status, started_at)
             VALUES (?1, ?2, 'active', ?3)",
            params![task, new_sweep, now],
        )?;
        {
            let mut ins = tx.prepare(
                "INSERT INTO work_items (task_name, sweep, path, status, score)
                 VALUES (?1, ?2, ?3, 'pending', ?4)",
            )?;
            for (path, fc, sz, ch, last_visit) in &rows {
                let weight = scheduler::normalize(pick_signal(signal, *fc, *sz, *ch), max_signal);
                let recency = match last_visit {
                    Some(lv) => scheduler::recency_priority(now - lv, cfg.strategy.half_life_secs),
                    None => 1.0, // never visited under this task = maximally stale
                };
                let score = scheduler::score(
                    strategy,
                    recency,
                    weight,
                    cfg.strategy.alpha,
                    seed,
                    new_sweep,
                    path,
                );
                ins.execute(params![task, new_sweep, path, score])?;
            }
        }
        tx.execute(
            "UPDATE tasks SET current_sweep = ?2 WHERE name = ?1",
            params![task, new_sweep],
        )?;
        tx.commit()?;
        Ok((new_sweep, rows.len() as i64))
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
        let covered: i64 = self.conn.query_row(
            "SELECT count(*) FROM work_items WHERE task_name = ?1 AND sweep = ?2 AND status = 'done'",
            params![task, sweep],
            |r| r.get(0),
        )?;
        Ok(NextResult::SweepComplete {
            task: task.to_string(),
            sweep,
            covered,
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
        // loop bounded even if a sweep turns out empty.
        let mut opened = 0u8;
        loop {
            let sweep = match self.latest_sweep(task)? {
                Some((n, ref st)) if st == "active" => n,
                Some((n, _complete)) => {
                    if opened == 0 && !auto_sweep {
                        // Finished, and not asked to auto-advance: report it.
                        return self.sweep_complete_result(task, n);
                    }
                    let (s, total) = self.open_sweep(task, cfg, now)?;
                    opened += 1;
                    if total == 0 {
                        self.mark_sweep_complete(task, s, now)?;
                        return self.sweep_complete_result(task, s);
                    }
                    s
                }
                None => {
                    // First sweep ever: always bootstrap.
                    let (s, total) = self.open_sweep(task, cfg, now)?;
                    opened += 1;
                    if total == 0 {
                        self.mark_sweep_complete(task, s, now)?;
                        return self.sweep_complete_result(task, s);
                    }
                    s
                }
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

        tx.execute(
            "UPDATE work_items
                SET status = 'pending', lease_owner = NULL, lease_expires_at = NULL
              WHERE task_name = ?1 AND sweep = ?2 AND status = 'leased'
                AND lease_expires_at < ?3",
            params![task, sweep, now],
        )?;

        let claimed: Option<(String, f64, i64)> = tx
            .query_row(
                "UPDATE work_items
                    SET status = 'leased', lease_owner = ?3, lease_expires_at = ?4
                  WHERE rowid = (
                      SELECT rowid FROM work_items
                       WHERE task_name = ?1 AND sweep = ?2 AND status = 'pending'
                       ORDER BY score DESC, path ASC
                       LIMIT 1)
                RETURNING path, score, lease_expires_at",
                params![task, sweep, agent, now + ttl],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;

        let outcome = match claimed {
            Some((path, score, expires)) => {
                let remaining: i64 = tx.query_row(
                    "SELECT count(*) FROM work_items
                      WHERE task_name = ?1 AND sweep = ?2 AND status = 'pending'",
                    params![task, sweep],
                    |r| r.get(0),
                )?;
                ClaimOutcome::Leased {
                    path,
                    score,
                    expires,
                    remaining,
                }
            }
            None => {
                let leased: i64 = tx.query_row(
                    "SELECT count(*) FROM work_items
                      WHERE task_name = ?1 AND sweep = ?2 AND status = 'leased'",
                    params![task, sweep],
                    |r| r.get(0),
                )?;
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

    /// Mark a folder done/skipped in the latest sweep and append to history.
    pub fn complete(
        &mut self,
        task: &str,
        path: &str,
        agent: Option<&str>,
        status: WorkStatus,
        now: i64,
    ) -> Result<CompleteResult> {
        let (sweep, _st) = self
            .latest_sweep(task)?
            .ok_or_else(|| Error::Other(format!("no sweep open for task {task:?}")))?;
        let st = status.as_str();

        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let updated = tx.execute(
            "UPDATE work_items
                SET status = ?4, visited_at = ?5, lease_owner = NULL, lease_expires_at = NULL
              WHERE task_name = ?1 AND sweep = ?2 AND path = ?3
                AND status NOT IN ('done', 'skipped')",
            params![task, sweep, path, st, now],
        )?;
        if updated > 0 {
            tx.execute(
                "INSERT INTO visits (task_name, path, sweep, visited_at, agent_id, status)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![task, path, sweep, now, agent, st],
            )?;
        }
        let remaining: i64 = tx.query_row(
            "SELECT count(*) FROM work_items
              WHERE task_name = ?1 AND sweep = ?2 AND status IN ('pending', 'leased')",
            params![task, sweep],
            |r| r.get(0),
        )?;
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
            status: st.to_string(),
            task: task.to_string(),
            sweep,
            path: path.to_string(),
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
                let count = |status: &str| -> Result<i64> {
                    Ok(self.conn.query_row(
                        "SELECT count(*) FROM work_items
                          WHERE task_name = ?1 AND sweep = ?2 AND status = ?3",
                        params![task, sweep, status],
                        |r| r.get(0),
                    )?)
                };
                let done = count("done")?;
                let leased = count("leased")?;
                let pending = count("pending")?;
                let skipped = count("skipped")?;
                let total = done + leased + pending + skipped;
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
                let total: i64 = self.conn.query_row(
                    "SELECT count(*) FROM work_items WHERE task_name = ?1 AND sweep = ?2",
                    params![task, sweep],
                    |r| r.get(0),
                )?;
                let (started, completed): (Option<i64>, Option<i64>) = self.conn.query_row(
                    "SELECT started_at, completed_at FROM sweeps WHERE task_name = ?1 AND sweep = ?2",
                    params![task, sweep],
                    |r| Ok((r.get(0)?, r.get(1)?)),
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

    /// Explicitly open a fresh sweep (`trail sweep new`).
    pub fn open_new_sweep(&mut self, task: &str, cfg: &Config, now: i64) -> Result<SweepInfo> {
        self.ensure_task(task, cfg.strategy.default, cfg.strategy.seed, now)?;
        self.open_sweep(task, cfg, now)?;
        self.sweep_info(task)
    }

    pub fn reset(&mut self, task: &str, all: bool) -> Result<ResetResult> {
        let tx = self.conn.transaction()?;
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

    /// Reclaim expired leases across all sweeps and compact the DB.
    pub fn gc(&mut self, now: i64) -> Result<GcResult> {
        let reclaimed = {
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let n = tx.execute(
                "UPDATE work_items
                    SET status = 'pending', lease_owner = NULL, lease_expires_at = NULL
                  WHERE status = 'leased' AND lease_expires_at < ?1",
                params![now],
            )?;
            tx.commit()?;
            n as i64
        };
        self.conn.execute_batch("VACUUM;")?;
        Ok(GcResult {
            reclaimed_leases: reclaimed,
        })
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
                        .complete("t", &path, Some("a1"), WorkStatus::Done, NOW)
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
            s.complete("t", p, Some("x"), WorkStatus::Done, t).unwrap();
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
                    s.complete("t", &path, Some("a"), WorkStatus::Done, NOW)
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
                                .complete("t", &path, Some(&agent), WorkStatus::Done, NOW)
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
}
