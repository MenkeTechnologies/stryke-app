//! Cross-app compensating transactions, end to end over real sockets.
//!
//! Every test stands up two or three `zgui_bridge::serve` hosts **in this process** with
//! fake surfaces, then drives them through `stryke_app::saga` — no GUI app has to be
//! running, so this is clean in headless CI on Linux and macOS.
//!
//! Unix-only, matching `zgui-bridge`'s own `tests/roundtrip.rs`: the socket path is
//! enumerable and removable here, which the assertions and cleanup rely on. The
//! named-pipe transport shares all the code above `plat::dial`.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use stryke_app::{client, saga::Saga};
use zgui_bridge::{serve, socket_path, Handler};

/// A stand-in app: a key/value store plus a shared log recording every forward and
/// compensating step in the order it ran. The log is what pins down unwind ORDER; the
/// store is what pins down unwind EFFECT.
struct FakeApp {
    label: String,
    store: Mutex<BTreeMap<String, String>>,
    log: Arc<Mutex<Vec<String>>>,
    /// Verbs whose compensation fails.
    undo_fails: Vec<&'static str>,
    /// Verbs classed irreversible, so the host refuses them inside a transaction.
    irreversible: Vec<&'static str>,
}

impl FakeApp {
    fn new(label: &str, log: Arc<Mutex<Vec<String>>>) -> Self {
        FakeApp {
            label: label.to_string(),
            store: Mutex::new(BTreeMap::new()),
            log,
            undo_fails: Vec::new(),
            irreversible: Vec::new(),
        }
    }

