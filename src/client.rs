//! The bus client: a process-wide pool of per-app connections and the
//! request/reply + event-poll logic behind the `App` module (see
//! `GUI_AUTOMATION_BUS.md` §6/§7). Transport errors reconnect once and retry; a
//! verb that returns `ok:false` is a value the caller sees, not a transport error.
//!
//! Transport is per platform, matching the host: a Unix-domain socket
//! (`$XDG_RUNTIME_DIR/zgui/<app>.sock`) on macOS/Linux, the named pipe
//! `\\.\pipe\<app>.sock` on Windows. The protocol below is identical on both — the
//! only platform-specific bits are dialing (`plat::dial`) and the poll drain-mode
//! toggle (`plat::set_drain_mode`).

use std::collections::HashMap;
use std::env;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{anyhow, bail, Result};
use once_cell::sync::OnceCell;
use serde_json::{json, Value};

use plat::Sock;

/* ---- Unix domain socket (macOS / Linux) ---- */
#[cfg(unix)]
mod plat {
    use anyhow::{anyhow, Result};
    pub use std::os::unix::net::UnixStream as Sock;
    use std::path::Path;
    use std::time::Duration;

    /// Dial the app's Unix socket at `path`.
    pub fn dial(path: &Path, app: &str) -> Result<Sock> {
        Sock::connect(path)
            .map_err(|e| anyhow!("app '{app}' not reachable ({}): {e}", path.display()))
    }

    /// Toggle the poll drain mode: a short read timeout while draining (so an empty
    /// bus returns promptly), blocking reads otherwise.
    pub fn set_drain_mode(s: &Sock, draining: bool) -> std::io::Result<()> {
        s.set_read_timeout(draining.then(|| Duration::from_millis(20)))
    }
}

/* ---- named pipe (Windows) ---- */
#[cfg(windows)]
mod plat {
    use anyhow::{anyhow, Result};
    pub use interprocess::local_socket::Stream as Sock;
    use interprocess::local_socket::{prelude::*, GenericNamespaced};
    use std::path::Path;

    /// Dial the app's named pipe. The leaf of `path` (`<app>.sock`) maps to
    /// `\\.\pipe\<app>.sock`, matching the host's `to_ns_name::<GenericNamespaced>()`.
    pub fn dial(path: &Path, app: &str) -> Result<Sock> {
        let leaf = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        let name = leaf
            .to_ns_name::<GenericNamespaced>()
            .map_err(|e| anyhow!("bad pipe name for app '{app}': {e}"))?;
        Sock::connect(name)
            .map_err(|e| anyhow!(r"app '{app}' not reachable (\\.\pipe\{leaf}): {e}"))
    }

    /// Toggle the poll drain mode: named pipes have no read timeout, so we use
    /// nonblocking mode while draining (read returns `WouldBlock` when empty) and
    /// restore blocking mode afterwards.
    pub fn set_drain_mode(s: &Sock, draining: bool) -> std::io::Result<()> {
        s.set_nonblocking(draining)
    }
}

/// One live connection to an app's endpoint, plus its correlation counter and a
/// buffer of events that arrived while awaiting a reply. Writes go through the
/// reader's `get_ref()` (`&Sock: Write`), so a single owned handle serves both
/// directions — no clone needed (named-pipe handles are not clonable).
struct Conn {
    reader: BufReader<Sock>,
    events: Vec<Value>,
    next_id: u64,
    /// The transaction this connection has open on the host, if any. The host
    /// keeps that state **per connection** (`zgui-bridge` `handle_conn`'s
    /// `open_txn`), so a reconnect has to re-join it — see [`with_conn`].
    open_txn: Option<u64>,
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
    let sock = plat::dial(&path, app)?;
    Ok(Conn {
        reader: BufReader::new(sock),
        events: Vec::new(),
        next_id: 0,
        open_txn: None,
    })
}

