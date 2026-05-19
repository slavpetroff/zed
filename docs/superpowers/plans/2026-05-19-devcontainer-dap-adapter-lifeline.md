# Devcontainer DAP Adapter stdin Lifeline Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the cross-process `/proc`-scanning reaper with a per-adapter stdin-EOF lifeline owned by the adapter's own `docker exec`, so a debugger restart in a devcontainer never cross-kills the new session.

**Architecture:** The in-container debug adapter is wrapped in a minimal POSIX `sh` that backgrounds the real adapter, blocks reading the exec's stdin, and `SIGTERM`/`SIGKILL`s exactly the PID it launched when stdin reaches EOF. `TcpTransport` holds the docker adapter's `docker exec` stdin open and drops it on `kill()`/`Drop` to trigger that EOF. The proxy bridge reverts to a pure byte-pump. Scope is strictly the docker (`PortForwardingMode::Separate`) DAP path.

**Tech Stack:** Rust, GPUI test harness, `smol::process`, `util::process::Child`, bash/POSIX `sh` inside containers.

**Spec:** `docs/superpowers/specs/2026-05-19-devcontainer-dap-adapter-lifeline-design.md`

---

## File Structure

| File | Responsibility | Change |
|------|----------------|--------|
| `crates/dap/src/adapters.rs` | DAP adapter binary types | **Add** pure `wrap_with_stdin_lifeline` + private `LIFELINE` const + tests |
| `crates/docker_proxy/src/docker_proxy.rs` | Host-side TCP↔docker-exec byte pump | **Revert** `build_bridge_command` to a pure pump; replace reaper tests |
| `crates/dap/src/transport.rs` | DAP transports incl. `TcpTransport` | **Modify** struct + `start()` + `kill()` + `Drop` + `connect()`; add tests |
| `crates/project/src/debugger/dap_store.rs` | Builds the `DebugAdapterBinary` for docker | **Modify** `Separate` branch to wrap; add pure decision helper + tests |

Tasks are ordered so each leaves the tree compiling and green. Task 1 (pure fn) and Task 2 (proxy revert) are independent. Task 5 depends on Task 1.

---

## Task 1: Add the stdin-lifeline wrapper (pure function)

**Files:**
- Modify: `crates/dap/src/adapters.rs` (add function + const after the `impl DebugAdapterBinary` block, ~line 233; add tests in the existing `#[cfg(test)] mod tests`, ~line 490+)

- [ ] **Step 1: Write the failing tests**

Add to the existing `#[cfg(test)] mod tests` in `crates/dap/src/adapters.rs`:

```rust
    #[test]
    fn wrap_with_stdin_lifeline_runs_original_argv_under_sh() {
        let (program, args) = wrap_with_stdin_lifeline(
            "python",
            &["-m".to_string(), "debugpy".to_string(), "--listen".to_string()],
        );
        assert_eq!(program, "sh");
        assert_eq!(args[0], "-c");
        // args[1] is the script; args[2] is $0 for `sh -c`.
        assert_eq!(args[2], "sh");
        assert_eq!(args[3], "python");
        assert_eq!(
            &args[4..],
            &["-m".to_string(), "debugpy".to_string(), "--listen".to_string()]
        );
    }

    #[test]
    fn lifeline_is_posix_only_and_has_no_setsid_or_group_kill() {
        let (_, args) = wrap_with_stdin_lifeline("p", &[]);
        let script = &args[1];
        assert!(!script.contains("setsid"), "no setsid dependency: {script}");
        assert!(
            !script.contains("kill -TERM -\"") && !script.contains("kill -KILL -\""),
            "no negative-PID process-group kill: {script}"
        );
        assert!(script.contains("cat >/dev/null"), "{script}");
        assert!(script.contains("kill -TERM \"$child\""), "{script}");
        assert!(script.contains("kill -KILL \"$child\""), "{script}");
        assert!(script.contains("[ $i -lt 20 ]"), "bounded grace: {script}");
    }

    #[test]
    fn lifeline_arms_exit_trap_backstop() {
        let (_, args) = wrap_with_stdin_lifeline("p", &[]);
        assert!(
            args[1].contains("trap 'kill -TERM \"$child\" 2>/dev/null' EXIT"),
            "{}",
            args[1]
        );
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p dap wrap_with_stdin_lifeline lifeline_ 2>&1 | tail -15`
Expected: FAIL — `cannot find function 'wrap_with_stdin_lifeline' in this scope`.

