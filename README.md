# Middlesox

Universal Window Manager Controller - scriptable event-driven automation for any WM/compositor with IPC.

## Architecture

```
┌─────────────────────────────────────────────┐
│            User Scripts (Rhai)              │
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
