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
per-app Unix-socket connection in a process-wide pool (no fork per call).

Host side: [`zgui-bridge`](https://github.com/MenkeTechnologies/zgui-bridge).
Design: [`GUI_AUTOMATION_BUS.md`](https://github.com/MenkeTechnologies/MenkeTechnologiesMeta/blob/main/GUI_AUTOMATION_BUS.md) (§6 the `App` module, §7 transport).

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

## Surface

| stryke | FFI export | Purpose |
| --- | --- | --- |
| `App::open(name)` | `app__open` | confirm reachable + pool the connection |
| `App::list()` | `app__list` | every running app's bus name |
| `App::verbs(name)` | `app__verbs` | the typed surface manifest |
| `App::call(name, verb, args)` | `app__call` | invoke a verb, return its value |
| `App::get(name, state)` | `app__get` | read a state query |
| `App::sub(name, event)` | `app__sub` | subscribe (events buffer) |
| `App::poll(name)` | `app__poll` | drain events since the last poll |

## Protocol

Newline-delimited JSON over the app's Unix socket (`$XDG_RUNTIME_DIR/zgui/<app>.sock`,
else `$TMPDIR/zgui/<app>.sock`). A request stamps a fresh `id`; the client reads until
the matching `reply`, buffering any interleaved `event` frames for `poll`. Transport
errors reconnect once and retry. Full frame spec in `GUI_AUTOMATION_BUS.md` §7.1.

## Build

```
cargo build              # cdylib + rlib
cargo test               # end-to-end against a real zgui-bridge host
```

## License

MIT — see [`LICENSE`](LICENSE).
