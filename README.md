```
     _              _
 ___| |_ _ __ _   _| | _____        __ _ _ __  _ __
/ __| __| '__| | | | |/ / _ \_____ / _` | '_ \| '_ \
\__ \ |_| |  | |_| |   <  __/_____| (_| | |_) | |_) |
|___/\__|_|   \__, |_|\_\___|      \__,_| .__/| .__/
              |___/                     |_|   |_|
              [ a p p ]
```

### `[ APP AUTOMATION FOR STRYKE // DRIVE YOUR GUI APPS BY NAME ]`

> *"AppleScript for your own suite, one stryke pipe away — cross-platform, JIT-compiled."*

App automation for stryke — call a MenkeTechnologies GUI app's verbs, read its
state, and subscribe to its events over the **GUI Automation Bus**. Semantic, not
pixel: where [`stryke-gui`](https://github.com/MenkeTechnologies/stryke-gui) moves
the mouse, `stryke-app` calls `library.search` and gets rows back. Shipped as a
cdylib that stryke dlopens in-process on first `use App`; the client holds a
per-app connection in a process-wide pool (no fork per call) — a Unix domain
socket on macOS/Linux, a named pipe on Windows.

Host side: [`zgui-bridge`](https://github.com/MenkeTechnologies/zgui-bridge).
Design: [`GUI_AUTOMATION_BUS.md`](https://github.com/MenkeTechnologies/MenkeTechnologiesMeta/blob/main/docs/GUI_AUTOMATION_BUS.md) (§6 the `App` module, §7 transport).

---

## Quick start

```stryke
#!/usr/bin/env stryke
use App

# every running app
p App::list()                                     # -> ["zcite", "zreq", ...]

# drive one by name
val $cite = App::open("zcite")
val @hits = @{ $cite->call("library.search", %{ q => "graphene" }) }
p "found ${\ scalar @hits}"

val @sel = @{ $cite->get("selection") }           # read state

# events: subscribe, then drain in your own flow
$cite->sub("itemAdded")
for val $ev (@{ $cite->poll() }) { p "added: ${ $ev->{payload}{title} }" }
```

## One transaction across separate applications

A chain that spans several apps unwinds in reverse — the exact reverse of the order the
steps executed, across process boundaries — when any step fails.

```stryke
val $txn = App::txn()
val $ok  = eval {
    $txn->call("zftp",       "file.push",     %{ path => "app-1.7.2.tar.zst" })
    $txn->call("zcontainer", "deploy.roll",   %{ service => "web", tag => "1.7.2" })
    $txn->call("zcite",      "citation.file", %{ doi => "10.0/1.7.2" })
    1
}
if (!$ok) {
    val $r = $txn->abort()
    p "unwound ${ $r->{compensated} }; still broken: ${\ scalar @{ $r->{failed} } }"
} else {
    $txn->commit()
}
```

The first call to an app sends `begin`, so the **app** refuses a verb it cannot undo before
that verb runs. Each app also journals the chain in parallel, which is the recovery path if
this script dies mid-flight. `abort` walks this side's journal in descending saga `seq`,
compensating each step with one `undo` frame to the app that ran it, then releases the app-side
journals so no inverse runs twice. A compensation that fails does not stop the unwind and is
reported — globally in `failed[]` and under `apps.<name>.failed[]`.

Design and the reason a per-app `abort` is not enough: `docs/GUI_AUTOMATION_BUS.md` §7.3.

## Surface

| stryke | FFI export | Purpose |
| --- | --- | --- |
| `App::here()` | `app__here` | open the app this script runs inside (`ZGUI_APP`) |
| `App::open(name)` | `app__open` | confirm reachable + pool the connection |
| `App::list()` | `app__list` | every running app's bus name |
| `$h->verbs()` | `app__verbs` | the typed surface manifest |
| `$h->call(verb, args)` | `app__call` | invoke a verb, return its value |
| `$h->get(state)` | `app__get` | read a state query |
| `$h->sub(event)` | `app__sub` | subscribe (events buffer) |
| `$h->poll()` | `app__poll` | drain events since the last poll |
| `$h->undo(verb, args, result)` | `app__undo` | compensate one executed verb, out of band |
| `App::txn([id])` | `app__txn_begin` | open a cross-app compensating transaction |
| `$txn->call(app, verb, args)` | `app__txn_call` | one journaled step of the transaction |
| `$txn->steps()` | `app__txn_steps` | `{seq, app, verb}` per forward step so far |
| `$txn->commit()` | `app__txn_commit` | close it; every app keeps its effects |
| `$txn->abort()` | `app__txn_abort` | unwind every step in reverse, across every app |

## Protocol

Newline-delimited JSON over the app's endpoint — a Unix socket
(`$XDG_RUNTIME_DIR/zgui/<app>.sock`, else `$TMPDIR/zgui/<app>.sock`) on macOS/Linux, the
named pipe `\\.\pipe\<app>.sock` on Windows. A request stamps a fresh `id`; the client reads until
the matching `reply`, buffering any interleaved `event` frames for `poll`. Transport
errors reconnect once and retry, re-joining an open transaction on the fresh connection first.
Full frame spec in `docs/GUI_AUTOMATION_BUS.md` §7.1, transactions in §7.2/§7.3.

## Build

```
cargo build                         # cdylib + rlib
cargo test                          # end-to-end against real zgui-bridge hosts
cargo run --example cross_app_saga  # three live sockets, a failed chain, the reverse unwind
```

## License

MIT — see [`LICENSE`](LICENSE).
