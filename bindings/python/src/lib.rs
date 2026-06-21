//! Native Python bindings for trail (pyo3).
//!
//! Unlike the thin `wrappers/python` (which shells out to the `trail` binary),
//! this runs the coverage scheduler in-process. Every method returns the same
//! dict shapes as the CLI's JSON, so the two are drop-in compatible.
//!
//! ```python
//! import trail
//! t = trail.Trail("/repo")
//! t.init()
//! while (folder := t.next("refine", agent="a1")) and folder["status"] == "ok":
//!     # ... investigate folder["path"] ...
//!     t.done("refine", folder["path"], agent="a1", found=3)
//! ```

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use trail_core::{Config, Store, WorkStatus};

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn err<E: std::fmt::Display>(e: E) -> PyErr {
    PyRuntimeError::new_err(e.to_string())
}

fn to_py<T: serde::Serialize>(py: Python<'_>, v: &T) -> PyResult<PyObject> {
    pythonize::pythonize(py, v).map(|b| b.unbind()).map_err(err)
}

/// A coverage-scheduler handle bound to a project root.
///
/// `unsendable`: the SQLite connection inside is not `Sync`/`Send`, so the
/// handle is pinned to the thread that created it (fine under the GIL).
#[pyclass(unsendable)]
struct Trail {
    store: Store,
    cfg: Config,
    root: PathBuf,
}

#[pymethods]
impl Trail {
    /// Open (or create) the trail state for `root`. Loads `<root>/.trail.toml`.
    #[new]
    fn new(root: String) -> PyResult<Self> {
        let root = PathBuf::from(root);
        let cfg = Config::load(&root).map_err(err)?;
        let store = Store::open(&root.join(".trail").join("state.db")).map_err(err)?;
        Ok(Trail { store, cfg, root })
    }

    /// Scan the tree and register the folder snapshot.
    fn init(&mut self, py: Python<'_>) -> PyResult<PyObject> {
        let out = trail_core::scan(&self.root, &self.cfg).map_err(err)?;
        self.store
            .replace_folders(&out.folders, now_unix())
            .map_err(err)?;
        let v = serde_json::json!({ "folders": out.folders.len(), "excluded": out.excluded });
        to_py(py, &v)
    }

    /// Claim the next folder. Returns a dict with `status` ("ok" /
    /// "sweep-complete" / "none-available") matching the CLI.
    #[pyo3(signature = (task, agent=None, auto_sweep=false))]
    fn next(
        &mut self,
        py: Python<'_>,
        task: &str,
        agent: Option<&str>,
        auto_sweep: bool,
    ) -> PyResult<PyObject> {
        let r = self
            .store
            .next(task, &self.cfg, agent, None, auto_sweep, now_unix())
            .map_err(err)?;
        to_py(py, &r)
    }

    #[pyo3(signature = (task, path, agent=None, found=None, reason=None))]
    fn done(
        &mut self,
        py: Python<'_>,
        task: &str,
        path: &str,
        agent: Option<&str>,
        found: Option<i64>,
        reason: Option<&str>,
    ) -> PyResult<PyObject> {
        let r = self
            .store
            .complete(task, path, agent, WorkStatus::Done, reason, found, now_unix())
            .map_err(err)?;
        to_py(py, &r)
    }

    #[pyo3(signature = (task, path, agent=None, found=None, reason=None))]
    fn skip(
        &mut self,
        py: Python<'_>,
        task: &str,
        path: &str,
        agent: Option<&str>,
        found: Option<i64>,
        reason: Option<&str>,
    ) -> PyResult<PyObject> {
        let r = self
            .store
            .complete(task, path, agent, WorkStatus::Skipped, reason, found, now_unix())
            .map_err(err)?;
        to_py(py, &r)
    }

    fn status(&self, py: Python<'_>, task: &str) -> PyResult<PyObject> {
        to_py(py, &self.store.status(task).map_err(err)?)
    }

    #[pyo3(signature = (task, state=None))]
    fn list(&self, py: Python<'_>, task: &str, state: Option<&str>) -> PyResult<PyObject> {
        let filter = state.and_then(WorkStatus::from_db);
        to_py(py, &self.store.list(task, filter).map_err(err)?)
    }

    fn sweep_new(&mut self, py: Python<'_>, task: &str) -> PyResult<PyObject> {
        to_py(
            py,
            &self
                .store
                .open_new_sweep(task, &self.cfg, now_unix())
                .map_err(err)?,
        )
    }

    #[pyo3(signature = (task, all=false))]
    fn reset(&mut self, py: Python<'_>, task: &str, all: bool) -> PyResult<PyObject> {
        to_py(py, &self.store.reset(task, all).map_err(err)?)
    }

    #[pyo3(signature = (vacuum=false))]
    fn gc(&mut self, py: Python<'_>, vacuum: bool) -> PyResult<PyObject> {
        to_py(py, &self.store.gc(now_unix(), vacuum).map_err(err)?)
    }
}

#[pymodule]
fn trail(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Trail>()?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
