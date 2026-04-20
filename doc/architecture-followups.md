# Architecture Follow-ups

Tasks created from the review of `doc/architecture.md`, with adjacent README drift included where it would otherwise leave the project inconsistent.

## Phase 1: Truth-sync the docs

- [x] Fix the control-path diagram and shutdown sequence in `doc/architecture.md`.
  Scope:
  Update the text and diagram so they match the current split between adapter-backed requests (`get`, `set`, `caps`) and controller-local requests (`exec`, `commands`, `status`, `stop`).
  Update the shutdown sequence to show that `stop` is translated into a controller shutdown signal after the response is written, and that socket cleanup happens in `middlesox-cli`, not in `Controller::run()`.
  Targets:
  `doc/architecture.md`, `middlesox/src/controller.rs`, `middlesox-cli/src/main.rs`, `middlesox/src/control.rs`
  Acceptance:
  A reader can trace each CLI command to the right handler without inferring that everything goes through `AdapterHandle`.

- [x] Narrow and clarify the security model in `doc/architecture.md`.
  Scope:
  Describe the real enforcement boundary: Rhai `set()` checks capabilities before delegating to the adapter, while CLI requests and shell scripts are not sandboxed by the capability system.
  Targets:
  `doc/architecture.md`, `middlesox/src/engine.rs`, `middlesox/src/controller.rs`
  Acceptance:
  The document no longer implies that capabilities protect the entire daemon or shell-script surface.

- [x] Document the full shell-script contract.
  Scope:
  Explicitly document that event-triggered and command-triggered shell scripts receive both `argv[1]` JSON and `MSX_EVENT` / `MSX_PREV` / `MSX_CURR`.
  Mention that `.sh` files must have a shebang.
  Targets:
  `doc/architecture.md`, `README.md`, `example/scripts/layout_notify.sh`, `middlesox/src/controller.rs`
  Acceptance:
  The shell interface described in docs matches the executable contract used by the controller and examples.

- [x] Document config fallback behavior, not just config discovery.
  Scope:
  State that the CLI searches `./middlesox.toml`, then the user config, then the system config, and if none exists it falls back to `Config::default_config()`.
  Document what that default currently enables: the mock adapter and the default workspace-change watch.
  Targets:
  `doc/architecture.md`, `README.md`, `middlesox/src/config.rs`, `middlesox-cli/src/main.rs`
  Acceptance:
  A user can predict startup behavior with and without a config file.

- [x] Fix naming in the crate/package layout section.
  Scope:
  Decide whether the table is describing workspace directories or Cargo package names, then label it accordingly.
  If package names are shown, use `middlesox-mock`, `middlesox-mangowc`, `middlesox-socket`, and `middlesox-hyprland`.
  Targets:
  `doc/architecture.md`, `Cargo.toml`, crate `Cargo.toml` files
  Acceptance:
  The layout section no longer mixes directory names and package names.

- [x] Bring `README.md` up to the current adapter API.
  Scope:
  Replace the old `listen(&self, tx, subs)` example with the current `subscribe(&mut self, subscriptions)` and `next_event(&mut self)` model.
  Recheck README claims about backend status, config fallback, and script behavior while touching the file.
  Targets:
  `README.md`, `middlesox/src/adapter.rs`, `middlesox/src/lib.rs`
  Acceptance:
  A backend author following the README would implement the current trait, not a removed one.

- [ ] Document the distinct script timeouts.
  Scope:
  Spell out the current timeout constants instead of saying only that scripts have a timeout: event-triggered shell scripts use `SCRIPT_TIMEOUT` at 30s, event-triggered Rhai scripts use `RHAI_SCRIPT_TIMEOUT` at 30s, and synchronous `msx exec` shell scripts use `EXEC_TIMEOUT` at 60s.
  Note that synchronous Rhai `msx exec` execution does not currently use `EXEC_TIMEOUT`.
  Targets:
  `doc/architecture.md`, `README.md`, `middlesox/src/controller.rs`
  Acceptance:
  A reader can tell which execution path is bounded by which timeout without reading `controller.rs`.

- [ ] Document the script concurrency cap.
  Scope:
  Add the `MAX_CONCURRENT_SCRIPTS = 8` semaphore limit to the Script Execution section.
  Be explicit that the shared semaphore gates event-triggered Rhai and shell script tasks, not synchronous `msx exec` execution.
  Targets:
  `doc/architecture.md`, `README.md`, `middlesox/src/controller.rs`
  Acceptance:
  The docs describe both the timeout and concurrency behavior that applies during event storms.

