//! Cross-app compensating transactions — one chain spanning several independent
//! desktop applications, unwound in reverse on any failure.
//!
//! `zgui-bridge` ships the per-app half: a connection can `begin` a transaction, its
//! calls are journaled, and `abort` compensates that **one app's** steps in descending
//! `seq`. What it cannot do is order steps across apps. Each app is a separate process
//! with its own `AtomicU64` seq clock, so `zftp` seq 2 and `zcite` seq 2 are unrelated
//! numbers; per-app aborts would unwind each app internally correctly and yet run the
//! apps' compensations in an order that never happened.
//!
//! So the orchestrator keeps the only clock that spans the chain: a client-side journal
//! in call order. A [`Saga`] then does three things the single-app path does not:
//!
//! 1. **Admission control, from the host.** The first call to an app sends `begin`, so
//!    the host applies its own `rev` gate — a verb it classes `irreversible` is refused
//!    *before it runs*, and the chain fails without a stranded half-effect.
//! 2. **One global order.** Every successful step is appended to [`Saga::steps`] with a
//!    saga-wide `seq`. [`Saga::abort`] walks it in descending `seq` and compensates each
//!    step with a single out-of-band `undo` frame to the app that ran it — strict reverse
//!    execution order across app boundaries. Restricted to any one app that walk is still
//!    that app's descending `seq`, so the per-app guarantee is preserved, not traded away.
//! 3. **Failures surfaced, per app.** A compensation that fails does not stop the unwind
//!    and is not swallowed: it lands in the report's `failed[]`, and again under
//!    `apps.<name>.failed[]` so a caller can see which application is now inconsistent.
//!
//! After the unwind every participating app is sent `commit`, which discards the journal
//! the host kept in parallel **without compensating** — the steps were just compensated
//! here, and a host-side `abort` on top would run every inverse a second time. That
//! parallel host journal is not waste: it is the recovery path if the orchestrating
//! process dies mid-chain, since the transaction stays open and abortable by anything
//! that can dial the socket.

use std::collections::HashMap;
use std::sync::Mutex;

use anyhow::{anyhow, Result};
use once_cell::sync::OnceCell;
use serde_json::{json, Value};

use crate::client;

/// One executed forward step of a saga, in global call order.
#[derive(Debug, Clone)]
pub struct Step {
    /// Saga-wide sequence number, 1-based, spanning every participating app.
    pub seq: u64,
    pub app: String,
    pub verb: String,
    pub args: Value,
    pub result: Value,
}

/// A transaction spanning several apps: which apps joined, and every step so far.
pub struct Saga {
    txn: u64,
    /// Apps sent `begin`, in the order they first participated.
    joined: Vec<String>,
    steps: Vec<Step>,
}

impl Saga {
    /// Start a saga under transaction id `txn`. No app is contacted until the first
    /// [`call`](Saga::call) — a saga that touches nothing costs nothing.
    pub fn new(txn: u64) -> Self {
        Saga {
            txn,
            joined: Vec::new(),
            steps: Vec::new(),
        }
    }

    pub fn txn(&self) -> u64 {
        self.txn
    }

    /// The forward steps executed so far, in call order.
    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    /// Invoke `verb` on `app` inside this saga.
    ///
    /// The app is sent `begin` on first participation, so the host journals the call and
    /// applies its `rev` gate to it. A successful call is appended to the saga journal; a
    /// failed one is not — it left no forward effect, so there is nothing to compensate,
    /// exactly as the host's own `call_in_txn` decides. The error propagates so the caller
    /// can [`abort`](Saga::abort).
    pub fn call(&mut self, app: &str, verb: &str, args: Value) -> Result<Value> {
        if !self.joined.iter().any(|a| a == app) {
            client::begin(app, self.txn)?;
            self.joined.push(app.to_string());
        }
        let result = client::call(app, verb, args.clone())?;
        self.steps.push(Step {
            seq: self.steps.len() as u64 + 1,
            app: app.to_string(),
            verb: verb.to_string(),
            args,
            result: result.clone(),
        });
        Ok(result)
    }

    /// Close the saga: `commit` every participating app, discarding both journals. The
    /// saga's own steps are cleared, so a later abort of the same handle unwinds nothing.
    ///
    /// A `commit` that cannot be delivered (the app went away) is reported rather than
    /// aborting the loop — the remaining apps still get closed.
    pub fn commit(&mut self) -> Value {
        let mut failed = Vec::new();
        for app in self.joined.iter().rev() {
            if let Err(e) = client::commit(app, self.txn) {
                failed.push(json!({ "app": app, "error": e.to_string() }));
            }
        }
        let committed = self.steps.len();
        self.steps.clear();
        self.joined.clear();
        json!({
            "txn": self.txn,
            "ok": failed.is_empty(),
            "committed": committed,
            "failed": failed,
        })
    }

