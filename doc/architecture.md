# Middlesox Architecture

Universal window manager controller — scriptable, event-driven automation for any WM/compositor with IPC.

## Process Model

In the common case there are two Middlesox processes at runtime:

- the long-running `msx run` daemon
- short-lived CLI client invocations such as `msx get`, `msx set`, or `msx stop`

The daemon may also spawn child OS processes for shell watches and `msx exec` shell commands.

### Daemon (`msx run`)

Long-running server process with two core long-lived tokio tasks, one Ctrl+C watcher task, plus per-connection control handlers:

1. **Adapter Actor** — a dedicated task that owns the `Box<dyn ProtocolAdapter>`. Runs a `select!` loop over `adapter.next_event()` and a command channel (`Get`/`Set`/`Manifest`/`Shutdown` with oneshot replies). All adapter access goes through `AdapterHandle`, a cloneable channel-based handle held by the controller and script engine.

2. **Controller** (`Controller::run()`) — the main `select!` loop that processes events from the adapter actor, accepts control socket connections, and handles shutdown signals.

```
┌──────────────────────────────────────────────────────────────────┐
│                        msx daemon                                │
│                                                                  │
│   ┌───────────────────────────────────────────────────────────┐  │
│   │              Adapter Actor Task                           │  │
│   │                                                           │  │
│   │  select! {                                                │  │
│   │    event = adapter.next_event() => event_tx.send(event)   │  │
│   │    cmd = cmd_rx.recv() => match cmd { Get/Set/... }       │  │
│   │  }                                                        │  │
│   └────────┬──────────────────────────▲───────────────────────┘  │
│            │ event_tx (RawEvent)      │ cmd_tx (AdapterCommand)  │
│            ▼                          │                          │
│   ┌───────────────────────────────────┴───────────────────────┐  │
│   │              Controller::run()  select!                  │  │
│   │                                                           │  │
│   │  event_rx.recv() ──► handle_event()                       │  │
│   │    ├─ match watches, spawn script tasks                   │  │
│   │    ├─ Rhai: spawn_blocking (engine uses AdapterHandle)    │  │
│   │    └─ Shell: process::Command (semaphore + timeout)       │  │
│   │                                                           │  │
│   │  control_listener.accept() ──► spawn handle_control()     │  │
│   │    ├─ get/set/caps ───────► AdapterHandle                 │  │
│   │    └─ exec/commands/status/stop ─► Controller logic       │  │
│   │                                                           │  │
│   │  shutdown_rx / stop_rx ──► break loop                     │  │
│   └───────────────────────────────────────────────────────────┘  │
│          ▲                                                       │
└──────────┼───────────────────────────────────────────────────────┘
           │ connect / request / response (Unix socket, JSON Lines)
           ▼
┌──────────────────┐
│    msx CLI       │
│   (short-lived)  │
└──────────────────┘
```

### Shutdown Sequence

1. A Ctrl+C task in `middlesox-cli` or a control-path `stop` request sends a oneshot shutdown signal to `Controller::run()`
2. `Controller::run()` breaks its main loop
3. `Controller::run()` calls `adapter.shutdown()` via `AdapterHandle`
4. `msx run` cleans up the Unix socket file and exits

### CLI (`msx get|set|exec|caps|commands|status|stop|...`)

Short-lived process. Connects to the daemon's Unix socket, sends a single `ControlRequest` (JSON line), reads the `ControlResponse`, prints it, exits.

## Adapter Model

All backends implement `ProtocolAdapter` (async trait, `Send` only, `&mut self`):

```
ProtocolAdapter
├── MockBackend      — synthetic events for testing
├── MangoWcBackend   — dwl/MangoWC via zdwl_ipc Wayland protocol
├── SocketAdapter    — bridges external processes via Unix sockets
└── HyprlandBackend  — Hyprland via .socket.sock / .socket2.sock IPC
```

The trait:
- `subscribe(&mut self, subscriptions)` — set up event sources
- `next_event(&mut self) -> Result<Option<RawEvent>>` — yield next event (must be cancel-safe)
- `get(&mut self, key)` / `set(&mut self, key, value)` — query/command
- `init(&mut self)` / `shutdown(&mut self)` — lifecycle hooks

### Cancel-safety

`next_event()` is called inside `select!` and must be cancel-safe:

```
┌─────────────────────────────┐     ┌─────────────────────────────┐
│  mock                       │     │  socket (IPC bridge)        │
│                             │     │                             │
│  next_event() awaits        │     │  subscribe() spawns an      │
│  interval.tick()            │     │  internal reader task:      │
│  (cancel-safe by design)    │     │    loop {                   │
│                             │     │      read_line() (not safe) │
│  Then generates event       │     │      event_tx.send()        │
│  based on cycle counter     │     │    }                        │
│                             │     │                             │
│                             │     │  next_event() awaits        │
│                             │     │  mpsc::recv() (cancel-safe) │
└─────────────────────────────┘     └─────────────────────────────┘

┌─────────────────────────────┐
│  mangowc (Wayland native)   │
│                             │
│  subscribe() spawns a       │
│  blocking thread:           │
│    run_wayland_loop()       │
│      loop {                 │
│        try_recv() commands  │
│        poll(fd, 100ms)      │
│        dispatch events      │
│        event_tx.send()      │
│      }                      │
│                             │
│  next_event() awaits        │
│  mpsc::recv() (cancel-safe) │
│                             │
│  get/set send commands via  │
│  std::sync::mpsc to the     │
│  Wayland thread             │
└─────────────────────────────┘
```