- [ ] **Step 3: Write the implementation**

Add to `crates/dap/src/adapters.rs` immediately after the closing `}` of the `impl DebugAdapterBinary { … }` block (around line 233):

```rust
/// Wraps a debug-adapter command so the in-container process reaps itself when
/// its `docker exec` stdin reaches EOF.
///
/// `docker exec` does not forward signals to the in-container process when the
/// local client is killed, but docker *does* close the in-container stdin
/// stream. We run the real adapter under a tiny POSIX `sh` that blocks reading
/// stdin; on EOF it `SIGTERM`s (then `SIGKILL`s) exactly the PID it launched.
/// debugpy terminates its own debuggee on `SIGTERM`, so no process-group kill
/// or `setsid` dependency is needed. Identity is intrinsic (`$!` inside this
/// exec), so a session restart that reuses the same port cannot be cross-killed.
///
/// Invoked as `sh -c <LIFELINE> sh <program> <args…>` — argv is positional, so
/// there is no shell-quoting or injection surface.
pub fn wrap_with_stdin_lifeline(program: &str, args: &[String]) -> (String, Vec<String>) {
    let mut wrapped = Vec::with_capacity(args.len() + 4);
    wrapped.push("-c".to_string());
    wrapped.push(LIFELINE.to_string());
    wrapped.push("sh".to_string());
    wrapped.push(program.to_string());
    wrapped.extend(args.iter().cloned());
    ("sh".to_string(), wrapped)
}

const LIFELINE: &str = concat!(
    "\"$@\" &\n",
    "child=$!\n",
    "trap 'kill -TERM \"$child\" 2>/dev/null' EXIT\n",
    "cat >/dev/null 2>&1\n",
    "kill -TERM \"$child\" 2>/dev/null\n",
    "i=0; while kill -0 \"$child\" 2>/dev/null && [ $i -lt 20 ]; ",
    "do i=$((i+1)); sleep 0.1; done\n",
    "kill -KILL \"$child\" 2>/dev/null\n",
);
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p dap wrap_with_stdin_lifeline lifeline_ 2>&1 | tail -15`
Expected: PASS — 3 passed.

- [ ] **Step 5: Clippy**

Run: `./script/clippy -p dap 2>&1 | tail -5`
Expected: Finishes with no warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/dap/src/adapters.rs
git commit -m "$(cat <<'EOF'
dap: Add stdin-EOF lifeline wrapper for containerized adapters

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Revert the proxy bridge to a pure byte-pump

**Files:**
- Modify: `crates/docker_proxy/src/docker_proxy.rs` — replace the doc comment + `build_bridge_command` (lines ~89–115); replace reaper tests with one pure-pump test (lines ~190–280)

- [ ] **Step 1: Replace the reaper tests with the pure-pump test**

In `crates/docker_proxy/src/docker_proxy.rs`, delete these three test functions entirely:
`bridge_command_reaps_adapter_scoped_to_remote_port_on_exit`,
`bridge_command_does_not_block_on_socket_side_after_stdin_eof`,
`bridge_reaper_only_kills_adapters_present_when_this_bridge_connected`.

Keep `bridge_command_retries_until_remote_port_is_ready` and all `parse_forward_spec` tests. Add this test in their place (inside `mod tests`):

```rust
    #[test]
    fn bridge_command_is_pure_pump() {
        let cmd = build_bridge_command("127.0.0.1", 5678);
        // No reaper machinery of any kind — adapter lifetime is owned by the
        // adapter's own docker exec stdin lifeline, not the bridge.
        assert!(!cmd.contains("reap"), "no reaper: {cmd}");
        assert!(!cmd.contains("trap"), "no trap: {cmd}");
        assert!(!cmd.contains("/proc"), "no /proc scan: {cmd}");
        assert!(!cmd.contains("targets"), "no pid snapshot: {cmd}");
        assert!(!cmd.contains("kill"), "bridge must not kill anything: {cmd}");
        // Retains the bounded connect-retry loop.
        assert!(
            cmd.contains("until exec 3<>/dev/tcp/127.0.0.1/5678 2>/dev/null"),
            "{cmd}"
        );
        assert!(cmd.contains("[ $i -ge 100 ] && exit 1"), "{cmd}");
        // Half-close fix: background socket->stdout, foreground stdin->socket,
        // never `wait` (which blocks forever on the never-closing socket cat).
        assert!(cmd.contains("cat <&3 & cat >&3"), "{cmd}");
        assert!(!cmd.contains("; wait"), "must not block on wait: {cmd}");
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p docker_proxy bridge_command_is_pure_pump 2>&1 | tail -15`
Expected: FAIL — assertion `no reaper` (current `build_bridge_command` still contains `reap`).