    /// Unwind the whole chain: compensate every step in descending saga `seq` — the exact
    /// reverse of execution, across every app — then `commit` each app so the host's
    /// parallel journal is discarded rather than compensated a second time.
    ///
    /// Every step is attempted even after one fails; the failures are reported, both
    /// globally and grouped by app.
    pub fn abort(&mut self) -> Value {
        let mut compensated: u64 = 0;
        let mut failed: Vec<Value> = Vec::new();
        let mut order: Vec<Value> = Vec::new();
        // Per app: [compensated, failed…], in the order the apps first appear in the unwind.
        let mut per_app: Vec<(String, u64, Vec<Value>)> = Vec::new();

        for step in self.steps.iter().rev() {
            order.push(json!({ "seq": step.seq, "app": step.app, "verb": step.verb }));
            let outcome = client::undo(
                &step.app,
                &step.verb,
                step.args.clone(),
                step.result.clone(),
            );
            let slot = match per_app.iter().position(|(a, _, _)| a == &step.app) {
                Some(i) => &mut per_app[i],
                None => {
                    per_app.push((step.app.clone(), 0, Vec::new()));
                    per_app.last_mut().unwrap()
                }
            };
            match outcome {
                Ok(_) => {
                    compensated += 1;
                    slot.1 += 1;
                }
                Err(e) => {
                    let entry = json!({
                        "app": step.app,
                        "seq": step.seq,
                        "verb": step.verb,
                        "error": e.to_string(),
                    });
                    slot.2.push(entry.clone());
                    failed.push(entry);
                }
            }
        }

        let mut apps = serde_json::Map::new();
        for (app, ok, errs) in per_app {
            apps.insert(app, json!({ "compensated": ok, "failed": errs }));
        }

        // Release the host-side journals. They mirror what was just compensated here, so
        // a host `abort` now would run every inverse twice; `commit` discards without
        // compensating. Delivery failures ride along in the report.
        let mut release_failed = Vec::new();
        for app in self.joined.iter().rev() {
            if let Err(e) = client::commit(app, self.txn) {
                release_failed.push(json!({ "app": app, "error": e.to_string() }));
            }
        }

        self.steps.clear();
        self.joined.clear();

        json!({
            "txn": self.txn,
            "ok": failed.is_empty() && release_failed.is_empty(),
            "compensated": compensated,
            "order": order,
            "failed": failed,
            "apps": Value::Object(apps),
            "releaseFailed": release_failed,
        })
    }
}

/* ---- process-wide registry, so the FFI can address a saga by transaction id ---- */

fn registry() -> &'static Mutex<HashMap<u64, Saga>> {
    static R: OnceCell<Mutex<HashMap<u64, Saga>>> = OnceCell::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Mint a transaction id that cannot collide with another process's. The low 32 bits
/// are a per-process counter, the high bits the pid — two scripts driving the same app
/// at once must not land in one another's journal, and the host keys journals by id
/// alone.
fn next_txn() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst) + 1;
    ((std::process::id() as u64) << 32) | (n & 0xffff_ffff)
}

/// Open a saga. `txn` of `None` mints a process-unique id. Returns `{txn}`.
pub fn begin(txn: Option<u64>) -> Result<Value> {
    let txn = txn.unwrap_or_else(next_txn);
    let mut reg = registry().lock().unwrap();
    if reg.contains_key(&txn) {
        return Err(anyhow!("saga already open: {txn}"));
    }
    reg.insert(txn, Saga::new(txn));
    Ok(json!({ "txn": txn }))
}

/// Run one step of an open saga.
pub fn call(txn: u64, app: &str, verb: &str, args: Value) -> Result<Value> {
    let mut reg = registry().lock().unwrap();
    let saga = reg
        .get_mut(&txn)
        .ok_or_else(|| anyhow!("no open saga: {txn}"))?;
    saga.call(app, verb, args)
}

/// Commit and close an open saga.
pub fn commit(txn: u64) -> Result<Value> {
    let mut saga = registry()
        .lock()
        .unwrap()
        .remove(&txn)
        .ok_or_else(|| anyhow!("no open saga: {txn}"))?;
    Ok(saga.commit())
}

/// Abort and close an open saga, returning the unwind report.
pub fn abort(txn: u64) -> Result<Value> {
    let mut saga = registry()
        .lock()
        .unwrap()
        .remove(&txn)
        .ok_or_else(|| anyhow!("no open saga: {txn}"))?;
    Ok(saga.abort())
}

/// The forward steps of an open saga, as `[{seq, app, verb}]` — what an unwind would
/// reverse. For a script that wants to report progress without ending the saga.
pub fn steps(txn: u64) -> Result<Value> {
    let reg = registry().lock().unwrap();
    let saga = reg.get(&txn).ok_or_else(|| anyhow!("no open saga: {txn}"))?;
    Ok(json!(saga
        .steps()
        .iter()
        .map(|s| json!({ "seq": s.seq, "app": s.app, "verb": s.verb }))
        .collect::<Vec<_>>()))
}
