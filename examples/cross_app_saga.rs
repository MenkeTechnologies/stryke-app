//! A compensating transaction spanning three independent applications.
//!
//! Run it: `cargo run --example cross_app_saga`.
//!
//! Three `zgui-bridge` hosts are stood up in this process on three real sockets — one
//! each for a fake `zftp`, `zcontainer` and `zcite`, every one holding its own mutable
//! state. A single saga then pushes two artifacts, rolls a deployment, files a citation,
//! and finally runs a verify step that fails. The abort walks the saga journal backwards
//! and compensates every step across all three apps, in strict reverse execution order.
//!
//! `zcontainer`'s rollback is wired to fail, to show a compensation failure being
//! reported and the unwind carrying on past it rather than stopping.
//!
//! The apps are fake; the sockets, the frames, the journal and the unwind are real — the
//! same `zgui_bridge::serve` the shipped apps call, driven by the same `stryke_app::client`
//! the `App` stryke module calls.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use stryke_app::saga::Saga;
use zgui_bridge::{serve, socket_path, Handler};

/// A stand-in app: a string-keyed store, a log of what it did, and verbs that mutate it.
/// `rev` opts every mutating verb into transactions; `undo` is the real inverse, so the
/// store itself proves whether compensation happened.
struct FakeApp {
    name: &'static str,
    store: Mutex<BTreeMap<String, String>>,
    log: Arc<Mutex<Vec<String>>>,
    /// When set, `undo` of this verb fails — a compensation that cannot complete.
    undo_fails: Option<&'static str>,
}

impl FakeApp {
    fn new(name: &'static str, log: Arc<Mutex<Vec<String>>>, undo_fails: Option<&'static str>) -> Self {
        FakeApp {
            name,
            store: Mutex::new(BTreeMap::new()),
            log,
            undo_fails,
        }
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
        match verb {
            // Every mutating verb writes one key and returns the handle needed to undo it.
            "file.push" | "deploy.roll" | "citation.file" => {
                let key = Self::key(&args);
                let value = args
                    .get("value")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                self.store.lock().unwrap().insert(key.clone(), value);
                self.log
                    .lock()
                    .unwrap()
                    .push(format!("do   {}.{verb}({key})", self.name));
                Ok(json!({ "key": key, "app": self.name }))
            }
            // The deliberate mid-chain failure.
            "deploy.verify" => Err("health check failed: 2/3 replicas unready".to_string()),
            other => Err(format!("no such verb: {other}")),
        }
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
        if self.undo_fails == Some(verb) {
            self.log
                .lock()
                .unwrap()
                .push(format!("FAIL {}.{verb} (compensation refused)", self.name));
            return Err("registry tag is pinned; rollback refused".to_string());
        }
        let key = Self::key(&args);
        self.store.lock().unwrap().remove(&key);
        self.log
            .lock()
            .unwrap()
            .push(format!("undo {}.{verb}({key})", self.name));
        Ok(json!({ "undone": verb }))
    }

    fn rev(&self, verb: &str) -> &'static str {
        match verb {
            "file.push" | "deploy.roll" | "citation.file" => "inverse",
            // A check that mutates nothing: admitted into a transaction, never journaled,
            // so its failure is the app's own verdict and not the admission gate's.
            "deploy.verify" => "pure",
            _ => "irreversible",
        }
    }
}

fn main() {
    let pid = std::process::id();
    let ftp = Box::leak(format!("zftp-demo-{pid}").into_boxed_str());
    let ctr = Box::leak(format!("zcontainer-demo-{pid}").into_boxed_str());
    let cite = Box::leak(format!("zcite-demo-{pid}").into_boxed_str());

    let log = Arc::new(Mutex::new(Vec::new()));
    let _b1 = serve(ftp, FakeApp::new("zftp", log.clone(), None)).expect("serve zftp");
    // zcontainer refuses to compensate its rolled deployment.
    let _b2 = serve(
        ctr,
        FakeApp::new("zcontainer", log.clone(), Some("deploy.roll")),
    )
    .expect("serve zcontainer");
    let _b3 = serve(cite, FakeApp::new("zcite", log.clone(), None)).expect("serve zcite");

    println!("three live app sockets:");
    for a in [&*ftp, &*ctr, &*cite] {
        println!("  {}", socket_path(a).unwrap().display());
    }

    let mut saga = Saga::new(4242);
    println!("\nforward chain under txn {}:", saga.txn());

    let chain: Vec<(&str, &str, Value)> = vec![
        (ftp, "file.push", json!({ "key": "app-1.7.2.tar.zst", "value": "sha256:aa" })),
        (ctr, "deploy.roll", json!({ "key": "web", "value": "1.7.2" })),
        (ftp, "file.push", json!({ "key": "app-1.7.2.sig", "value": "sha256:bb" })),
        (cite, "citation.file", json!({ "key": "release-note", "value": "doi:10.0/1.7.2" })),
        (ctr, "deploy.verify", json!({ "key": "web" })),
    ];

    let mut failure: Option<String> = None;
    for (app, verb, args) in chain {
        match saga.call(app, verb, args) {
            Ok(_) => println!("  seq {}  {app}  {verb}  ok", saga.steps().len()),
            Err(e) => {
                println!("  ----  {app}  {verb}  FAILED: {e}");
                failure = Some(e.to_string());
                break;
            }
        }
    }

    let report = if failure.is_some() {
        println!("\nunwinding (reverse saga seq, across all three apps):");
        saga.abort()
    } else {
        saga.commit()
    };

    for step in report["order"].as_array().cloned().unwrap_or_default() {
        println!(
            "  seq {}  {}  {}",
            step["seq"],
            step["app"].as_str().unwrap_or(""),
            step["verb"].as_str().unwrap_or("")
        );
    }

    println!("\nwhat each app actually executed, in order:");
    for line in log.lock().unwrap().iter() {
        println!("  {line}");
    }

    println!("\nper-app compensation report:");
    println!("{}", serde_json::to_string_pretty(&report["apps"]).unwrap());
    println!("failed[] (surfaced, not swallowed):");
    println!("{}", serde_json::to_string_pretty(&report["failed"]).unwrap());

    println!("\nstate left in each app (empty == fully compensated):");
    for (label, app) in [("zftp", &*ftp), ("zcontainer", &*ctr), ("zcite", &*cite)] {
        let store = stryke_app::client::get(app, "store").unwrap();
        println!("  {label:<11} {store}");
    }

    for a in [&*ftp, &*ctr, &*cite] {
        let _ = std::fs::remove_file(socket_path(a).unwrap());
    }
}