- [ ] **Step 3: Replace the doc comment and `build_bridge_command`**

Replace the entire doc comment block (lines starting `// `docker exec` does not forward signals…`) **and** the `fn build_bridge_command` body with:

```rust
// The bridge is a pure byte-pump. Adapter lifetime is owned by the adapter's
// own `docker exec` stdin lifeline (see
// docs/superpowers/specs/2026-05-19-devcontainer-dap-adapter-lifeline-design.md):
// the in-container adapter reaps itself on stdin-EOF, so the bridge never kills
// anything. It keeps a bounded connect-retry loop because the proxy may accept
// the local TCP connection slightly before the in-container adapter has bound
// its port. It drives its lifetime off stdin-EOF (`cat >&3` returns when Zed
// disconnects) rather than `wait`, which would block forever on the
// never-closing socket-side `cat`.
fn build_bridge_command(remote_host: &str, remote_port: u16) -> String {
    format!(
        "i=0; until exec 3<>/dev/tcp/{remote_host}/{remote_port} 2>/dev/null; \
         do i=$((i+1)); [ $i -ge 100 ] && exit 1; sleep 0.1; done; \
         cat <&3 & cat >&3"
    )
}
```

- [ ] **Step 4: Run the full crate suite to verify green**

Run: `cargo test -p docker_proxy 2>&1 | tail -15`
Expected: PASS — `bridge_command_is_pure_pump`, `bridge_command_retries_until_remote_port_is_ready`, and the `parse_forward_spec` tests all pass; no reaper tests remain.

- [ ] **Step 5: Clippy**

Run: `./script/clippy -p docker_proxy 2>&1 | tail -5`
Expected: Finishes with no warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/docker_proxy/src/docker_proxy.rs
git commit -m "$(cat <<'EOF'
docker_proxy: Revert bridge to a pure byte-pump

Adapter lifetime is now owned by the adapter's own docker exec stdin
lifeline, so the bridge no longer scans /proc or kills anything.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Hold the docker adapter's stdin open in `TcpTransport`

**Files:**
- Modify: `crates/dap/src/transport.rs` — `struct TcpTransport` (~472), `start()` (~529–582), `kill()` (~591–598), `Drop` (~658–667), test accessor (~1042–1047), tests (~1122)

- [ ] **Step 1: Write the failing tests**

In `crates/dap/src/transport.rs`, extend the `#[cfg(test)] impl TcpTransport` block (lines ~1042–1047) to add an accessor:

```rust
#[cfg(test)]
impl TcpTransport {
    fn has_port_forward_sidecar(&self) -> bool {
        self.port_forward_process.is_some()
    }

    fn has_adapter_stdin(&self) -> bool {
        self.adapter_stdin.is_some()
    }
}
```

Then add to `mod tests` (after `tcp_transport_no_sidecar_when_port_forward_command_is_none`):