/// Run `f` against the app's pooled connection, dialing it if absent. On a
/// transport error, reconnect once and retry — this absorbs an app that
/// restarted between calls (RFC §13.1: hold, reconnect on EPIPE).
///
/// A connection that was inside a transaction re-joins it on the fresh socket
/// before the retry. The host's `open_txn` is per connection while its journal is
/// per process, so re-sending `begin` rejoins the *same* journal; skipping it
/// would let the retried call run untransacted — executed, unjournaled, and
/// therefore silently un-unwindable by a later abort.
fn with_conn<T>(app: &str, f: impl Fn(&mut Conn) -> Result<T>) -> Result<T> {
    let mut guard = pool().lock().unwrap();
    if !guard.contains_key(app) {
        let c = connect(app)?;
        guard.insert(app.to_string(), c);
    }
    match f(guard.get_mut(app).unwrap()) {
        Ok(v) => Ok(v),
        Err(_) => {
            let open_txn = guard.get(app).and_then(|c| c.open_txn);
            let mut c = connect(app)?;
            if let Some(txn) = open_txn {
                unwrap_reply(request(&mut c, json!({ "t": "begin", "txn": txn }))?)?;
                c.open_txn = Some(txn);
            }
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
    // Write through the reader's handle (`&Sock: Write`) — one handle, both directions.
    let mut w = conn.reader.get_ref();
    w.write_all(&line)?;
    w.flush()?;

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
            reply
                .get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("call failed")
        ))
    }
}

/// Invoke a verb and return its value.
pub fn call(app: &str, verb: &str, args: Value) -> Result<Value> {
    let reply = with_conn(app, |c| {
        request(c, json!({ "t": "call", "verb": verb, "args": args }))
    })?;
    unwrap_reply(reply)
}

/* ---- transaction frames (GUI_AUTOMATION_BUS.md §7.2) ---- */

/// Open transaction `txn` on `app`: every subsequent [`call`] on this app's pooled
/// connection is journaled by the host under `txn`, and a verb whose reversibility
/// class is `irreversible` is refused before it runs.
///
/// The host treats a `begin` for an already-open transaction as a **join**, not a
/// collision — that is what lets several apps (and several connections) share one
/// transaction id.
pub fn begin(app: &str, txn: u64) -> Result<Value> {
    let reply = with_conn(app, |c| {
        let r = request(c, json!({ "t": "begin", "txn": txn }));
        if r.is_ok() {
            c.open_txn = Some(txn);
        }
        r
    })?;
    unwrap_reply(reply)
}

/// Close `txn` on `app`, discarding its journal. No compensation runs.
pub fn commit(app: &str, txn: u64) -> Result<Value> {
    let reply = with_conn(app, |c| {
        let r = request(c, json!({ "t": "commit", "txn": txn }));
        if r.is_ok() && c.open_txn == Some(txn) {
            c.open_txn = None;
        }
        r
    })?;
    unwrap_reply(reply)
}

/// Abort `txn` on `app`: the host compensates every step **it** journaled, in
/// descending `seq`, and replies `{compensated, failed:[…]}`.
///
/// This unwinds one app. Its `seq` clock is private to that app's process, so it
/// cannot order steps against another app's — a chain spanning several apps is
/// unwound by [`crate::saga`], which keeps the cross-app order itself.
pub fn abort(app: &str, txn: u64) -> Result<Value> {
    let reply = with_conn(app, |c| {
        let r = request(c, json!({ "t": "abort", "txn": txn }));
        if r.is_ok() && c.open_txn == Some(txn) {
            c.open_txn = None;
        }
        r
    })?;
    unwrap_reply(reply)
}

/// Compensate one already-executed verb out of band, quoting back the `args` it ran
/// with and the `result` it returned. This is the single-step primitive a cross-app
/// unwind is built from, because the caller — not the host — chooses the order.
pub fn undo(app: &str, verb: &str, args: Value, result: Value) -> Result<Value> {
    let reply = with_conn(app, |c| {
        request(
            c,
            json!({ "t": "undo", "verb": verb, "args": args, "result": result }),
        )
    })?;
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

/// The name of the app a script is running inside — set by the host in `ZGUI_APP`
/// before it runs a palette/hook script (`run_stryke_hook`). This is what
/// `App::here()` resolves so an in-process script drives its own app without
/// naming it. Errors if unset (the script isn't running inside a bus app).
pub fn here_name() -> Result<String> {
    env::var("ZGUI_APP")
        .ok()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("App::here() outside a bus app (ZGUI_APP unset)"))
}

/// Open the app the current script runs inside (`ZGUI_APP`); `{app, ok:true}`.
pub fn here() -> Result<Value> {
    open(&here_name()?)
}

/// Every running app, by bus name — the `*.sock` files in the socket dir.
///
/// Unix only: named pipes are not enumerable as filesystem entries, so on Windows
/// this returns an empty list. Dialing a known app by name still works there.
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
        plat::set_drain_mode(c.reader.get_ref(), true)?;
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
                    let _ = plat::set_drain_mode(c.reader.get_ref(), false);
                    return Err(e.into());
                }
            }
        }
        plat::set_drain_mode(c.reader.get_ref(), false)?;
        let drained: Vec<Value> = c.events.drain(..).collect();
        Ok(json!(drained))
    })
}