    fn undo_fails(mut self, verbs: &[&'static str]) -> Self {
        self.undo_fails = verbs.to_vec();
        self
    }

    fn irreversible(mut self, verbs: &[&'static str]) -> Self {
        self.irreversible = verbs.to_vec();
        self
    }

    fn key(args: &Value) -> String {
        args.get("key")
            .and_then(|k| k.as_str())
            .unwrap_or("?")
            .to_string()
    }
}

impl Handler for FakeApp {
    fn call(&self, verb: &str, args: Value) -> Result<Value, String> {
        if verb == "fails" {
            return Err("forward step failed".to_string());
        }
        let key = Self::key(&args);
        self.store.lock().unwrap().insert(key.clone(), verb.into());
        self.log
            .lock()
            .unwrap()
            .push(format!("do {}.{verb}({key})", self.label));
        Ok(json!({ "key": key }))
    }

    fn get(&self, state: &str) -> Result<Value, String> {
        match state {
            "store" => Ok(json!(*self.store.lock().unwrap())),
            other => Err(format!("no such state: {other}")),
        }
    }

    fn surface(&self) -> Value {
        json!({ "verbs": [], "state": [ { "id": "store", "returns": "object" } ], "events": [] })
    }

    fn undo(&self, verb: &str, args: Value, _result: Value) -> Result<Value, String> {
        if self.undo_fails.contains(&verb) {
            self.log
                .lock()
                .unwrap()
                .push(format!("FAIL {}.{verb}", self.label));
            return Err("compensation refused".to_string());
        }
        let key = Self::key(&args);
        self.store.lock().unwrap().remove(&key);
        self.log
            .lock()
            .unwrap()
            .push(format!("undo {}.{verb}({key})", self.label));
        Ok(json!({ "undone": verb }))
    }

    fn rev(&self, verb: &str) -> &'static str {
        if self.irreversible.contains(&verb) {
            "irreversible"
        } else if verb == "check" {
            "pure"
        } else {
            "inverse"
        }
    }
}

/// Unique per-test socket names, so tests running in parallel in one process never share
/// a host or a pooled client connection.
fn app_names(tag: &str, n: usize) -> Vec<String> {
    (0..n)
        .map(|i| format!("stryke-app-saga-{tag}-{}-{i}", std::process::id()))
        .collect()
}

fn cleanup(apps: &[String]) {
    for a in apps {
        let _ = std::fs::remove_file(socket_path(a).unwrap());
    }
}

fn store_of(app: &str) -> Value {
    client::get(app, "store").expect("get store")
}

/// The load-bearing guarantee: a chain that interleaves three apps unwinds in strict
/// reverse EXECUTION order, not app by app.
///
/// The chain is deliberately non-contiguous per app — a(1) b(2) a(3) c(4) — because that
/// is the only shape that separates a global reverse walk from the obvious alternative of
/// aborting each app in reverse join order. That alternative would produce
/// `c4, a3, a1, b2` (each app internally correct, `b2` stranded to the end); the correct
/// order is `c4, a3, b2, a1`. Both compensate everything, so only the ORDER assertion
/// below distinguishes them — and order is what a saga is for.
#[test]
fn unwind_is_reverse_execution_order_across_three_apps() {
    let apps = app_names("order", 3);
    let log = Arc::new(Mutex::new(Vec::new()));
    let _b: Vec<_> = apps
        .iter()
        .enumerate()
        .map(|(i, a)| {
            serve(
                a,
                FakeApp::new(["a", "b", "c"][i], log.clone()),
            )
            .expect("serve")
        })
        .collect();

    let mut saga = Saga::new(1);
    saga.call(&apps[0], "push", json!({ "key": "k1" })).unwrap();
    saga.call(&apps[1], "roll", json!({ "key": "k2" })).unwrap();
    saga.call(&apps[0], "push", json!({ "key": "k3" })).unwrap();
    saga.call(&apps[2], "file", json!({ "key": "k4" })).unwrap();

    // The mid-chain failure. It is the app's own verdict, and it must leave the saga
    // journal at four steps — a failed forward step had no effect to compensate.
    let err = saga.call(&apps[1], "fails", json!({})).unwrap_err();
    assert!(err.to_string().contains("forward step failed"), "{err}");
    assert_eq!(saga.steps().len(), 4, "a failed step must not be journaled");

    let report = saga.abort();
    assert_eq!(report["compensated"], 4);
    assert_eq!(report["ok"], true);

    let order: Vec<String> = report["order"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| format!("{}:{}", s["seq"], s["app"].as_str().unwrap()))
        .collect();
    assert_eq!(
        order,
        vec![
            format!("4:{}", apps[2]),
            format!("3:{}", apps[0]),
            format!("2:{}", apps[1]),
            format!("1:{}", apps[0]),
        ],
        "unwind must be descending saga seq across app boundaries"
    );

    // And the apps really ran it in that order, not merely reported it.
    assert_eq!(
        *log.lock().unwrap(),
        vec![
            "do a.push(k1)",
            "do b.roll(k2)",
            "do a.push(k3)",
            "do c.file(k4)",
            "undo c.file(k4)",
            "undo a.push(k3)",
            "undo b.roll(k2)",
            "undo a.push(k1)",
        ]
    );

    // Every app is back to empty: compensation had real effect.
    for a in &apps {
        assert_eq!(store_of(a), json!({}), "{a} not fully compensated");
    }
    cleanup(&apps);
}

/// A compensation that fails is reported — globally and under its app — and the unwind
/// carries on to the steps beneath it rather than stopping at the first casualty.
#[test]
fn compensation_failure_is_surfaced_and_the_unwind_continues() {
    let apps = app_names("fail", 2);
    let log = Arc::new(Mutex::new(Vec::new()));
    let _b0 = serve(&apps[0], FakeApp::new("a", log.clone())).expect("serve");
    let _b1 = serve(
        &apps[1],
        FakeApp::new("b", log.clone()).undo_fails(&["roll"]),
    )
    .expect("serve");

    let mut saga = Saga::new(2);
    saga.call(&apps[0], "push", json!({ "key": "k1" })).unwrap();
    saga.call(&apps[1], "roll", json!({ "key": "k2" })).unwrap();
    saga.call(&apps[0], "push", json!({ "key": "k3" })).unwrap();

    let report = saga.abort();
    assert_eq!(report["ok"], false);
    assert_eq!(report["compensated"], 2, "the two survivors still unwound");

    let failed = report["failed"].as_array().unwrap();
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0]["app"], apps[1]);
    assert_eq!(failed[0]["seq"], 2);
    assert_eq!(failed[0]["verb"], "roll");
    assert_eq!(failed[0]["error"], "compensation refused");

    // Same failure again under its app, so a caller can see WHICH app is inconsistent.
    assert_eq!(report["apps"][&apps[1]]["compensated"], 0);
    assert_eq!(
        report["apps"][&apps[1]]["failed"].as_array().unwrap().len(),
        1
    );
    assert_eq!(report["apps"][&apps[0]]["compensated"], 2);
    assert_eq!(
        report["apps"][&apps[0]]["failed"].as_array().unwrap().len(),
        0
    );

    // seq 1 was compensated even though seq 2's compensation failed first.
    assert_eq!(
        *log.lock().unwrap(),
        vec![
            "do a.push(k1)",
            "do b.roll(k2)",
            "do a.push(k3)",
            "undo a.push(k3)",
            "FAIL b.roll",
            "undo a.push(k1)",
        ]
    );

    // The refusing app kept its effect; that is exactly what `failed[]` is telling you.
    assert_eq!(store_of(&apps[0]), json!({}));
    assert_eq!(store_of(&apps[1]), json!({ "k2": "roll" }));
    cleanup(&apps);
}