```rust
    fn binary_with_command_and_sidecar(
        command: Option<String>,
        port_forward_command: Option<CommandTemplate>,
    ) -> DebugAdapterBinary {
        DebugAdapterBinary {
            command,
            arguments: vec![],
            envs: Default::default(),
            cwd: None,
            connection: Some(TcpArguments {
                host: IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                port: 9999,
                timeout: Some(100),
            }),
            request_args: StartDebuggingRequestArguments {
                configuration: serde_json::Value::Null,
                request: StartDebuggingRequestArgumentsRequest::Attach,
            },
            port_forward_command,
        }
    }

    #[gpui::test]
    async fn tcp_transport_pipes_adapter_stdin_when_forwarding(cx: &mut TestAppContext) {
        let binary = binary_with_command_and_sidecar(
            Some("/bin/cat".to_string()),
            Some(CommandTemplate {
                program: "/bin/sh".to_string(),
                args: vec!["-c".to_string(), "".to_string()],
                env: Default::default(),
            }),
        );

        let mut async_cx = cx.to_async();
        let mut transport = TcpTransport::start(&binary, Default::default(), &mut async_cx)
            .await
            .unwrap();

        assert!(
            transport.has_adapter_stdin(),
            "docker adapter (port_forward_command Some) must have a piped stdin handle"
        );

        transport.kill();

        assert!(
            !transport.has_adapter_stdin(),
            "kill() must drop the adapter stdin handle (triggers in-container EOF)"
        );
    }

    #[gpui::test]
    async fn tcp_transport_no_adapter_stdin_without_forwarding(cx: &mut TestAppContext) {
        let binary = binary_with_command_and_sidecar(Some("/bin/cat".to_string()), None);

        let mut async_cx = cx.to_async();
        let transport = TcpTransport::start(&binary, Default::default(), &mut async_cx)
            .await
            .unwrap();

        assert!(
            !transport.has_adapter_stdin(),
            "non-docker adapter must keep Stdio::null() stdin (no handle)"
        );
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p dap tcp_transport_pipes_adapter_stdin tcp_transport_no_adapter_stdin 2>&1 | tail -15`
Expected: FAIL — `no field 'adapter_stdin' on type '&TcpTransport'` / `no method 'has_adapter_stdin'`.

- [ ] **Step 3: Add the `adapter_stdin` field**

In `crates/dap/src/transport.rs`, change the `TcpTransport` struct (line ~472) to:

```rust
pub struct TcpTransport {
    executor: BackgroundExecutor,
    pub port: u16,
    pub host: IpAddr,
    pub timeout: u64,
    process: Arc<Mutex<Option<Child>>>,
    port_forward_process: Option<Child>,
    adapter_stdin: Option<smol::process::ChildStdin>,
    _stderr_task: Option<Task<()>>,
    _stdout_task: Option<Task<()>>,
}
```

- [ ] **Step 4: Pipe and capture stdin in `start()`**

In `start()`, find the adapter-spawn block (lines ~525–559). Replace:

```rust
        let mut process = None;
        let mut stdout_task = None;
        let mut stderr_task = None;

        if let Some(command) = &binary.command {
            let mut command = util::command::new_std_command(&command);

            if let Some(cwd) = &binary.cwd {
                command.current_dir(cwd);
            }

            command.args(&binary.arguments);
            command.envs(&binary.envs);

            let mut p = Child::spawn(command, Stdio::null(), Stdio::piped(), Stdio::piped())
                .with_context(|| "failed to start debug adapter.")?;
```

with:

```rust
        let mut process = None;
        let mut stdout_task = None;
        let mut stderr_task = None;
        let mut adapter_stdin = None;

        if let Some(command) = &binary.command {
            let mut command = util::command::new_std_command(&command);

            if let Some(cwd) = &binary.cwd {
                command.current_dir(cwd);
            }

            command.args(&binary.arguments);
            command.envs(&binary.envs);

            // The docker DAP path holds the adapter's `docker exec` stdin open
            // and drops it on kill()/Drop, so the in-container lifeline wrapper
            // sees EOF and reaps itself. Non-docker keeps Stdio::null().
            let stdin_mode = if binary.port_forward_command.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            };

            let mut p = Child::spawn(command, stdin_mode, Stdio::piped(), Stdio::piped())
                .with_context(|| "failed to start debug adapter.")?;

            adapter_stdin = p.stdin.take();
```

Then find where `this` is constructed (the `let this = Self { … };` near line ~571) and add the field:

```rust
        let this = Self {
            executor: cx.background_executor().clone(),
            port,
            host,
            process: Arc::new(Mutex::new(process)),
            port_forward_process,
            adapter_stdin,
            timeout,
            _stdout_task: stdout_task,
            _stderr_task: stderr_task,
        };
```

- [ ] **Step 5: Drop stdin first in `kill()` and `Drop`**

Replace `fn kill(&mut self)` (lines ~591–598) with:

