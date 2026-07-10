//! End-to-end across both crates: stand up a real `zgui-bridge` host with a mock
//! surface, then drive it through the `stryke-app` client exactly as the `App`
//! module would — `verbs`, `call` (ok + error), `get`, `list`, and a `sub` whose
//! emitted event is drained by `poll`. Proves the whole bus loop, host to client,
//! over a real Unix socket (clean in headless CI on Linux and macOS).

use serde_json::{json, Value};
use stryke_app::client;
use zgui_bridge::{serve, Handler};

struct Mock;

impl Handler for Mock {
    fn call(&self, verb: &str, args: Value) -> Result<Value, String> {
        match verb {
            "echo" => Ok(json!({ "echoed": args.get("msg").cloned().unwrap_or(Value::Null) })),
            other => Err(format!("no such verb: {other}")),
        }
    }
    fn get(&self, state: &str) -> Result<Value, String> {
        match state {
            "count" => Ok(json!(42)),
            other => Err(format!("no such state: {other}")),
        }
    }
    fn surface(&self) -> Value {
        json!({
            "verbs": [ { "id": "echo", "params": [ { "name": "msg", "type": "string" } ], "returns": "object" } ],
            "state": [ { "id": "count", "returns": "number" } ],
            "events": [ { "id": "tick", "payload": "object" } ],
        })
    }
}

#[test]
fn app_module_drives_a_running_host() {
    let app = format!("stryke-app-test-{}", std::process::id());
    let bridge = serve(&app, Mock).expect("serve");

    // open → reachable
    let opened = client::open(&app).expect("open");
    assert_eq!(opened["ok"], true);

    // list → includes this app
    let apps = client::list().expect("list");
    assert!(apps.as_array().unwrap().iter().any(|a| a == &json!(app)));

    // verbs → surface manifest
    let surface = client::verbs(&app).expect("verbs");
    assert_eq!(surface["state"][0]["id"], "count");

    // call echo → value back
    let r = client::call(&app, "echo", json!({ "msg": "hi" })).expect("call");
    assert_eq!(r["echoed"], "hi");

    // get count → 42
    assert_eq!(client::get(&app, "count").expect("get"), json!(42));

    // unknown verb → Err carrying the app's message
    let err = client::call(&app, "nope", Value::Null).unwrap_err();
    assert!(err.to_string().contains("no such verb"));

    // subscribe, emit from the host, poll drains the event
    client::subscribe(&app, "tick").expect("sub");
    bridge.emit("tick", json!({ "n": 1 }));
    let events = client::poll(&app).expect("poll");
    let arr = events.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["event"], "tick");
    assert_eq!(arr[0]["payload"]["n"], 1);
}

#[test]
fn here_resolves_the_hosting_app_from_the_env() {
    let app = format!("stryke-app-here-{}", std::process::id());
    let _bridge = serve(&app, Mock).expect("serve");

    // unset → App::here() errors (a script not running inside a bus app)
    std::env::remove_var("ZGUI_APP");
    assert!(client::here().is_err());

    // the host sets ZGUI_APP before running a hook/palette script
    std::env::set_var("ZGUI_APP", &app);
    let here = client::here().expect("here");
    assert_eq!(here["app"], app);
    assert_eq!(here["ok"], true);

    // and it reaches the same surface
    assert_eq!(client::get(&app, "count").expect("get"), json!(42));
    std::env::remove_var("ZGUI_APP");
}
