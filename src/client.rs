//! The bus client: a process-wide pool of per-app Unix-socket connections and the
//! request/reply + event-poll logic behind the `App` module (see
//! `GUI_AUTOMATION_BUS.md` §6/§7). Transport errors reconnect once and retry; a
//! verb that returns `ok:false` is a value the caller sees, not a transport error.

use std::collections::HashMap;
use std::env;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use once_cell::sync::OnceCell;
use serde_json::{json, Value};

/// One live connection to an app's socket, plus its correlation counter and a
/// buffer of events that arrived while awaiting a reply.
struct Conn {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
    events: Vec<Value>,
    next_id: u64,
}

fn pool() -> &'static Mutex<HashMap<String, Conn>> {
    static POOL: OnceCell<Mutex<HashMap<String, Conn>>> = OnceCell::new();
    POOL.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The socket directory, matching the host: `$XDG_RUNTIME_DIR/zgui`, else
/// `$TMPDIR/zgui`, else `/tmp/zgui`.
fn socket_dir() -> PathBuf {
    let base = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .or_else(|| env::var_os("TMPDIR").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("zgui")
}

fn socket_path(app: &str) -> PathBuf {
    socket_dir().join(format!("{app}.sock"))
}

fn connect(app: &str) -> Result<Conn> {
    let path = socket_path(app);
    let stream = UnixStream::connect(&path)
        .map_err(|e| anyhow!("app '{app}' not reachable ({}): {e}", path.display()))?;
    let reader = BufReader::new(stream.try_clone()?);
    Ok(Conn { writer: stream, reader, events: Vec::new(), next_id: 0 })
}

/// Run `f` against the app's pooled connection, dialing it if absent. On a
/// transport error, reconnect once and retry — this absorbs an app that
/// restarted between calls (RFC §13.1: hold, reconnect on EPIPE).
fn with_conn<T>(app: &str, f: impl Fn(&mut Conn) -> Result<T>) -> Result<T> {
    let mut guard = pool().lock().unwrap();
    if !guard.contains_key(app) {
        let c = connect(app)?;
        guard.insert(app.to_string(), c);
    }
    match f(guard.get_mut(app).unwrap()) {
        Ok(v) => Ok(v),
        Err(_) => {
            let c = connect(app)?;
            guard.insert(app.to_string(), c);
            f(guard.get_mut(app).unwrap())
        }
    }
}

/// Send one request frame (stamping a fresh `id`) and read frames until the
/// matching reply arrives, buffering any interleaved events. Returns the whole
/// reply frame; transport failures are `Err`.
fn request(conn: &mut Conn, mut frame: Value) -> Result<Value> {
    conn.next_id += 1;
    let id = conn.next_id;
    frame["id"] = json!(id);
    let mut line = serde_json::to_vec(&frame)?;
    line.push(b'\n');
    conn.writer.write_all(&line)?;
    conn.writer.flush()?;

    loop {
        let mut buf = String::new();
        if conn.reader.read_line(&mut buf)? == 0 {
            bail!("connection closed");
        }
        let v: Value = serde_json::from_str(buf.trim())?;
        match v.get("t").and_then(|t| t.as_str()) {
            Some("reply") if v.get("id").and_then(|i| i.as_u64()) == Some(id) => return Ok(v),
            Some("event") => conn.events.push(v),
            _ => {} // stray frame for another id — ignore
        }
    }
}

/// Unwrap a reply frame: its `value` on success, an error carrying the app's
/// message on `ok:false` (so the stryke side `die`s with the real reason).
fn unwrap_reply(reply: Value) -> Result<Value> {
    if reply.get("ok").and_then(|b| b.as_bool()) == Some(true) {
        Ok(reply.get("value").cloned().unwrap_or(Value::Null))
    } else {
        Err(anyhow!(
            "{}",
            reply.get("error").and_then(|e| e.as_str()).unwrap_or("call failed")
        ))
    }
}

/// Invoke a verb and return its value.
pub fn call(app: &str, verb: &str, args: Value) -> Result<Value> {
    let reply = with_conn(app, |c| request(c, json!({ "t": "call", "verb": verb, "args": args })))?;
    unwrap_reply(reply)
}

/// Read a state query and return its value.
pub fn get(app: &str, state: &str) -> Result<Value> {
    let reply = with_conn(app, |c| request(c, json!({ "t": "get", "state": state })))?;
    unwrap_reply(reply)
}

/// Fetch the app's typed automation surface (verbs/state/events).
pub fn verbs(app: &str) -> Result<Value> {
    let reply = with_conn(app, |c| request(c, json!({ "t": "verbs" })))?;
    unwrap_reply(reply)
}

/// Confirm an app is reachable (dial + pool it); returns `{app, ok:true}`.
pub fn open(app: &str) -> Result<Value> {
    with_conn(app, |_| Ok(()))?;
    Ok(json!({ "app": app, "ok": true }))
}

/// Every running app, by bus name — the `*.sock` files in the socket dir.
pub fn list() -> Result<Value> {
    let mut apps = Vec::new();
    if let Ok(rd) = std::fs::read_dir(socket_dir()) {
        for entry in rd.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                if let Some(app) = name.strip_suffix(".sock") {
                    apps.push(app.to_string());
                }
            }
        }
    }
    apps.sort();
    Ok(json!(apps))
}

/// Subscribe to an event; delivered events buffer until [`poll`] drains them.
pub fn subscribe(app: &str, event: &str) -> Result<Value> {
    let reply = with_conn(app, |c| request(c, json!({ "t": "sub", "event": event })))?;
    unwrap_reply(reply)
}

/// Drain the events that have arrived for an app since the last poll (RFC §13.3:
/// pull model, so events surface in stryke's own execution flow).
pub fn poll(app: &str) -> Result<Value> {
    with_conn(app, |c| {
        c.reader
            .get_ref()
            .set_read_timeout(Some(Duration::from_millis(20)))?;
        loop {
            let mut buf = String::new();
            match c.reader.read_line(&mut buf) {
                Ok(0) => break,
                Ok(_) => {
                    if let Ok(v) = serde_json::from_str::<Value>(buf.trim()) {
                        if v.get("t").and_then(|t| t.as_str()) == Some("event") {
                            c.events.push(v);
                        }
                    }
                }
                Err(ref e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    break
                }
                Err(e) => {
                    let _ = c.reader.get_ref().set_read_timeout(None);
                    return Err(e.into());
                }
            }
        }
        c.reader.get_ref().set_read_timeout(None)?;
        let drained: Vec<Value> = c.events.drain(..).collect();
        Ok(json!(drained))
    })
}
