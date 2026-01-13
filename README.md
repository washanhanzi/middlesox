# Middlesox

Universal Window Manager Controller - scriptable event-driven automation for any WM/compositor with IPC.

## Architecture

```
┌─────────────────────────────────────────────┐
│            User / CLI (`msx ...`)           │
└──────────────────────┬──────────────────────┘
                       │ JSON Lines over Unix socket
┌──────────────────────┴──────────────────────┐
│                 msx daemon                  │
│   Controller + Adapter Actor + Script I/O   │
└──────────────────────┬──────────────────────┘
                       │ `ProtocolAdapter`
       ┌───────────────┼───────────────┬───────────────┐
       ▼               ▼               ▼               ▼
      mock          mangowc          socket       hyprland
    (testing)      (Wayland)     (external IPC)    (stub)
```

## Configuration

Config file is loaded from (first match wins):
1. `./middlesox.toml` (current directory)
2. `~/.config/middlesox/config.toml`
3. `/etc/middlesox/config.toml`

If no config file is found, `msx` falls back to a built-in default config. Today that means the `mock` adapter plus a default `workspace_change -> on_workspace_change.rhai` watch.

### Example

```toml
[settings]
scripts_dir = "~/.config/middlesox/scripts"  # Example scripts directory
log_level = "info"                           # trace, debug, info, warn, error

# Adapter: mock, mangowc, or socket
[adapter]
name = "mangowc"

# Socket adapter (for external bridges)
# [adapter]
# name = "socket"
# base_path = "/run/user/1000/my-bridge"  # derives .sock and -events.sock
# # Or explicit paths:
# # cmd_socket = "/run/user/1000/my-bridge.sock"
# # event_socket = "/run/user/1000/my-bridge-events.sock"

# Simple declarative watch: match event + conditions, run script
[[watch]]
event = "workspace_change"
exec = "wallpaper.rhai"
description = "Change wallpaper when switching to workspace 2"
prev = { id = 1 }
curr = { id = 2 }

# Scriptable watch: script handles both matching and action
[[watch]]
event = "workspace_change"
exec = "on_workspace_change.rhai"
description = "Custom workspace change handler"

[[watch]]
event = "focus_change"
exec = "on_focus.rhai"

# Named commands (invoked via `msx exec <name>`)
[[command]]
name = "cycle_layout"
script = "cycle_layout.rhai"
description = "Cycle through available layouts"

[[command]]
name = "toggle_float"
script = "toggle_float.rhai"
```

## Scripts

### Path Resolution

Script paths in `exec` and `script` fields are resolved as follows:

1. **Absolute path** (`/usr/local/bin/notify.sh`) - executed directly
2. **Relative path** (`wallpaper.sh`) - joined with `scripts_dir`

If `scripts_dir` itself is relative, it is resolved relative to the config file's directory.

Default `scripts_dir`: `~/.config/middlesox/scripts`

| `exec` value | `scripts_dir` | Resolved path |
|--------------|---------------|---------------|
| `/usr/bin/notify-send` | (any) | `/usr/bin/notify-send` |
| `wallpaper.sh` | (default) | `~/.config/middlesox/scripts/wallpaper.sh` |
| `wallpaper.sh` | `/opt/scripts` | `/opt/scripts/wallpaper.sh` |
| `sub/script.sh` | (default) | `~/.config/middlesox/scripts/sub/script.sh` |

### Script Types

**Rhai scripts** (`.rhai` extension) run in-process with access to WM state:
```rhai
// ~/.config/middlesox/scripts/cycle_layout.rhai
let current = get("layout");
if current == "master" {
    set("layout", "grid");
} else {
    set("layout", "master");
}
```

**Shell scripts / executables** (any other extension) spawn as a subprocess with event context available via:

1. **Positional argument** (`$1`): Full JSON payload
2. **Environment variables**: `MSX_EVENT`, `MSX_PREV`, `MSX_CURR`

`.sh` files must include a shebang because Middlesox executes them directly.
For `msx exec <name>`, shell commands receive `{"event":"command","prev":null,"curr":null}`.

```bash
#!/bin/bash
# ~/.config/middlesox/scripts/wallpaper.sh

# Option 1: Parse the JSON argument
payload="$1"
echo "Event: $(echo "$payload" | jq -r '.event')"      # e.g., "workspace_change"
echo "Prev:  $(echo "$payload" | jq -c '.prev')"       # JSON: {"id": 1}
echo "Curr:  $(echo "$payload" | jq -c '.curr')"       # JSON: {"id": 2}

# Option 2: Use environment variables (simpler for many cases)
echo "Event: $MSX_EVENT"                               # e.g., "workspace_change"
echo "Prev:  $MSX_PREV"                                # JSON: {"id": 1}
echo "Curr:  $MSX_CURR"                                # JSON: {"id": 2}

# Change wallpaper based on workspace
id=$(echo "$MSX_CURR" | jq -r '.id')
feh --bg-fill ~/wallpapers/workspace-$id.jpg
```

## CLI

```bash
msx run                    # Start daemon
msx stop                   # Stop daemon
msx status                 # Check if running
msx get workspace          # Query value
msx set layout grid        # Set value
msx caps                   # List capabilities
msx commands               # List named commands
msx exec cycle_layout      # Run named command
```

## Adding a Backend

```rust
impl ProtocolAdapter for MyBackend {
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

`next_event()` is polled inside `tokio::select!`, so it must be cancel-safe. Backends that read from non-cancel-safe sources should bridge them through an internal task or thread and return events via `mpsc::Receiver::recv()`.

## License

MIT