```rust
    fn kill(&mut self) {
        // Close the adapter's docker-exec stdin first: the in-container
        // lifeline wrapper reaps itself (and, via debugpy, its debuggee) on
        // stdin-EOF. Doing this before killing the local clients ensures the
        // in-container process is gone before a restart reuses the port.
        drop(self.adapter_stdin.take());
        if let Some(mut proxy) = self.port_forward_process.take() {
            proxy.kill().log_err();
        }
        if let Some(process) = &mut *self.process.lock() {
            process.kill().log_err();
        }
    }
```

Replace the `impl Drop for TcpTransport` body (lines ~658–667) with:

```rust
impl Drop for TcpTransport {
    fn drop(&mut self) {
        drop(self.adapter_stdin.take());
        if let Some(mut proxy) = self.port_forward_process.take() {
            proxy.kill().log_err();
        }
        if let Some(mut p) = self.process.lock().take() {
            p.kill().log_err();
        }
    }
}
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test -p dap tcp_transport_pipes_adapter_stdin tcp_transport_no_adapter_stdin tcp_transport_spawns_port_forward_sidecar tcp_transport_no_sidecar 2>&1 | tail -15`
Expected: PASS — all four (the two new + the two existing sidecar tests, unbroken).

- [ ] **Step 7: Clippy**

Run: `./script/clippy -p dap 2>&1 | tail -5`
Expected: Finishes with no warnings.

- [ ] **Step 8: Commit**