- [ ] Document adapter channel capacities on both sides.
  Scope:
  Keep the documented adapter event channel capacity of 100 and add the command channel capacity of 32 used by `AdapterHandle::spawn()`.
  Place the details near the adapter actor/process model so the event and command paths are described together.
  Targets:
  `doc/architecture.md`, `middlesox/src/adapter_handle.rs`
  Acceptance:
  The architecture doc no longer documents only the event-side mpsc capacity while omitting the command-side capacity.

- [ ] Clarify the two shutdown signal paths.
  Scope:
  Update the shutdown diagram and sequence to distinguish external `shutdown_rx` from Ctrl+C/test harness shutdown and `control_shutdown_rx` from a control-path `stop` request.
  Preserve the detail that `stop` sends its response before triggering the control shutdown channel.
  Targets:
  `doc/architecture.md`, `middlesox/src/controller.rs`, `middlesox-cli/src/main.rs`
  Acceptance:
  The shutdown sequence reflects the two oneshot receivers selected by `Controller::run()` rather than flattening them into one path.

- [ ] Document enforced shell shebang validation.
  Scope:
  Keep the existing requirement that `.sh` files include a shebang, and add that the controller checks this before execution.
  Mention the difference in behavior: event-triggered scripts log an error and return, while `msx exec` returns an error response.
  Targets:
  `doc/architecture.md`, `README.md`, `middlesox/src/controller.rs`
  Acceptance:
  Script authors know the `.sh` shebang rule is enforced by Middlesox, not just recommended.

## Phase 2: Architecture decisions

- [ ] Decide whether the docs should describe the current split control path, or whether the implementation should be changed to match a unified control model.
  Options:
  Keep the current design and document it precisely.
  Or introduce a new internal control service abstraction so all CLI methods flow through one path.
  Targets:
  `doc/architecture.md`, `middlesox/src/controller.rs`, `middlesox/src/adapter_handle.rs`, `middlesox/src/control.rs`
  Acceptance:
  There is one endorsed control-flow model, and both code and docs follow it.

- [ ] Decide what security claim Middlesox wants to make.
  Options:
  Documentation-only narrowing: capabilities protect Rhai writes only.
  Stronger enforcement: add checks for CLI `set`, and if strict security is a goal, either sandbox or de-scope shell scripts because arbitrary subprocesses are not compatible with the current claim.
  Targets:
  `doc/architecture.md`, `README.md`, `middlesox/src/engine.rs`, `middlesox/src/controller.rs`
  Acceptance:
  The project’s public security statement is accurate and testable.

- [ ] Decide whether the no-config default should stay enabled.
  Options:
  Keep `Config::default_config()` and document it as a development-friendly default.
  Or fail fast when no config file is present and require explicit setup for production use.
  Targets:
  `middlesox-cli/src/main.rs`, `middlesox/src/config.rs`, `README.md`, `doc/architecture.md`
  Acceptance:
  Startup behavior without a config file is an intentional product decision, not an undocumented fallback.

- [ ] Decide whether the shell-script API should stay dual-mode.
  Options:
  Keep both `argv[1]` JSON and environment variables.
  Or standardize on one interface and remove the other from code, docs, and examples.
  Targets:
  `middlesox/src/controller.rs`, `README.md`, `doc/architecture.md`, `example/scripts/`
  Acceptance:
  Script authors have one stable contract to rely on.

## Phase 3: Implementation tasks if the decisions favor code changes

- [x] If CLI writes should respect capabilities, add capability validation on `ControlRequest::Set`.
  Scope:
  Fetch the manifest before delegating `set`, reject writes to unknown or read-only keys, and add integration tests for both allowed and denied cases.
  Targets:
  `middlesox/src/controller.rs`, `middlesox-tests/tests/get_set.rs`
  Acceptance:
  CLI `set` behavior is consistent with the chosen security model.

- [ ] If the control path should be unified, introduce a dedicated request-handling layer.
  Scope:
  Move command classification into an explicit service or enum-based dispatcher instead of describing it as adapter-only.
  Preserve the current adapter actor boundary for backend access.
  Targets:
  `middlesox/src/controller.rs`, `middlesox/src/adapter_handle.rs`, `middlesox/src/control.rs`
  Acceptance:
  The code structure matches the final architecture diagram without hidden exceptions.

- [ ] Add regression tests for the chosen architectural contract.
  Scope:
  Cover stop/shutdown behavior, status handling, command execution, and any new capability checks.
  Add at least one documentation-driven test case where a shell command relies on the documented event payload contract.
  Targets:
  `middlesox-tests/tests/daemon.rs`, `middlesox-tests/tests/get_set.rs`, new integration tests as needed
  Acceptance:
  The most important architecture claims are enforced by tests, not just prose.

## Recommended order

1. Complete all Phase 1 doc truth-sync work.
2. Make the Phase 2 decisions explicitly.
3. Only then implement the Phase 3 code changes that remain necessary.
