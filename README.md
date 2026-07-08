# Middlesox

Middlesox is a scriptable, event-driven controller for window managers and
compositors that expose IPC. Run `msx` as a daemon, subscribe to window manager
events, and trigger Rhai or executable scripts when those events match your
configuration.

It is designed around adapters: `mock` for tests and local development,
`mangowc` for MangoWC/dwl-style Wayland control, `hyprland` for the Hyprland
compositor (native socket IPC), and `socket` for external IPC bridges written
in any language.

- Watches adapter events such as workspace, focus, layout, and output changes.
- Runs scripts when event names and optional `prev`/`curr` conditions match.
- Lets Rhai scripts call `get("key")` and capability-checked `set("key", value)`.
- Exposes named commands through `msx exec <name>`.

Middlesox is not published to any package manager yet — build it from source
as described below.

## Building

Requirements: stable Rust (edition 2024) and Linux.

```bash
git clone <this repository>
cd middlesox
cargo build --release -p middlesox-cli
```

The binary is `target/release/msx`. Run the test suite with `cargo test`.

## Running the Service

### Quick install (cargo-make)

With [cargo-make](https://github.com/sagiegurari/cargo-make) installed:

```bash
cargo make install-service
```

This builds the release binary and then:

1. installs it to `~/.local/bin/msx`,
2. copies `example/config.toml` to `~/.config/middlesox/config.toml` and the
   example scripts to `~/.config/middlesox/scripts/`,
3. creates `~/.config/systemd/user/middlesox.service` if it does not exist,
4. enables and (re)starts the service.

There is also `cargo make dev`, which installs the same files but runs the
daemon in the foreground instead of through systemd.

### Manual install

```bash
install -m 0755 target/release/msx ~/.local/bin/msx
mkdir -p ~/.config/middlesox/scripts
cp example/config.toml ~/.config/middlesox/config.toml
cp -a example/scripts/. ~/.config/middlesox/scripts/
```

Create `~/.config/systemd/user/middlesox.service`:

```ini
[Unit]
Description=Middlesox window manager automation daemon
After=graphical-session.target
PartOf=graphical-session.target

[Service]
Type=simple
# Optional: wait for the compositor socket so msx does not restart-loop at login
ExecStartPre=/bin/sh -c 'until [ -S "$${XDG_RUNTIME_DIR}/$${WAYLAND_DISPLAY:-wayland-0}" ]; do sleep 1; done'
ExecStart=%h/.local/bin/msx --config %h/.config/middlesox/config.toml run
Restart=on-failure
RestartSec=2

[Install]
WantedBy=default.target
```

Then enable and start it:

```bash
systemctl --user daemon-reload
systemctl --user enable --now middlesox.service
```

### Managing the service

```bash
systemctl --user status middlesox      # service state
journalctl --user -u middlesox -f      # follow the daemon log
systemctl --user restart middlesox     # restart after config changes
```

Once the daemon is running (via systemd or in the foreground), interact with
it from any shell:

```bash
msx status                 # Check whether the daemon is running
msx caps                   # List adapter capabilities
msx get workspace          # Query a capability value
msx set layout_name Tile   # Set a writable capability value
msx commands               # List named commands from config
msx exec cycle_layout      # Run a named command
msx stop                   # Stop the daemon
```

### Trying it without a compositor

The `mock` adapter needs no window manager and is handy for a first run:

```bash
cat > /tmp/middlesox-mock.toml <<'TOML'
[adapter]
name = "mock"
TOML

./target/release/msx --config /tmp/middlesox-mock.toml run
```

## Configuration

`msx` loads the first config file it finds:

1. `./middlesox.toml`
2. `~/.config/middlesox/config.toml`
3. `/etc/middlesox/config.toml`

Use `--config <path>` to choose a file explicitly. If no config file exists,
`msx` uses a development-friendly default: the `mock` adapter and a
`workspace_change -> on_workspace_change.rhai` watch.

```toml
[settings]
scripts_dir = "scripts" # relative to this config file
log_level = "info"      # trace, debug, info, warn, error

[adapter]
name = "mangowc"        # mock | mangowc | hyprland | socket

# Hyprland adapter (sockets discovered from HYPRLAND_INSTANCE_SIGNATURE):
# [adapter]
# name = "hyprland"
# socket_dir = "/run/user/1000/hypr/<signature>"  # optional override

# Socket adapter for external bridges:
# [adapter]
# name = "socket"
# base_path = "/run/user/1000/my-bridge"
# This derives:
#   cmd_socket = "/run/user/1000/my-bridge.sock"
#   event_socket = "/run/user/1000/my-bridge-events.sock"
#
# Or set explicit paths:
# cmd_socket = "/run/user/1000/my-bridge.sock"
# event_socket = "/run/user/1000/my-bridge-events.sock"

[[watch]]
event = "workspace_change"
exec = "wallpaper.rhai"
description = "Change wallpaper when switching to workspace 2"
curr = { id = 2 }

[[watch]]
event = "focus_change"
exec = "on_focus.sh"
description = "Update external tools when focus changes"

[[command]]
name = "cycle_layout"
script = "cycle_layout.rhai"
description = "Cycle through available layouts"
```

A watch always matches by `event`. If `prev` or `curr` conditions are present,
every listed field must match the event state. If both are omitted, every event
with that name runs the script.

Script paths in `exec` and `script` fields resolve as follows:

| Config value | Resolution |
| --- | --- |
| `/usr/local/bin/notify` | Used directly |
| `wallpaper.rhai` | Joined with `scripts_dir` |
| `subdir/hook.sh` | Joined with `scripts_dir` |

Relative `scripts_dir` values are resolved relative to the config file's
directory. Absolute paths are used directly. `~/...` paths are expanded by the
CLI before scripts run.

## Scripts

Middlesox supports Rhai scripts and executable files.

### Rhai

Files ending in `.rhai` run in process. Watch scripts receive `event_name`,
`prev`, and `curr` in scope, plus adapter access through `get()` and `set()`.
Writes are checked against the adapter's capability manifest.

```rhai
// ~/.config/middlesox/scripts/cycle_layout.rhai
let current = get("layout_name");

if current == "Tile" {
    let err = set("layout_name", "Scroller");
    if err != "" {
        log(err);
    }
} else {
    let err = set("layout_name", "Tile");
    if err != "" {
        log(err);
    }
}
```

```rhai
// A watch script can inspect event context.
if event_name == "workspace_change" && curr.id == 2 {
    log("workspace 2 focused");
}
```

### Executables

Any non-`.rhai` path is spawned as a process. Middlesox passes the event payload
as the first argument and also sets environment variables:

- `$1`: JSON payload such as `{"event":"workspace_change","prev":{...},"curr":{...}}`
- `MSX_EVENT`: event name
- `MSX_PREV`: previous state as JSON, or `null`
- `MSX_CURR`: current state as JSON, or `null`

For `msx exec <name>`, the payload is
`{"event":"command","prev":null,"curr":null}`.

`.sh` files must include a shebang because Middlesox executes them directly.

```bash
#!/usr/bin/env bash
set -eu

payload="$1"
event="$(printf '%s' "$payload" | jq -r '.event')"
workspace="$(printf '%s' "$MSX_CURR" | jq -r '.id // empty')"

if [ "$event" = "workspace_change" ] && [ -n "$workspace" ]; then
    feh --bg-fill "$HOME/wallpapers/workspace-${workspace}.jpg"
fi
```

## Writing a Custom Bridge

To control a window manager Middlesox has no native adapter for, write a
bridge: an external process, in any language, that translates between your WM
and the Middlesox socket protocol. Configure the daemon with the `socket`
adapter pointing at the bridge (see [Configuration](#configuration)).

The bridge is the server. It listens on two Unix sockets that speak JSON
Lines (one JSON object per `\n`-terminated line):

- **Command socket** (`{base}.sock`) — request/response. Middlesox opens a
  new connection per request, writes one request line, and reads one response
  line. Requests must be answered within 10 seconds.
- **Event socket** (`{base}-events.sock`) — push. Middlesox connects once at
  startup, sends a `subscribe` request, and expects an acknowledgment line
  within 15 seconds. After that the bridge pushes event lines on the same
  connection whenever something happens in the WM.

### Command socket

Requests carry an `id` that the response must echo. Reply with `result` on
success or `error` on failure.

```json
← {"id":1,"method":"caps"}
→ {"id":1,"result":[{"name":"workspace","access":"rw","description":"Active workspace"},
                    {"name":"title","access":"ro"}]}

← {"id":2,"method":"get","params":{"key":"workspace"}}
→ {"id":2,"result":3}

← {"id":3,"method":"set","params":{"key":"workspace","value":5}}
→ {"id":3,"result":null}

← {"id":4,"method":"get","params":{"key":"bogus"}}
→ {"id":4,"error":"unknown key"}
```

`caps` is called once at daemon startup; it declares which keys exist and
whether scripts may write them. `access` accepts `rw`/`read_write` or
`ro`/`read_only` (case-insensitive; anything else is treated as read-only).

### Event socket

After accepting the connection, read the subscription request, acknowledge
it, then stream events:

```json
← {"id":0,"method":"subscribe","params":{"events":["workspace_change","focus_change"]}}
→ {"id":0,"result":null}

→ {"event":"workspace_change","prev":{"id":1},"curr":{"id":2}}
→ {"event":"focus_change","prev":{"title":"Terminal"},"curr":{"title":"Firefox"}}
```

`prev` and `curr` are optional objects; their fields are what watch
conditions match against and what scripts receive as `MSX_PREV`/`MSX_CURR`.
Only send events the daemon subscribed to (sending others is harmless — they
are matched against watches by name and simply never fire one).

### Minimal bridge skeleton (Python)

```python
#!/usr/bin/env python3
import json, os, socket, threading

BASE = os.path.expandvars("$XDG_RUNTIME_DIR/my-bridge")
CAPS = [{"name": "workspace", "access": "rw"}]

def handle_command(req):
    method, params = req["method"], req.get("params", {})
    if method == "caps":
        return {"id": req["id"], "result": CAPS}
    if method == "get" and params.get("key") == "workspace":
        return {"id": req["id"], "result": my_wm_get_workspace()}
    if method == "set" and params.get("key") == "workspace":
        my_wm_set_workspace(params["value"])
        return {"id": req["id"], "result": None}
    return {"id": req["id"], "error": f"unsupported: {method}"}

def command_server():
    srv = socket.socket(socket.AF_UNIX)
    srv.bind(BASE + ".sock"); srv.listen(8)
    while True:
        conn, _ = srv.accept()
        with conn, conn.makefile("rw") as f:
            req = json.loads(f.readline())
            f.write(json.dumps(handle_command(req)) + "\n")

def event_server():
    srv = socket.socket(socket.AF_UNIX)
    srv.bind(BASE + "-events.sock"); srv.listen(1)
    while True:
        conn, _ = srv.accept()
        with conn, conn.makefile("rw") as f:
            sub = json.loads(f.readline())
            f.write(json.dumps({"id": sub["id"], "result": None}) + "\n")
            f.flush()
            for name, prev, curr in my_wm_event_loop():  # blocks, yields events
                f.write(json.dumps({"event": name, "prev": prev, "curr": curr}) + "\n")
                f.flush()

threading.Thread(target=command_server, daemon=True).start()
event_server()
```

Point the daemon at it:

```toml
[adapter]
name = "socket"
base_path = "/run/user/1000/my-bridge"
```

Start the bridge before the daemon (or let systemd restart `middlesox` until
the bridge sockets exist).

## Native Adapters

Bridges are the fastest way to add a WM. For an in-process backend, implement
`ProtocolAdapter` in a new crate instead:

```rust
impl ProtocolAdapter for MyAdapter {
    fn name(&self) -> &str;
    fn manifest(&self) -> CapabilityManifest;
    async fn subscribe(&mut self, subscriptions: HashSet<String>) -> Result<()>;
    async fn next_event(&mut self) -> Result<Option<RawEvent>>;
    async fn get(&mut self, key: &str) -> Result<Value>;
    async fn set(&mut self, key: &str, value: Value) -> Result<()>;
    async fn init(&mut self) -> Result<()> { Ok(()) }
    async fn shutdown(&mut self) -> Result<()> { Ok(()) }
}
```

`next_event()` is polled inside `tokio::select!`, so it must be cancel-safe.
Adapters that read from non-cancel-safe APIs should isolate that work in a task
or thread and return events through `mpsc::Receiver::recv()`.

## Architecture

```text
User / CLI (`msx ...`)
        |
        | JSON Lines over the Middlesox Unix socket
        v
msx daemon
  - controller
  - adapter actor
  - Rhai and executable script handling
        |
        | ProtocolAdapter
        v
mock | mangowc | hyprland | socket
```

For a deeper process and event-flow description, see
[doc/architecture.md](doc/architecture.md).

## Workspace Layout

| Directory | Cargo package | Purpose |
| --- | --- | --- |
| `middlesox` | `middlesox` | Core library: adapters, capabilities, config, controller, script engine, control protocol |
| `middlesox-cli` | `middlesox-cli` | `msx` CLI and daemon entry point |
| `middlesox-tests` | `middlesox-tests` | Integration test harness |
| `mock` | `middlesox-mock` | Mock adapter for tests and local development |
| `mangowc` | `middlesox-mangowc` | MangoWC/dwl Wayland adapter |
| `socket` | `middlesox-socket` | Unix socket adapter for external bridge processes |
| `hyprland` | `middlesox-hyprland` | Hyprland adapter using native socket IPC |

## License

MIT