/// The saga compensates through out-of-band `undo` frames, while the hosts journal the
/// same steps in parallel. If the saga did not release those journals, a host-side abort
/// afterwards would run every inverse a SECOND time. It must find nothing.
///
/// This also pins the reason the parallel host journal exists: it is live and abortable
/// while the saga is open (recovery if the orchestrator dies), and only then discarded.
#[test]
fn abort_releases_the_host_journals_instead_of_double_compensating() {
    let apps = app_names("release", 2);
    let log = Arc::new(Mutex::new(Vec::new()));
    let _b0 = serve(&apps[0], FakeApp::new("a", log.clone())).expect("serve");
    let _b1 = serve(&apps[1], FakeApp::new("b", log.clone())).expect("serve");

    let mut saga = Saga::new(3);
    saga.call(&apps[0], "push", json!({ "key": "k1" })).unwrap();
    saga.call(&apps[1], "roll", json!({ "key": "k2" })).unwrap();
    let report = saga.abort();
    assert_eq!(report["compensated"], 2);

    let after_unwind = log.lock().unwrap().clone();

    // A host-side abort of the same transaction, per app, now compensates nothing.
    for a in &apps {
        let r = client::abort(a, 3).expect("host abort");
        assert_eq!(r["compensated"], 0, "{a} would have double-compensated");
        assert_eq!(r["failed"].as_array().unwrap().len(), 0);
    }
    assert_eq!(
        *log.lock().unwrap(),
        after_unwind,
        "no inverse may run twice"
    );
    cleanup(&apps);
}

/// Admission control comes from the host, not the client: joining a transaction is what
/// makes an app refuse a verb it cannot undo, BEFORE that verb runs. A saga therefore
/// never contains a step that will strand it.
#[test]
fn an_irreversible_verb_is_refused_before_it_runs() {
    let apps = app_names("gate", 2);
    let log = Arc::new(Mutex::new(Vec::new()));
    let _b0 = serve(&apps[0], FakeApp::new("a", log.clone())).expect("serve");
    let _b1 = serve(
        &apps[1],
        FakeApp::new("b", log.clone()).irreversible(&["wire.transfer"]),
    )
    .expect("serve");

    let mut saga = Saga::new(4);
    saga.call(&apps[0], "push", json!({ "key": "k1" })).unwrap();

    let err = saga
        .call(&apps[1], "wire.transfer", json!({ "key": "k2" }))
        .unwrap_err();
    assert!(
        err.to_string().contains("verb not reversible"),
        "expected the host's rev gate, got: {err}"
    );

    // Refused before running: it is absent from the app's log and from its store.
    assert_eq!(*log.lock().unwrap(), vec!["do a.push(k1)"]);
    assert_eq!(store_of(&apps[1]), json!({}));
    assert_eq!(saga.steps().len(), 1);

    // A `pure` verb is admitted and simply not journaled — it changed nothing to undo.
    saga.call(&apps[1], "check", json!({ "key": "probe" }))
        .unwrap();
    assert_eq!(
        saga.steps().len(),
        2,
        "the saga journals what it called; the HOST is what declines to journal `pure`"
    );

    let report = saga.abort();
    assert_eq!(report["compensated"], 2);
    cleanup(&apps);
}

/// Commit is the other half: it closes every participating app's transaction and
/// compensates nothing, so a chain that succeeded stays applied.
#[test]
fn commit_leaves_every_app_applied() {
    let apps = app_names("commit", 2);
    let log = Arc::new(Mutex::new(Vec::new()));
    let _b0 = serve(&apps[0], FakeApp::new("a", log.clone())).expect("serve");
    let _b1 = serve(&apps[1], FakeApp::new("b", log.clone())).expect("serve");

    let mut saga = Saga::new(5);
    saga.call(&apps[0], "push", json!({ "key": "k1" })).unwrap();
    saga.call(&apps[1], "roll", json!({ "key": "k2" })).unwrap();

    let report = saga.commit();
    assert_eq!(report["ok"], true);
    assert_eq!(report["committed"], 2);

    assert_eq!(
        *log.lock().unwrap(),
        vec!["do a.push(k1)", "do b.roll(k2)"],
        "commit must not compensate"
    );
    assert_eq!(store_of(&apps[0]), json!({ "k1": "push" }));
    assert_eq!(store_of(&apps[1]), json!({ "k2": "roll" }));

    // And the host journals are gone, so a later abort finds nothing to unwind.
    for a in &apps {
        assert_eq!(client::abort(a, 5).expect("host abort")["compensated"], 0);
    }
    cleanup(&apps);
}