## Event Flow

```
WM/Compositor
    │
    ▼
ProtocolAdapter::next_event()
    │  mock:    interval.tick() + generate event
    │  socket:  internal reader task → mpsc::recv()
    │  mangowc: blocking Wayland thread → mpsc::recv()
    │
    ▼ adapter actor forwards via mpsc channel (capacity 100)
Controller::run() select! { event_rx.recv() }
    │
    ├─ match event name against config [[watch]] entries
    ├─ check prev/curr conditions (declarative matching)
    │
    ▼
Controller::handle_event()
    │
    ├─ Rhai script: tokio::spawn + timeout (RHAI_SCRIPT_TIMEOUT)
    │    └─ runs in spawn_blocking, gets event_name/prev/curr in scope
    └─ Shell script: tokio::spawn + Semaphore + timeout (SCRIPT_TIMEOUT)
         └─ spawned as child process with JSON payload in argv[1]
            and MSX_EVENT/MSX_PREV/MSX_CURR env vars
```

## Control Flow (CLI → Daemon)

```
msx get workspace
    │
    ▼
ControlRequest::Get { key: "workspace" }
    │ JSON line over Unix socket
    ▼
Controller::handle_control()
    │
    ▼
AdapterHandle::get("workspace")
    │ sends Get command over mpsc, awaits oneshot reply
    ▼
adapter.get("workspace")
    │
    ▼
ControlResponse { success: true, result: 1 }
    │ JSON line back
    ▼
CLI prints "1"
```

## Security Model

Capability-based enforcement is intentionally narrow:

- Each adapter declares a `CapabilityManifest` — list of keys with `ReadOnly` or `ReadWrite` access
- Before any `set(key, value)` from a Rhai script, the engine fetches the manifest and checks it
- Before any CLI `set(key, value)`, the controller also fetches the manifest and rejects unknown or read-only keys before calling the adapter
- Read-only or unknown keys reject Rhai writes at the engine level, before reaching the adapter
- CLI `get` and controller-local requests (`exec`, `commands`, `status`, `stop`) do not use capability enforcement
- Shell scripts are ordinary subprocesses. They are not sandboxed by the capability system and should not be described as a security boundary

## Script Execution

Two execution modes exist:

- **Rhai** (`.rhai`) — runs in-process on a blocking worker thread, with `event_name`, `prev`, `curr`, `get()`, and capability-checked `set()`
- **Shell / executable** (all other paths) — spawned as a subprocess with:
  - `argv[1]`: JSON payload like `{"event":"workspace_change","prev":{...},"curr":{...}}`
  - `MSX_EVENT`, `MSX_PREV`, `MSX_CURR`: duplicated event context in environment variables

For `msx exec` shell commands, the payload is `{"event":"command","prev":null,"curr":null}`.

`.sh` files must include a shebang because the controller executes them directly rather than through a shell.

## Configuration

TOML config discovery order:

1. `./middlesox.toml`
2. `~/.config/middlesox/config.toml`
3. `/etc/middlesox/config.toml`

If none of those files exists, `msx` falls back to `Config::default_config()`, which currently selects the `mock` adapter and installs a default `workspace_change -> on_workspace_change.rhai` watch.

Relative `scripts_dir` values are resolved relative to the config file's directory. Absolute paths are used as-is, and `~/...` is expanded by the CLI before script execution.

```toml
[settings]
scripts_dir = "scripts"   # relative to the config file directory
log_level = "info"

[adapter]
name = "mock"          # mock | mangowc | socket | hyprland

[[watch]]
event = "workspace_change"
exec = "on_workspace_change.rhai"
# optional declarative conditions:
# prev = { id = 1 }
# curr = { id = 2 }

[[command]]
name = "cycle_layout"
script = "cycle_layout.rhai"
```

## Workspace Layout

| Directory | Cargo package | Purpose |
|-----------|---------------|---------|
| `middlesox` | `middlesox` | Core library: traits, adapter handle, config, engine, controller, control protocol |
| `middlesox-cli` | `middlesox-cli` | CLI binary (`msx`) and daemon entry point |
| `middlesox-tests` | `middlesox-tests` | Integration test harness |
| `mock` | `middlesox-mock` | Mock adapter for testing |
| `mangowc` | `middlesox-mangowc` | MangoWC/dwl Wayland adapter |
| `socket` | `middlesox-socket` | Socket-based external bridge adapter |
| `hyprland` | `middlesox-hyprland` | Hyprland adapter using native socket IPC |
