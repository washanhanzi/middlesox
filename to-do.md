# Middlesox Fix Plan - COMPLETED

All issues have been fixed. This document summarizes what was done.

---

## Issue 1 (High): Socket adapter capabilities - FIXED

### Problem
`ScriptEngine::new()` snapshots `adapter.manifest()` once during construction, but for `SocketAdapter` the manifest is empty until async `fetch_capabilities()` runs.

### Fix Applied
1. Changed `ScriptEngine::manifest` to `Arc<RwLock<CapabilityManifest>>` for dynamic updates
2. Added `with_shared_adapter()` constructor for sharing adapters
3. Added `refresh_manifest()` method to refresh capabilities after async fetch
4. Added capability priming for socket adapters in daemon startup
5. Files modified: `middlesox/src/engine/scripting.rs`

---

## Issue 2 (High): Dual backend instances - FIXED

### Problem
The daemon created two separate backend instances - one for `ScriptEngine` and one for event listening - causing state inconsistency.

### Fix Applied
1. Added `shared_adapter()` method to `ScriptEngine` to get the shared adapter reference
2. Updated daemon to use single backend instance shared via `Arc<RwLock<BoxedAdapter>>`
3. Updated test harness to use same pattern
4. Files modified: `middlesox/src/engine/scripting.rs`, `middlesox-cli/src/main.rs`, `middlesox-tests/src/lib.rs`

---

## Issue 3 (Medium): `msx exec` only runs Rhai files - FIXED

### Problem
`ControlRequest::Exec` handler only called `engine.execute_file()` which only works for Rhai scripts, and didn't support absolute paths.

### Fix Applied
1. Added `resolve_script_path()` helper for consistent path handling
2. Added `execute_script()` method that handles both Rhai and shell scripts
3. Added `execute_shell_script()` method for synchronous shell execution
4. Updated `ControlRequest::Exec` handler to use new methods
5. Files modified: `middlesox-cli/src/main.rs`

---

## Issue 4 (Medium): Shell scripts missing environment variables - FIXED

### Problem
Shell scripts only received event context via `$1` JSON argument, not via environment variables.

### Fix Applied
1. Added `MSX_EVENT`, `MSX_PREV`, `MSX_CURR` environment variables when spawning shell scripts
2. Updated both `run_shell_script()` and `execute_shell_script()` methods
3. Updated README to document both methods (positional arg and env vars)
4. Files modified: `middlesox-cli/src/main.rs`, `README.md`

---

## Issue 5 (Medium): Config discovery/paths don't match docs - FIXED

### Problem
1. `find_config_path()` didn't check `./middlesox.toml`
2. `scripts_dir` had no tilde expansion

### Fix Applied
1. Added `./middlesox.toml` as first candidate in `find_config_path()`
2. Added `expand_path()` function for tilde expansion
3. Applied expansion to `scripts_dir` in daemon startup
4. Updated README to document config discovery order
5. Files modified: `middlesox-cli/src/main.rs`, `README.md`

---

## Issue 6 (Low): README shows invalid config entries - VERIFIED OK

### Status
No invalid entries found. The reported `[[watch_with_script]]` and `.rhaij` entries don't exist in the current README.

---

## Summary of Changes

### Files Modified
- `middlesox/src/engine/scripting.rs` - Shared adapter support, dynamic manifest
- `middlesox-cli/src/main.rs` - Config discovery, path expansion, shell script support
- `middlesox-tests/src/lib.rs` - Shared adapter in test harness
- `README.md` - Documentation updates

### All Tests Pass
```
test result: ok. 33 passed; 0 failed
```
