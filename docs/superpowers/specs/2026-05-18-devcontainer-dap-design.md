# Devcontainer DAP Complete Fix

**Date:** 2026-05-18
**Branch:** stacked on `sp/implement-color-mate` → new branch `sp/devcontainer-dap`
**Issue:** [#49924](https://github.com/zed-industries/zed/issues/49924) — debugpy doesn't work in devcontainer

## Problem

Zed's devcontainer support works for editing, LSP, and tasks, but the debugger is broken for all languages. Three root causes:

1. **DAP port forwarding silently discarded** — `DockerExecConnection::build_command` receives `_port_forward: Option<(u16, String, u16)>` and ignores it (underscore prefix). Zed allocates a host port, debugpy listens inside the container on a different port, nothing bridges them → `Connection to TCP DAP timeout`.

2. **debugpy venv creation fails in some devcontainer images** — `base_venv_path` calls `python3 -m venv` but some images (Microsoft Python devcontainer, conda-based) have a Python without the `venv` module → `Failed to create base virtual environment`.

3. **`forwardPorts` in `devcontainer.json` is parsed but never acted upon** — `build_forward_ports_command` returns `Err("Not currently supported for docker_exec")`.

## Solution Overview

Six self-contained changes:

| # | Change | File(s) |
|---|--------|---------|
| 1 | `zed docker-proxy` public subcommand | `crates/cli/src/` |
| 2 | `DockerExecConnection::build_forward_ports_command` | `crates/remote/src/transport/docker.rs` |
| 3 | `RemoteConnection` trait: `port_forwarding_mode()` | `crates/remote/src/remote_client.rs` |
| 4 | `dap_store`: route Docker through separate forwarder | `crates/project/src/debugger/dap_store.rs` |
| 5 | `DebugAdapterBinary` + `TcpTransport` sidecar | `crates/dap/src/adapters.rs`, `crates/dap/src/transport.rs` |
| 6 | debugpy venv fallback + `forwardPorts` activation | `crates/dap_adapters/src/python.rs`, `crates/dev_container/` |

## Component Designs

### 1. `zed docker-proxy` subcommand

A new **public** top-level CLI subcommand (appears in `zed --help`):

```
zed docker-proxy --docker-cli <path> --container <id> --forward <local:host:remote> [--forward ...]
```

**Behavior:**
- For each `--forward local_port:remote_host:remote_port` spec, binds a TCP listener on `127.0.0.1:local_port`
- For each accepted connection, spawns:
  ```
  docker exec -i <container> bash -c \
    'exec 3<>/dev/tcp/<remote_host>/<remote_port>; cat <&3 & cat >&3; wait'
  ```
- Proxies bytes bidirectionally using `smol::io::copy` in both directions concurrently
- Handles multiple concurrent connections per forward spec
- Handles multiple `--forward` specs simultaneously
- Runs until killed; lifecycle is managed by the spawning `TcpTransport`

**Why bash `/dev/tcp`:** Available in bash without any external packages. Works in all standard devcontainer base images (Debian, Ubuntu, Alpine with bash). No dependency on `socat`, `nc`, or `ncat`.

**Error handling:**
- Docker CLI not found at given path → exit code 1 with message to stderr
- Container not running → `docker exec` fails per-connection → connection closes, proxy continues listening
- `/dev/tcp` unsupported (musl + ash) → `docker exec` exits non-zero → connection closes cleanly
- Port already bound → exit code 1: `"docker-proxy: port <N> already in use"`

**Location:** `crates/cli/src/docker_proxy.rs` — new module, registered as a subcommand in `crates/cli/src/main.rs`.

### 2. `DockerExecConnection::build_forward_ports_command`

Replace the current `Err("Not currently supported for docker_exec")` with:

```rust
fn build_forward_ports_command(
    &self,
    forwards: Vec<(u16, String, u16)>,
) -> Result<CommandTemplate> {
    let current_exe = std::env::current_exe()
        .context("could not determine zed executable path")?;
    let mut args = vec![
        "docker-proxy".to_string(),
        "--docker-cli".to_string(),
        self.docker_cli().to_string(),
        "--container".to_string(),
        self.connection_options.container_id.clone(),
    ];
    for (local_port, remote_host, remote_port) in forwards {
        args.push("--forward".to_string());
        args.push(format!("{local_port}:{remote_host}:{remote_port}"));
    }
    Ok(CommandTemplate {
        program: current_exe.display().to_string(),
        args,
        env: Default::default(),
    })
}
```

Also: rename `_port_forward` → `port_forward` in `build_command` signature (the parameter is now intentionally unused because Docker cannot do inline forwarding, but the underscore was hiding a silent discard bug). Add a `debug_assert!(port_forward.is_none(), "Docker port forwarding must go through build_forward_ports_command")` to catch misuse.

### 3. `RemoteConnection` trait: `port_forwarding_mode()`

Add to the `RemoteConnection` trait in `remote_client.rs`:

```rust
fn port_forwarding_mode(&self) -> PortForwardingMode {
    PortForwardingMode::Inline  // default: SSH
}
```

```rust
pub enum PortForwardingMode {
    /// Port forward is embedded in the launch command (SSH: -L flag).
    Inline,
    /// Port forward requires a separate sidecar process.
    Separate,
    /// No forwarding needed; host and remote share a network interface.
    SharedInterface,
}
```

`DockerExecConnection` overrides to `Separate`. WSL returns `SharedInterface` (consistent with `shares_network_interface()` = true). SSH returns `Inline`.

The existing `shares_network_interface()` method remains unchanged and returns `true` only for WSL — it is consistent with `PortForwardingMode::SharedInterface`. Both coexist; no refactor of existing callers is needed.

### 4. `dap_store.rs`: Docker code path

In `DapStoreMode::Remote`, after receiving the binary from the remote server, detect the forwarding mode:

```rust
let forwarding_mode = remote.read_with(cx, |r, _| r.port_forwarding_mode());

let (port_forward_inline, port_forward_command) = match forwarding_mode {
    PortForwardingMode::SharedInterface => {
        // WSL: connect directly, no forwarding
        (None, None)
    }
    PortForwardingMode::Inline => {
        // SSH: bake -L into the launch command
        (port_forwarding, None)
    }
    PortForwardingMode::Separate => {
        // Docker: spawn a separate proxy process
        let cmd = remote.read_with(cx, |r, _| {
            let forwards = port_forwarding
                .map(|(lp, rh, rp)| vec![(lp, rh, rp)])
                .unwrap_or_default();
            r.build_forward_ports_command(forwards)
        })?;
        (None, Some(cmd))
    }
};

let command = remote.build_command_with_options(
    binary.command,
    &binary.arguments,
    &binary.envs,
    cwd,
    port_forward_inline,  // None for Docker
    Interactive::No,
)?;

Ok(DebugAdapterBinary {
    command: Some(command.program),
    arguments: command.args,
    envs: command.env,
    cwd: None,
    connection,
    request_args: binary.request_args,
    port_forward_command,  // new field
})
```

### 5. `DebugAdapterBinary` + `TcpTransport` sidecar

**`DebugAdapterBinary`** (`crates/dap/src/adapters.rs`): add one field:

```rust
pub struct DebugAdapterBinary {
    pub command: Option<String>,
    pub arguments: Vec<String>,
    pub envs: HashMap<String, String>,
    pub cwd: Option<PathBuf>,
    pub connection: Option<TcpArguments>,
    pub request_args: StartDebuggingRequestArguments,
    pub port_forward_command: Option<CommandTemplate>,  // NEW
}
```

This is a local host-side struct; it does not touch the proto format.

**`TcpTransport`** (`crates/dap/src/transport.rs`):

```rust
pub struct TcpTransport {
    host: IpAddr,
    port: u16,
    timeout: u64,
    executor: BackgroundExecutor,
    process: Arc<Mutex<Option<Child>>>,
    port_forward_process: Option<Child>,  // NEW — sidecar, killed in Drop
}
```

In `TcpTransport::start`:
1. If `binary.port_forward_command` is `Some(cmd)`, spawn it first and store in `port_forward_process`
2. Spawn the debug adapter process as before
3. The existing connect loop already retries every 100 ms; it will naturally succeed once the proxy has bound — no extra sleep needed

In `Drop`: kill `port_forward_process` before killing `process`.

In the connect loop: if `port_forward_process` has exited, bail with `"Docker port forwarder exited unexpectedly — check that docker is running and the container is healthy"`.

### 6. debugpy venv fallback + `forwardPorts` activation

**debugpy venv fallback** (`crates/dap_adapters/src/python.rs`):

In `base_venv_path`, after `python3 -m venv zed_base_venv` fails, try in sequence:
1. `python3 -m virtualenv zed_base_venv` (virtualenv package)
2. `uv venv zed_base_venv` (uv, common in modern devcontainers)
3. Return error listing all attempts with suggestion to `apt-get install python3-venv`

Also: after venv creation, verify `bin/python3` exists before returning the path (catches silent partial-creation failures).

**`forwardPorts` activation** (`crates/dev_container/src/devcontainer_api.rs`):

After `start_dev_container_with_config` connects and returns a `DevContainerConnection`, if the parsed `DevContainer` config has non-empty `forwardPorts`:
- For each port, call `build_forward_ports_command([(port, "127.0.0.1", port)])` via the `remote_client`
- Spawn the returned `CommandTemplate` and store the `Child` handles in `start_dev_container_with_config`'s return value alongside the `DevContainerConnection` — specifically as a `Vec<Child>` held by the caller (workspace/project) for the session lifetime
- Port forward processes are killed when their `Child` handles are dropped (when the project closes or reconnects)

## Data Flow: End-to-End for Django Debug

```
User: Run debug configuration for manage.py
  ↓
dap_store → GetDebugAdapterBinary → remote server inside container
  ↓
remote server: system_python_name() finds python3, creates venv,
               installs debugpy, returns TcpArguments{host: 127.0.0.1, port: 5678}
  ↓
dap_store: port_forwarding_mode() == Separate
  → allocates free host port: 54321
  → build_forward_ports_command([(54321, "127.0.0.1", 5678)])
    → CommandTemplate { program: "/Applications/Zed.app/.../zed",
                        args: ["docker-proxy", "--docker-cli", "docker",
                               "--container", "abc123",
                               "--forward", "54321:127.0.0.1:5678"] }
  → build_command(..., port_forward: None)
    → CommandTemplate { program: "docker",
                        args: ["exec", "-u", "dev-user", "-i", "abc123",
                               "python", "-m", "debugpy", "--listen",
                               "127.0.0.1:5678", "manage.py"] }
  ↓
TcpTransport::start(binary):
  1. spawn: zed docker-proxy --container abc123 --forward 54321:127.0.0.1:5678
  2. wait 100ms for proxy to bind
  3. spawn: docker exec -u dev-user -i abc123 python -m debugpy --listen 127.0.0.1:5678 manage.py
  4. connect loop → TcpStream::connect(127.0.0.1:54321) → succeeds
  ↓
zed docker-proxy (running):
  TCP accept on :54321
  → spawn: docker exec -i abc123 bash -c 'exec 3<>/dev/tcp/127.0.0.1/5678; cat <&3 & cat >&3; wait'
  → smol::io::copy bidirectionally: [host DAP client] ↔ [docker exec stdio] ↔ [debugpy TCP]
  ↓
DAP session established. Breakpoints, stepping, variable inspection work.
```

## Testing Strategy (TDD — write tests first)

### Unit tests

| Test | Location | Verifies |
|------|----------|---------|
| `parse_forward_spec` | `crates/cli/src/docker_proxy.rs` | `"54321:127.0.0.1:5678"` → `(54321, "127.0.0.1", 5678)` |
| `parse_forward_spec_ipv6` | same | `"54321:::1:5678"` → correct IPv6 tuple |
| `parse_forward_spec_invalid` | same | Bad input returns descriptive error |
| `build_forward_ports_command_docker` | `crates/remote/src/transport/docker.rs` | Returns `CommandTemplate` with `zed docker-proxy` as program and correct args |
| `port_forwarding_mode_docker` | `crates/remote/src/transport/docker.rs` | Returns `PortForwardingMode::Separate` |
| `port_forwarding_mode_ssh` | `crates/remote/src/transport/ssh.rs` | Returns `PortForwardingMode::Inline` |
| `port_forwarding_mode_wsl` | `crates/remote/src/transport/wsl.rs` | Returns `PortForwardingMode::SharedInterface` |
| `dap_store_docker_sets_port_forward_command` | `crates/project/src/debugger/dap_store.rs` | Mock transport with `Separate` mode → `DebugAdapterBinary.port_forward_command` is `Some` and `port_forward_inline` is `None` |
| `dap_store_ssh_inline_forwarding` | same | Mock transport with `Inline` mode → `port_forward_command` is `None` and inline forwarding used |
| `tcp_transport_drops_sidecar` | `crates/dap/src/transport.rs` | Sidecar process is killed when `TcpTransport` is dropped |
| `tcp_transport_sidecar_exit_error` | same | When sidecar exits before connect, error surfaces correctly |
| `debugpy_venv_fallback_virtualenv` | `crates/dap_adapters/src/python.rs` | When venv fails, virtualenv is tried |
| `debugpy_venv_fallback_uv` | same | When virtualenv also fails, uv is tried |
| `debugpy_venv_all_fail_error` | same | Error message lists all attempts |

### Integration tests

| Test | Location | Verifies |
|------|----------|---------|
| `docker_proxy_proxy_roundtrip` | `crates/cli/src/docker_proxy.rs` | Spawns a mock echo server on a local port; proxy forwards through a `docker exec` (or mock); bytes round-trip correctly |
| `docker_proxy_multiple_connections` | same | Two concurrent connections both succeed independently |
| `docker_proxy_connection_close` | same | When host closes connection, `docker exec` process terminates |
| `forward_ports_activated_on_connect` | `crates/dev_container/src/` | After simulated devcontainer connect, port forward processes are started for each entry in `forwardPorts` |

### Approach: TDD sequence per component

1. Write failing test
2. Write minimal implementation to pass
3. Run `./script/clippy` to verify
4. Move to next component

## Branch Structure

```
main
  └── sp/implement-color-mate   (existing, current branch)
        └── sp/devcontainer-dap  (new stacked branch)
```

All commits on `sp/devcontainer-dap` follow Zed PR hygiene (imperative title, `Release Notes:` section).

## Out of Scope

- Windows container support (Docker on Windows uses named pipes, not `/dev/tcp`)
- Auto-rebuild on `devcontainer.json` changes (separate issue)
- Extension isolation inside containers (separate issue)
- `appPort` → `forwardPorts` migration (documented limitation, separate issue)