```bash
git add crates/dap/src/transport.rs
git commit -m "$(cat <<'EOF'
dap: Hold the docker adapter docker-exec stdin open in TcpTransport

Dropping it on kill()/Drop drives the in-container lifeline to EOF.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Surface an actionable `/bin/sh` hint on docker connect failure

**Files:**
- Modify: `crates/dap/src/transport.rs` — `fn connect()` (lines ~608–655); tests in `mod tests`

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `crates/dap/src/transport.rs`. The adapter command is
`/bin/sh -c "exit 1"`, which terminates immediately, so `connect()`
deterministically hits the *process-exited* branch (not the timeout) with
nothing listening on `:9999`:

```rust
    fn exiting_adapter_binary(
        port_forward_command: Option<CommandTemplate>,
    ) -> DebugAdapterBinary {
        let mut binary =
            binary_with_command_and_sidecar(Some("/bin/sh".to_string()), port_forward_command);
        binary.arguments = vec!["-c".to_string(), "exit 1".to_string()];
        binary
    }

    #[gpui::test]
    async fn connect_failure_on_docker_path_hints_at_sh_requirement(
        cx: &mut TestAppContext,
    ) {
        let binary = exiting_adapter_binary(Some(CommandTemplate {
            program: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), "".to_string()],
            env: Default::default(),
        }));

        let mut async_cx = cx.to_async();
        let mut transport = TcpTransport::start(&binary, Default::default(), &mut async_cx)
            .await
            .unwrap();

        let err = transport.connect().await.unwrap_err().to_string();
        assert!(
            err.contains("/bin/sh in the target container"),
            "docker connect failure must hint at the sh requirement: {err}"
        );
    }

    #[gpui::test]
    async fn connect_failure_without_forwarding_has_no_sh_hint(cx: &mut TestAppContext) {
        let binary = exiting_adapter_binary(None);

        let mut async_cx = cx.to_async();
        let mut transport = TcpTransport::start(&binary, Default::default(), &mut async_cx)
            .await
            .unwrap();

        let err = transport.connect().await.unwrap_err().to_string();
        assert!(
            !err.contains("/bin/sh in the target container"),
            "non-docker failure must not add the docker sh hint: {err}"
        );
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p dap connect_failure_on_docker_path connect_failure_without_forwarding 2>&1 | tail -15`
Expected: FAIL — the error string does not yet contain `/bin/sh in the target container`.

- [ ] **Step 3: Add the docker hint to `connect()`**

In `fn connect()` (lines ~608–655), add `let is_docker = self.port_forward_process.is_some();` after `let process = self.process.clone();`, and move `is_docker` into the inner async. Replace the `anyhow::bail!("{output}\nerror: process exited before debugger attached.");` line with:

```rust
                                        let hint = if is_docker {
                                            "\nhint: devcontainer debugging requires \
                                             /bin/sh in the target container"
                                        } else {
                                            ""
                                        };
                                        anyhow::bail!(
                                            "{output}\nerror: process exited before \
                                             debugger attached.{hint}"
                                        );
```

Concretely, the function head becomes:

```rust
        let executor = self.executor.clone();
        let timeout = self.timeout;
        let address = SocketAddr::new(self.host, self.port);
        let process = self.process.clone();
        let is_docker = self.port_forward_process.is_some();
        executor.clone().spawn(async move {
            select! {
                _ = executor.timer(Duration::from_millis(timeout)).fuse() => {
                    anyhow::bail!("Connection to TCP DAP timeout {address}");
                },
                result = executor.clone().spawn(async move {
                    loop {
                        match TcpStream::connect(address).await {
```

(The `async move` block already moves `process`/`executor`; `is_docker` is `Copy` and is captured by the same `move`.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p dap connect_failure_on_docker_path connect_failure_without_forwarding 2>&1 | tail -15`
Expected: PASS — both.

- [ ] **Step 5: Run the transport test module to check for regressions**

Run: `cargo test -p dap transport 2>&1 | tail -15`
Expected: PASS — all transport tests, including Task 3's.

- [ ] **Step 6: Clippy**

Run: `./script/clippy -p dap 2>&1 | tail -5`
Expected: Finishes with no warnings.

- [ ] **Step 7: Commit**

```bash
git add crates/dap/src/transport.rs
git commit -m "$(cat <<'EOF'
dap: Hint at the container /bin/sh requirement on docker DAP connect failure

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Wrap the adapter command in the docker (`Separate`) branch

**Files:**
- Modify: `crates/project/src/debugger/dap_store.rs` — import (~line 20), the `Separate` flow (~lines 323–385), add a pure decision helper + `#[cfg(test)] mod` at end of file

- [ ] **Step 1: Write the failing tests**

Append to the end of `crates/project/src/debugger/dap_store.rs`:

```rust
#[cfg(test)]
mod lifeline_wiring_tests {
    use super::adapter_command_for_forwarding;
    use remote::PortForwardingMode;

    #[test]
    fn separate_mode_wraps_the_adapter_command() {
        let (program, args) = adapter_command_for_forwarding(
            Some("python".to_string()),
            vec!["-m".to_string(), "debugpy".to_string()],
            PortForwardingMode::Separate,
        );
        assert_eq!(program.as_deref(), Some("sh"));
        assert_eq!(args[0], "-c");
        assert_eq!(args[2], "sh");
        assert_eq!(args[3], "python");
        assert_eq!(&args[4..], &["-m".to_string(), "debugpy".to_string()]);
    }

    #[test]
    fn inline_and_shared_modes_do_not_wrap() {
        for mode in [
            PortForwardingMode::Inline,
            PortForwardingMode::SharedInterface,
        ] {
            let (program, args) = adapter_command_for_forwarding(
                Some("python".to_string()),
                vec!["-m".to_string()],
                mode,
            );
            assert_eq!(program.as_deref(), Some("python"));
            assert_eq!(args, vec!["-m".to_string()]);
        }
    }

    #[test]
    fn separate_mode_with_no_command_passes_through() {
        let (program, args) = adapter_command_for_forwarding(
            None,
            vec!["shell-arg".to_string()],
            PortForwardingMode::Separate,
        );
        assert_eq!(program, None);
        assert_eq!(args, vec!["shell-arg".to_string()]);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p project lifeline_wiring_tests 2>&1 | tail -15`
Expected: FAIL — `cannot find function 'adapter_command_for_forwarding'`.

- [ ] **Step 3: Add the pure decision helper**

In `crates/project/src/debugger/dap_store.rs`, add this free function near the top of the file (after the `use` block, before the first `impl`/`struct`):

```rust
/// Decides the adapter `(program, args)` for a forwarding mode. In docker
/// (`Separate`) mode the adapter is wrapped in a stdin-EOF lifeline so the
/// in-container process reaps itself when its `docker exec` stdin closes.
/// Other modes (and a `None` program — a shell adapter) pass through unchanged.
fn adapter_command_for_forwarding(
    command: Option<String>,
    arguments: Vec<String>,
    forwarding_mode: PortForwardingMode,
) -> (Option<String>, Vec<String>) {
    match (command, forwarding_mode) {
        (Some(program), PortForwardingMode::Separate) => {
            let (program, args) =
                dap::adapters::wrap_with_stdin_lifeline(&program, &arguments);
            (Some(program), args)
        }
        (command, _) => (command, arguments),
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p project lifeline_wiring_tests 2>&1 | tail -15`
Expected: PASS — 3 passed.

- [ ] **Step 5: Wire the helper into the `Separate` flow**

In the same file, find the `let command = remote.read_with(cx, |remote, _cx| { remote.build_command_with_options( binary.command, &binary.arguments, … ) })?;` block (lines ~376–385). Replace it with:

```rust
                    let (adapter_command, adapter_arguments) =
                        adapter_command_for_forwarding(
                            binary.command,
                            binary.arguments,
                            forwarding_mode,
                        );

                    let command = remote.read_with(cx, |remote, _cx| {
                        remote.build_command_with_options(
                            adapter_command,
                            &adapter_arguments,
                            &binary.envs,
                            binary.cwd.map(|path| path.display().to_string()),
                            port_forwarding_inline,
                            Interactive::No,
                        )
                    })?;
```

Note on the `forwarding_mode` binding: it is bound at line ~323 (`let forwarding_mode = remote.read_with(cx, |remote, _cx| remote.port_forwarding_mode());`) and the earlier `match forwarding_mode { … }` at line ~332 would move it, making the by-value use in this step a use-after-move (unless `PortForwardingMode` is `Copy` — do not rely on that). Make it deterministic regardless of derives: change line ~332 only from `match forwarding_mode {` to `match &forwarding_mode {`. Leave the arm patterns (`PortForwardingMode::SharedInterface => …`, etc.) **unchanged** — Rust match ergonomics binds them by reference automatically, and this form does not trip `clippy::match_ref_pats`. The earlier match now only borrows, so `forwarding_mode` is still owned and is consumed by-value exactly once here in `adapter_command_for_forwarding(binary.command, binary.arguments, forwarding_mode)`.

- [ ] **Step 6: Build the crate to verify it compiles**

Run: `cargo build -p project 2>&1 | tail -15`
Expected: Compiles with no errors.

- [ ] **Step 7: Run the wiring tests + a broad project debugger check**

Run: `cargo test -p project lifeline_wiring_tests 2>&1 | tail -10`
Expected: PASS — 3 passed (helper still correct after wiring).

- [ ] **Step 8: Clippy**

Run: `./script/clippy -p project 2>&1 | tail -5`
Expected: Finishes with no warnings.

- [ ] **Step 9: Commit**

```bash
git add crates/project/src/debugger/dap_store.rs
git commit -m "$(cat <<'EOF'
dap_store: Wrap the docker adapter command in the stdin lifeline

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Final Verification

- [ ] **Workspace test sweep for touched crates**

Run: `cargo test -p dap -p docker_proxy -p project 2>&1 | tail -25`
Expected: All green; no reaper tests remain in `docker_proxy`.

- [ ] **Workspace clippy for touched crates**

Run: `./script/clippy -p dap -p docker_proxy -p project 2>&1 | tail -5`
Expected: No warnings.

- [ ] **Manual devcontainer verification (cannot be automated — CI has no devcontainer)**

In a real devcontainer Django/debugpy session: start a debug session, confirm it runs; restart it; confirm the restarted session reaches a running `threads` response (no `Server is not available`, no `"attach" expected`), and that ports `:5678` and the app port are free for the restart. This is the acceptance criterion the unit tests cannot prove.

---

## Spec Coverage Check

| Spec item | Task |
|-----------|------|
| D1 full replacement / pure-pump bridge | Task 2 |
| D2 adapter-owned stdin lifeline | Task 1 + Task 3 |
| D3 debuggee teardown via debugpy SIGTERM (no setsid/group-kill) | Task 1 (LIFELINE shape + tests) |
| D4 `/bin/sh` surfaced as actionable error | Task 4 |
| D5 scope strictly `Separate` | Task 5 (helper gating) + Task 3 (stdin gating) |
| Exact LIFELINE shell | Task 1 Step 3 |
| Restart data flow | Tasks 3+5 combined; Final Verification manual step |
| Edge: PID reuse / cross-session impossible | Task 1 (intrinsic `$!`); covered by design, no code branch needed |
| Edge: non-docker unaffected | Task 3 (`tcp_transport_no_adapter_stdin_without_forwarding`), Task 5 (`inline_and_shared_modes_do_not_wrap`) |
| Test table rows | Tasks 1–5 steps |
