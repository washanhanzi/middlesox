# Middlesox

Universal Window Manager Controller - scriptable event-driven automation for any WM/compositor with IPC.

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│                    User Scripts (Rhai)                   │
│         get("workspace"), set("layout", "grid")          │
│         Access: prev, curr, event_name                   │
└─────────────────────────────────────────────────────────┘
                            │
┌─────────────────────────────────────────────────────────┐
│                    Protocol Adapter                      │
│       Hyprland │ MangoWC │ i3 │ yabai │ komorebi        │
└─────────────────────────────────────────────────────────┘
```

## CLI

```bash
msx get workspace          # Query value
msx set layout grid        # Set value
msx run cycle_layout       # Run named command
msx caps                   # List capabilities
```

## Configuration

```toml
[settings]
backend = "hyprland"
scripts_dir = "scripts"

# Declarative: match prev/curr, then execute
[[watch]]
event = "workspace_change"
exec = "toggle_layout.rhai"
[watch.prev]
id = 1
[watch.curr]
id = 2

# Scriptable: script handles matching + action
[[watch_with_script]]
event = "workspace_change"
script = "smart_layout.rhai"

# Named command (invokable via CLI)
[[command]]
name = "cycle_layout"
script = "cycle_layout.rhai"
```

## Scripting

```rust
// Declarative watch script - matching done by config
let layout = get("layout");
set("layout", if layout == "master" { "grid" } else { "master" });

// Script watch - full control over matching
if curr.id > 5 && prev.id <= 5 {
    set("layout", "grid");
}
```

## Project Structure

```
middlesox/       # Core library + daemon
cli/             # CLI (msx binary)
hyprland/        # Hyprland backend
mangowc/         # MangoWC/dwl backend
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
