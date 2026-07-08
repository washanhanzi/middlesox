# Middlesox

Middlesox is a scriptable, event-driven controller for window managers and
compositors that expose IPC. Run `msx` as a daemon, subscribe to window manager
events, and trigger Rhai or executable scripts when those events match your
configuration.

It is designed around adapters: `mock` for tests and local development,
`mangowc` for MangoWC/dwl-style Wayland control, `hyprland` for the Hyprland
compositor (native socket IPC), and `socket` for external IPC bridges.

## What It Does

- Watches adapter events such as workspace, focus, layout, and output changes.
- Runs scripts when event names and optional `prev`/`curr` conditions match.
- Lets Rhai scripts call `get("key")` and capability-checked `set("key", value)`.
- Exposes named commands through `msx exec <name>`.
- Provides a socket adapter so external bridge processes can connect other
  window managers without changing the core library.

## Quick Start

Build the CLI:

```bash
cargo build -p middlesox-cli
```

Create a small mock config and run the daemon:

```bash
cat > /tmp/middlesox-mock.toml <<'TOML'
[adapter]
name = "mock"
TOML

./target/debug/msx --config /tmp/middlesox-mock.toml run
```

In another shell, talk to the daemon:

```bash
./target/debug/msx status
./target/debug/msx caps
./target/debug/msx get workspace
./target/debug/msx stop
```

For a local MangoWC setup, copy the example config and scripts, then run with
that config:

```bash
mkdir -p ~/.config/middlesox/scripts
cp example/config.toml ~/.config/middlesox/config.toml
cp -a example/scripts/. ~/.config/middlesox/scripts/
./target/debug/msx --config ~/.config/middlesox/config.toml run
```

If you use `cargo-make`, the repository also includes convenience tasks:

```bash
cargo make dev
cargo make install-service
```

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

## Configuration

`msx` loads the first config file it finds:

1. `./middlesox.toml`
2. `~/.config/middlesox/config.toml`
3. `/etc/middlesox/config.toml`

If no config file exists, `msx` uses a development-friendly default: the `mock`
adapter and a `workspace_change -> on_workspace_change.rhai` watch.

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

## CLI

```bash
msx run                    # Start the daemon
msx stop                   # Stop the daemon
msx status                 # Check whether the daemon is running
msx caps                   # List adapter capabilities
msx get workspace          # Query a capability value
msx set layout_name Tile   # Set a writable capability value
msx commands               # List named commands from config
msx exec cycle_layout      # Run a named command
```

Use `--config <path>` with `msx run` to choose the daemon config explicitly.

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

## Adding an Adapter

Implement `ProtocolAdapter` for the target window manager or compositor:

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

## License

MIT
