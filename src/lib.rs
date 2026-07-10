//! stryke-app — app-automation cdylib loaded in-process by stryke via dlopen.
//!
//! Each `#[no_mangle] extern "C" fn app__*(args_json: *const c_char) -> *const c_char`
//! is a JSON-string-in / JSON-string-out wrapper around [`client`], which holds a
//! per-app Unix-socket connection to a `zgui-bridge` host. stryke's FFI bridge
//! resolves these symbols on first `use App`, registers each as a stryke-callable
//! function, and frees each returned string via [`stryke_free_cstring`].
//!
//! This is the semantic counterpart to `stryke-gui`: `stryke-gui` moves the mouse,
//! `stryke-app` calls a named app's verbs and reads values back. See
//! `GUI_AUTOMATION_BUS.md` for the design and the `App` module surface (§6).

pub mod client;

use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::panic::AssertUnwindSafe;

use anyhow::Result;
use serde_json::{json, Value};

/// Run a handler taking a parsed JSON `Value` and returning a JSON `Value`,
/// turning any error or panic into `{"error": "<msg>"}` so the stryke side can
/// `die` on it. Always returns a freshly allocated `CString` the caller frees
/// via [`stryke_free_cstring`].
fn ffi_call<F>(args: *const c_char, handler: F) -> *const c_char
where
    F: FnOnce(Value) -> Result<Value>,
{
    let input = if args.is_null() {
        Value::Null
    } else {
        // SAFETY: `args` is a NUL-terminated string from stryke's FFI bridge,
        // valid for the duration of this call.
        let cs = unsafe { CStr::from_ptr(args) };
        serde_json::from_slice::<Value>(cs.to_bytes()).unwrap_or(Value::Null)
    };
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| handler(input)));
    let out = match result {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => json!({ "error": e.to_string() }),
        Err(_) => json!({ "error": "stryke-app handler panicked" }),
    };
    let s = serde_json::to_string(&out).unwrap_or_else(|_| String::from(r#"{"error":"serialize"}"#));
    match CString::new(s) {
        Ok(c) => c.into_raw() as *const c_char,
        Err(_) => std::ptr::null(),
    }
}

/// String field `key` from a JSON args object, or an error naming it.
fn arg_str(v: &Value, key: &str) -> Result<String> {
    v.get(key)
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("missing '{key}' argument"))
}

/// Free a C string previously returned by an export of this cdylib. stryke's FFI
/// bridge calls this after copying the bytes into a stryke string.
///
/// # Safety
///
/// `p` must be a pointer returned by an export of this cdylib (a
/// `CString::into_raw` output), or null. Any other pointer is undefined behavior.
#[no_mangle]
pub unsafe extern "C" fn stryke_free_cstring(p: *mut c_char) {
    if p.is_null() {
        return;
    }
    drop(CString::from_raw(p));
}

/// `App::open(name)` — confirm an app is reachable, pool its connection.
#[no_mangle]
pub extern "C" fn app__open(args: *const c_char) -> *const c_char {
    ffi_call(args, |v| client::open(&arg_str(&v, "app")?))
}

/// `App::here()` — open the app this script runs inside (via `ZGUI_APP`).
#[no_mangle]
pub extern "C" fn app__here(_args: *const c_char) -> *const c_char {
    ffi_call(_args, |_| client::here())
}

/// `App::list()` — every running app's bus name.
#[no_mangle]
pub extern "C" fn app__list(args: *const c_char) -> *const c_char {
    ffi_call(args, |_| client::list())
}

/// `App::verbs(name)` — the typed surface manifest.
#[no_mangle]
pub extern "C" fn app__verbs(args: *const c_char) -> *const c_char {
    ffi_call(args, |v| client::verbs(&arg_str(&v, "app")?))
}

/// `App::call(name, verb, args)` — invoke a verb, return its value.
#[no_mangle]
pub extern "C" fn app__call(args: *const c_char) -> *const c_char {
    ffi_call(args, |v| {
        let app = arg_str(&v, "app")?;
        let verb = arg_str(&v, "verb")?;
        let call_args = v.get("args").cloned().unwrap_or(Value::Null);
        client::call(&app, &verb, call_args)
    })
}

/// `App::get(name, state)` — read a state query.
#[no_mangle]
pub extern "C" fn app__get(args: *const c_char) -> *const c_char {
    ffi_call(args, |v| {
        let app = arg_str(&v, "app")?;
        let state = arg_str(&v, "state")?;
        client::get(&app, &state)
    })
}

/// `App::sub(name, event)` — subscribe; events buffer until `App::poll`.
#[no_mangle]
pub extern "C" fn app__sub(args: *const c_char) -> *const c_char {
    ffi_call(args, |v| {
        let app = arg_str(&v, "app")?;
        let event = arg_str(&v, "event")?;
        client::subscribe(&app, &event)
    })
}

/// `App::poll(name)` — drain the events delivered since the last poll.
#[no_mangle]
pub extern "C" fn app__poll(args: *const c_char) -> *const c_char {
    ffi_call(args, |v| client::poll(&arg_str(&v, "app")?))
}
