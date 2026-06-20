//! `trail` CLI. Every command prints one JSON object/array to stdout. The
//! process exit code carries the loop-relevant outcome so shells and the thin
//! language wrappers can branch without parsing:
//!
//!   0  ok / success
//!   1  internal error (JSON error object on stderr)
//!   2  usage error (clap)
//!   3  sweep-complete  (the loop should stop, or open a new sweep)
//!   4  none-available  (folders are leased elsewhere; wait briefly and retry)

use clap::{Parser, Subcommand, ValueEnum};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};
use trail_core::config::EXAMPLE_CONFIG;
use trail_core::{Config, InitResult, NextResult, Store, Strategy, WorkStatus};

const EXIT_OK: u8 = 0;
const EXIT_ERROR: u8 = 1;
const EXIT_SWEEP_COMPLETE: u8 = 3;
const EXIT_NONE_AVAILABLE: u8 = 4;

#[derive(Parser)]
#[command(
    name = "trail",
    version,
    about = "Coverage scheduler with memory: hands agents the next folder to cover, per task-name."
)]
struct Cli {
    /// Project root. State lives at <root>/.trail/state.db, config at <root>/.trail.toml.
    #[arg(long, global = true)]
    root: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Scan the tree and register the folder snapshot (run once, or to rescan).
    Init,
    /// Claim and lease the next folder for a task.
    Next {
        #[arg(long)]
        task: String,
        /// Stable id for the calling agent (recorded as the lease owner).
        #[arg(long)]
        agent: Option<String>,
        /// Override the task's ordering strategy for future sweeps.
        #[arg(long, value_enum)]
        strategy: Option<StrategyArg>,
        /// Roll into a fresh sweep automatically when the current one completes.
        #[arg(long)]
        auto_sweep: bool,
    },
    /// Mark a folder done and append it to the task's visit history.
    Done {
        #[arg(long)]
        task: String,
        #[arg(long)]
        path: String,
        #[arg(long)]
        agent: Option<String>,
    },
    /// Mark a folder skipped (counts as covered, recorded in history).
    Skip {
        #[arg(long)]
        task: String,
        #[arg(long)]
        path: String,
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Coverage snapshot for the task's latest sweep.
    Status {
        #[arg(long)]
        task: String,
    },
    /// List work items in the latest sweep.
    List {
        #[arg(long)]
        task: String,
        #[arg(long, value_enum)]
        state: Option<StateArg>,
    },
    /// Sweep control.
    Sweep {
        #[command(subcommand)]
        cmd: SweepCmd,
    },
    /// Clear a task's sweeps (and its visit history with --all).
    Reset {
        #[arg(long)]
        task: String,
        #[arg(long)]
        all: bool,
    },
    /// Reclaim expired leases (and compact the database with --vacuum).
    Gc {
        /// Also run VACUUM to compact the DB file (best-effort; may be skipped
        /// if another connection holds the database).
        #[arg(long)]
        vacuum: bool,
    },
}

#[derive(Subcommand)]
enum SweepCmd {
    /// Open a fresh sweep (use --rescan to refresh the folder snapshot first).
    New {
        #[arg(long)]
        task: String,
        #[arg(long)]
        rescan: bool,
    },
    /// Show the latest sweep for a task.
    Show {
        #[arg(long)]
        task: String,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum StrategyArg {
    RoundRobin,
    Weighted,
    Random,
}

impl From<StrategyArg> for Strategy {
    fn from(a: StrategyArg) -> Strategy {
        match a {
            StrategyArg::RoundRobin => Strategy::RoundRobin,
            StrategyArg::Weighted => Strategy::Weighted,
            StrategyArg::Random => Strategy::Random,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum StateArg {
    Pending,
    Leased,
    Done,
    Skipped,
}

impl From<StateArg> for WorkStatus {
    fn from(a: StateArg) -> WorkStatus {
        match a {
            StateArg::Pending => WorkStatus::Pending,
            StateArg::Leased => WorkStatus::Leased,
            StateArg::Done => WorkStatus::Done,
            StateArg::Skipped => WorkStatus::Skipped,
        }
    }
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().filter_or("TRAIL_LOG", "warn")).init();
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            let err = serde_json::json!({ "status": "error", "error": e.to_string() });
            eprintln!("{}", serde_json::to_string(&err).unwrap_or_default());
            ExitCode::from(EXIT_ERROR)
        }
    }
}

fn run(cli: Cli) -> trail_core::Result<u8> {
    let root = cli
        .root
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let cfg = Config::load(&root)?;
    let db = root.join(".trail").join("state.db");
    let now = now_unix();

    match cli.cmd {
        Cmd::Init => {
            let wrote = maybe_write_example(&root)?;
            let out = trail_core::scan(&root, &cfg)?;
            let mut store = Store::open(&db)?;
            store.replace_folders(&out.folders, now)?;
            emit(&InitResult {
                folders: out.folders.len(),
                excluded: out.excluded,
                wrote_example_config: wrote,
            });
            Ok(EXIT_OK)
        }
        Cmd::Next {
            task,
            agent,
            strategy,
            auto_sweep,
        } => {
            let mut store = Store::open(&db)?;
            let res = store.next(
                &task,
                &cfg,
                agent.as_deref(),
                strategy.map(Into::into),
                auto_sweep,
                now,
            )?;
            emit(&res);
            Ok(match res {
                NextResult::Ok { .. } => EXIT_OK,
                NextResult::SweepComplete { .. } => EXIT_SWEEP_COMPLETE,
                NextResult::NoneAvailable { .. } => EXIT_NONE_AVAILABLE,
            })
        }
        Cmd::Done { task, path, agent } => {
            let mut store = Store::open(&db)?;
            let res =
                store.complete(&task, &path, agent.as_deref(), WorkStatus::Done, None, now)?;
            emit(&res);
            Ok(EXIT_OK)
        }
        Cmd::Skip {
            task,
            path,
            agent,
            reason,
        } => {
            let mut store = Store::open(&db)?;
            let res = store.complete(
                &task,
                &path,
                agent.as_deref(),
                WorkStatus::Skipped,
                reason.as_deref(),
                now,
            )?;
            emit(&res);
            Ok(EXIT_OK)
        }
        Cmd::Status { task } => {
            let store = Store::open(&db)?;
            emit(&store.status(&task)?);
            Ok(EXIT_OK)
        }
        Cmd::List { task, state } => {
            let store = Store::open(&db)?;
            emit(&store.list(&task, state.map(Into::into))?);
            Ok(EXIT_OK)
        }
        Cmd::Sweep { cmd } => match cmd {
            SweepCmd::New { task, rescan } => {
                let mut store = Store::open(&db)?;
                if rescan {
                    let out = trail_core::scan(&root, &cfg)?;
                    store.replace_folders(&out.folders, now)?;
                }
                emit(&store.open_new_sweep(&task, &cfg, now)?);
                Ok(EXIT_OK)
            }
            SweepCmd::Show { task } => {
                let store = Store::open(&db)?;
                emit(&store.sweep_info(&task)?);
                Ok(EXIT_OK)
            }
        },
        Cmd::Reset { task, all } => {
            let mut store = Store::open(&db)?;
            emit(&store.reset(&task, all)?);
            Ok(EXIT_OK)
        }
        Cmd::Gc { vacuum } => {
            let mut store = Store::open(&db)?;
            emit(&store.gc(now, vacuum)?);
            Ok(EXIT_OK)
        }
    }
}

/// Write `.trail.toml.example` next to the root if it is not already there.
fn maybe_write_example(root: &Path) -> trail_core::Result<bool> {
    let path = root.join(".trail.toml.example");
    if path.exists() {
        return Ok(false);
    }
    std::fs::write(&path, EXAMPLE_CONFIG)?;
    Ok(true)
}

fn emit<T: serde::Serialize>(v: &T) {
    use std::io::Write;
    let s = serde_json::to_string(v)
        .unwrap_or_else(|e| format!("{{\"status\":\"error\",\"error\":\"serialize: {e}\"}}"));
    // Ignore write errors (e.g. a closed pipe from `... | head`) rather than
    // panicking the way `println!` does on EPIPE.
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let _ = writeln!(lock, "{s}");
    let _ = lock.flush();
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
