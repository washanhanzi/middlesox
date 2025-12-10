# Middlesox

Universal Window Manager Controller - scriptable event-driven automation for any WM/compositor with IPC.

## Architecture

```
┌─────────────────────────────────────────────┐
│         User Scripts (Rhai / Shell)         │
│    get("workspace"), set("layout", "grid")  │
└──────────────────────┬──────────────────────┘
                       │
┌──────────────────────┴──────────────────────┐
│              msx daemon                     │
│         (control socket + flock)            │
└──────────────────────┬──────────────────────┘
                       │ ProtocolAdapter
       ┌───────────────┼───────────────┐
       ▼               ▼               ▼
   Hyprland        MangoWC          i3/Sway
   (Wayland)      (Wayland)          (X11)
```

## Configuration

Config file is loaded from (first match wins):
1. `./middlesox.toml` (current directory)
2. `~/.config/middlesox/config.toml`
3. `/etc/middlesox/config.toml`

### Example

```toml
[settings]
scripts_dir = "~/.config/middlesox/scripts"  # Default location
log_level = "info"                           # trace, debug, info, warn, error

# Adapter: mock, hyprland, mangowc, or socket
[adapter]
name = "hyprland"

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
msx exec cycle_layout      # Run named command
```

## Adding a Backend

```rust
impl ProtocolAdapter for MyBackend {
    fn name(&self) -> &str;
    fn manifest(&self) -> CapabilityManifest;
    async fn listen(&self, tx: Sender<RawEvent>, subs: HashSet<String>) -> Result<()>;
    async fn get(&self, key: &str) -> Result<Value>;
    async fn set(&self, key: &str, value: Value) -> Result<()>;
}
```

## License

MIT
