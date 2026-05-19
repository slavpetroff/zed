# Devcontainer DAP — Adapter stdin Lifeline (replace the /proc reaper)

**Date:** 2026-05-19
**Branch:** `sp/devcontainer-dap`
**Supersedes:** the interim `/proc`-scanning reaper in `crates/docker_proxy/src/docker_proxy.rs::build_bridge_command`
**Related:** [#49924](https://github.com/zed-industries/zed/issues/49924), `docs/superpowers/specs/2026-05-18-devcontainer-dap-design.md`

## Problem

Debugging in a devcontainer routes the DAP connection through a `zed --docker-proxy`
sidecar. The in-container debug adapter (e.g. `python -m debugpy --listen
127.0.0.1:5678 …`) is launched by its own `docker exec`. `docker exec` does not
forward signals to the in-container process when the local client is killed, so
on `Transport::kill()` the in-container adapter survives and keeps the DAP listen
port bound. The next session then fails with *"Address already in use"*.

An interim fix added a reaper to the proxy bridge: on bridge-exit it scanned
`/proc` and `kill -9`'d any process whose cmdline matched
`*debugpy.adapter*<remote_port>*`. This is fundamentally wrong, not merely
imperfect:

- **Identity by reused port.** A session restart reuses the same
  `DebugAdapterBinary`, so the replacement adapter listens on the *same* port
  with a byte-identical cmdline. The old session's bridge-exit reaper matches
  and kills the **new** session's adapter, producing `Server is not available`
  and `"attach" expected` on the restarted session (observed
  2026-05-19 in a Django/debugpy devcontainer).
- **Cross-process coupling.** A component (the proxy bridge) reaches across
  process boundaries to kill a process it does not own, identified by a
  heuristic string match. There is no ownership, so there is no correct scope.

A snapshot-at-connect mitigation was applied to stop the immediate cross-kill,
but the architecture — one process guessing which unrelated process to kill —
remains wrong. This spec replaces it.

## Decisions

| # | Decision | Rationale |
|---|----------|-----------|
| D1 | **Full replacement.** Revert the reaper *and* the snapshot mitigation; `docker_proxy.rs` returns to a pure byte-pump (retaining only the connect-retry loop and the half-close/no-`wait` race fix). The adapter lifeline is the single authoritative mechanism. | One mechanism, one owner, one place to reason about. No cross-process identity guessing anywhere. |
| D2 | **Adapter owns its own lifetime via its `docker exec` stdin.** The adapter is wrapped in a minimal POSIX `sh` that, on stdin-EOF, signals the exact PID it launched. | Intrinsic identity (`$!` in its own exec) — restart-safe by construction, no scan, no port match. Extends the stdin-EOF lifetime pattern the bridge in this crate already ships. |
| D3 | **Debuggee teardown is delegated to the adapter, not to `setsid`/process-group kill.** We `SIGTERM` the adapter PID; debugpy terminates the debuggee it launched (documented debugpy behavior). | Removes the fragile, non-POSIX, util-linux-dependent group-kill. Q2=B (no orphaned debuggee on the app port across restarts) is satisfied by the adapter's own contract. |
| D4 | **`/bin/sh` in the target container is an explicit, surfaced contract.** If absent, the adapter `docker exec` fails and `TcpTransport::start` propagates a specific, actionable error to the debug console. | Per Zed `CLAUDE.md`: errors propagate to the UI with meaningful feedback; never silently degrade. Every image that can run the existing `bash -c` bridge or host a debuggable toolchain has `/bin/sh`. |
| D5 | **Scope strictly to `PortForwardingMode::Separate` (docker).** SSH (`Inline`), WSL (`SharedInterface`), and local are untouched; their adapter stdin stays `Stdio::null()`. | No behavior change outside the docker DAP path. |

## Chosen Approach

`sh` lifeline wrapper applied at the single docker-DAP injection point, plus a
piped adapter stdin in `TcpTransport`, plus reverting `docker_proxy.rs` to a
pure pump. (Approaches considered and rejected: baking the lifeline into the
generic `build_command` — pollutes a unit shared by LSP/tasks/terminals; a
separate `docker exec` kill on `kill()` — that is the reaper relocated, still
cross-process and port-identity-racy.)

## Architecture & Components

Principle: the in-container adapter's lifetime is bound to the stdin of *its
own* `docker exec`. Docker closes the in-container stdin stream when the local
exec client dies or its stdin is closed (already relied upon and documented in
`docker_proxy.rs`). The wrapper, on EOF, signals only the PID it itself
launched. No other process reaps it; identity is intrinsic, so restarts cannot
cross-kill.

### Changed units

1. **New pure function `dap::adapters::wrap_with_stdin_lifeline(program: &str,
   args: &[String]) -> (String, Vec<String>)`** in `crates/dap/src/adapters.rs`,
   beside `DebugAdapterBinary`. Returns `("sh".into(), ["-c", LIFELINE, "sh",
   program, args…])`. Single responsibility; unit-testable with no process or
   container. `LIFELINE` is a private `const &str` in the same module.

2. **`crates/project/src/debugger/dap_store.rs`, `PortForwardingMode::Separate`
   branch (~line 354).** The sole caller. Before `build_command_with_options`,
   replace `(binary.command, binary.arguments)` with
   `wrap_with_stdin_lifeline(...)`. `binary.command == None` (no program; shell
   adapter) is passed through unwrapped — the lifeline only applies when there
   is an explicit adapter program. Gated strictly to `Separate`.

3. **`crates/dap/src/transport.rs`, `TcpTransport`.** When
   `binary.port_forward_command.is_some()` (⇔ docker DAP), spawn the adapter
   with `Stdio::piped()` stdin instead of `Stdio::null()`, and retain the
   `ChildStdin` in a new field `adapter_stdin: Option<ChildStdin>`. `kill()` and
   `Drop` take + drop `adapter_stdin` **first** (→ in-container EOF → wrapper
   reaps its child), then kill the local process and the sidecar exactly as
   today. `port_forward_command.is_none()` keeps `Stdio::null()` and a `None`
   handle — no behavior change for non-docker.

4. **`crates/docker_proxy/src/docker_proxy.rs` revert.**
   `build_bridge_command` drops `reap()`, the `targets` snapshot, and the
   `trap … EXIT`; it returns to:
   `i=0; until exec 3<>/dev/tcp/{host}/{port} 2>/dev/null; do i=$((i+1)); [ $i -ge 100 ] && exit 1; sleep 0.1; done; cat <&3 & bg=$!; cat >&3`
   (connect-retry loop and half-close/no-`wait` race fix retained). The doc
   comment is rewritten to state that adapter lifetime is owned by the adapter's
   own `docker exec` stdin lifeline (cross-referencing this spec), and that the
   bridge is a pure byte-pump.

## The Lifeline Shell

`LIFELINE` (a single-quoted `const`, run as `sh -c '<LIFELINE>' sh <program>
<args…>` so argv is positional — zero interpolation, zero injection surface):

```sh
"$@" &
child=$!
trap 'kill -TERM "$child" 2>/dev/null' EXIT
cat >/dev/null 2>&1
kill -TERM "$child" 2>/dev/null
i=0; while kill -0 "$child" 2>/dev/null && [ $i -lt 20 ]; do i=$((i+1)); sleep 0.1; done
kill -KILL "$child" 2>/dev/null
```

- Backgrounds the real adapter; `child=$!` is its PID.
- `cat >/dev/null` blocks on the exec's stdin; docker closes that stream on
  client death or explicit `ChildStdin` drop → `cat` returns EOF.
- `SIGTERM` the adapter (debugpy then terminates its own debuggee — D3);
  bounded ≤2 s grace polling `kill -0`; then `SIGKILL`.
- `trap … EXIT` is the backstop if the wrapper `sh` itself is killed.
- Constructs are POSIX-only and identical across dash/ash/bash: backgrounding,
  `$!`, `trap`, `kill`, `[ ]`, `while`, `$((…))`. `sleep 0.1` matches the
  fractional-sleep assumption the shipped bridge already depends on.

## Data Flow — Restart (previously broken path)

```
Client 0 running → user restarts
  └ debugger_panel::handle_restart_request → dap_store.shutdown_session(0)
      └ TcpTransport::kill():
          1. take + drop adapter ChildStdin   → docker closes in-container stdin
          2. kill local docker-exec client
          3. kill port-forward sidecar
  └ in-container wrapper: cat hits EOF
      → SIGTERM child (debugpy) → debugpy frees :5678 AND terminates Django (:8001)
      → ≤2 s grace → SIGKILL backstop
  └ restart boots Client 1 with the same binary:
      - new sh-lifeline + new debugpy adapter on :5678 (now free)
      - new sidecar, new bridge (pure pump, no reaper)
  └ Client 1: initialize → launch → configurationDone → threads → running ✓
```

The old session's cleanup signals only the PID *it* launched, inside *its own*
`docker exec`. The restart's adapter is a different PID in a different exec —
structurally impossible for the old wrapper to touch. The
`Server is not available` / `"attach" expected` race cannot recur.

## Error Handling & Edge Cases

| Case | Handling |
|------|----------|
| `/bin/sh` absent | Adapter `docker exec` fails; `TcpTransport::start` propagates a specific error to the debug console: *"devcontainer debugging requires /bin/sh in the target container"*. No silent degrade (D4). |
| stdin-EOF never arrives | Triple-covered: (a) `kill()` drops `ChildStdin` (fast path); (b) docker closes in-container stdin on exec-client death (backstop, documented); (c) `trap … EXIT` fires if the wrapper `sh` dies. No single point whose failure leaks the port. |
| Adapter exits cleanly first (DAP `disconnect`) | Analyzed, not hand-waved: `cat` lingers on open stdin, but debugpy already exited and freed :5678; the residual `sh`+`cat` holds no port and is reaped when `kill()` closes stdin. No child-waiter added (YAGNI) — the lingering process provably holds no resource. |
| PID reuse / cross-session kill | Structurally impossible — the wrapper signals only the `$!` captured inside its own exec. No scan, no port match, no shared identity. |
| Non-docker transports | Wrapper never applied (gated on `Separate`); adapter stdin stays `Stdio::null()`; zero behavior change. |
| Removing the proxy reaper regresses port-in-use | It cannot: the lifeline frees the port synchronously on EOF before `kill()` returns control, and the restart boots afterward (same `kill()` ordering). |

## Testing Strategy (TDD — failing test first per component)

| Test | Location | Verifies |
|------|----------|----------|
| `wrap_with_stdin_lifeline_runs_original_argv_under_sh` | `dap/src/adapters.rs` | Returns `("sh", ["-c", L, "sh", prog, args…])`; argv preserved positionally |
| `lifeline_is_posix_only_and_has_no_setsid_or_group_kill` | same | `L` has no `setsid`, no `kill … -"$child"` group form, no bashisms; has `cat >/dev/null`, `kill -TERM "$child"`, bounded grace, `kill -KILL` |
| `lifeline_arms_exit_trap_backstop` | same | `L` contains `trap '…kill -TERM "$child"…' EXIT` |
| `dap_store_separate_wraps_adapter_command` | `project/src/debugger/dap_store.rs` | `Separate` wraps; `Inline`/`SharedInterface` do **not**; `command == None` passes through unwrapped |
| `tcp_transport_pipes_adapter_stdin_when_forwarding` | `dap/src/transport.rs` | `port_forward_command.is_some()` ⇒ piped stdin handle stored; `None` ⇒ null, no handle |
| `tcp_transport_drops_adapter_stdin_on_kill` | same | `kill()`/`Drop` releases the stdin handle before killing the process |
| `tcp_transport_surfaces_sh_missing_error` | same | Adapter spawn failure yields the actionable `/bin/sh` message, not a generic error |
| `bridge_command_is_pure_pump` (replaces all reaper tests) | `docker_proxy.rs` | No `reap`/`trap`/`/proc`/`targets`; retains retry loop + `cat <&3 & bg=$!; cat >&3`; no `; wait` |

Integration (best-effort, mock `docker` that echoes argv): assert the wrapper
shape end-to-end through `dap_store` → `build_command`. No live-container test
(CI has no devcontainer); shell semantics are covered by string-invariant unit
tests, consistent with the existing `build_bridge_command` test approach.

## Out of Scope

- Windows containers (named pipes, not `/dev/tcp`) — already out of scope in the
  parent devcontainer-DAP spec.
- Non-debugpy adapters that do not terminate their debuggee on `SIGTERM`: the
  adapter PID is still reliably killed (port freed); debuggee teardown for such
  adapters is a separate, adapter-specific concern.
- Changes to the generic `build_command` path (LSP/tasks/terminals).

## Branch / PR Hygiene

Commits on `sp/devcontainer-dap` follow Zed PR hygiene (imperative title, no
conventional-commit prefix, `Release Notes:` section). Suggested release note:
`- Fixed debugger restart in devcontainers leaving the adapter port bound`.
