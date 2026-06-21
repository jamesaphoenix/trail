//! Native Node.js bindings for trail (napi-rs).
//!
//! Unlike the thin `wrappers/typescript` (which shells out to the `trail`
//! binary), this runs the coverage scheduler in-process. Every method returns
//! the same object shapes as the CLI's JSON.
//!
//! ```js
//! const { Trail } = require("trail-node");
//! const t = new Trail("/repo");
//! t.init();
//! let r;
//! while ((r = t.next("refine", "a1")).status === "ok") {
//!   // ... investigate r.path ...
//!   t.done("refine", r.path, "a1", 3);
//! }
//! ```

use napi::Result;
use napi_derive::napi;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use trail_core::{Config, Store, WorkStatus};

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn e<E: std::fmt::Display>(x: E) -> napi::Error {
    napi::Error::from_reason(x.to_string())
}

fn val<T: serde::Serialize>(v: &T) -> Result<serde_json::Value> {
    serde_json::to_value(v).map_err(e)
}

/// A coverage-scheduler handle bound to a project root.
#[napi]
pub struct Trail {
    store: Store,
    cfg: Config,
    root: PathBuf,
}

#[napi]
impl Trail {
    /// Open (or create) the trail state for `root`. Loads `<root>/.trail.toml`.
    #[napi(constructor)]
    pub fn new(root: String) -> Result<Self> {
        let root = PathBuf::from(root);
        let cfg = Config::load(&root).map_err(e)?;
        let store = Store::open(&root.join(".trail").join("state.db")).map_err(e)?;
        Ok(Trail { store, cfg, root })
    }

    /// Scan the tree and register the folder snapshot.
    #[napi]
    pub fn init(&mut self) -> Result<serde_json::Value> {
        let out = trail_core::scan(&self.root, &self.cfg).map_err(e)?;
        self.store
            .replace_folders(&out.folders, now_unix())
            .map_err(e)?;
        val(&serde_json::json!({ "folders": out.folders.len(), "excluded": out.excluded }))
    }

    /// Claim the next folder. Returns an object with `status`.
    #[napi]
    pub fn next(
        &mut self,
        task: String,
        agent: Option<String>,
        auto_sweep: Option<bool>,
    ) -> Result<serde_json::Value> {
        let r = self
            .store
            .next(
                &task,
                &self.cfg,
                agent.as_deref(),
                None,
                auto_sweep.unwrap_or(false),
                now_unix(),
            )
            .map_err(e)?;
        val(&r)
    }

    #[napi]
    pub fn done(
        &mut self,
        task: String,
        path: String,
        agent: Option<String>,
        found: Option<i64>,
        reason: Option<String>,
    ) -> Result<serde_json::Value> {
        let r = self
            .store
            .complete(
                &task,
                &path,
                agent.as_deref(),
                WorkStatus::Done,
                reason.as_deref(),
                found,
                now_unix(),
            )
            .map_err(e)?;
        val(&r)
    }

    #[napi]
    pub fn skip(
        &mut self,
        task: String,
        path: String,
        agent: Option<String>,
        found: Option<i64>,
        reason: Option<String>,
    ) -> Result<serde_json::Value> {
        let r = self
            .store
            .complete(
                &task,
                &path,
                agent.as_deref(),
                WorkStatus::Skipped,
                reason.as_deref(),
                found,
                now_unix(),
            )
            .map_err(e)?;
        val(&r)
    }

    #[napi]
    pub fn status(&self, task: String) -> Result<serde_json::Value> {
        val(&self.store.status(&task).map_err(e)?)
    }

    #[napi]
    pub fn list(&self, task: String, state: Option<String>) -> Result<serde_json::Value> {
        let filter = state.as_deref().and_then(WorkStatus::from_db);
        val(&self.store.list(&task, filter).map_err(e)?)
    }

    #[napi(js_name = "sweepNew")]
    pub fn sweep_new(&mut self, task: String) -> Result<serde_json::Value> {
        val(&self
            .store
            .open_new_sweep(&task, &self.cfg, now_unix())
            .map_err(e)?)
    }

    #[napi]
    pub fn reset(&mut self, task: String, all: Option<bool>) -> Result<serde_json::Value> {
        val(&self.store.reset(&task, all.unwrap_or(false)).map_err(e)?)
    }

    #[napi]
    pub fn gc(&mut self, vacuum: Option<bool>) -> Result<serde_json::Value> {
        val(&self
            .store
            .gc(now_unix(), vacuum.unwrap_or(false))
            .map_err(e)?)
    }
}
