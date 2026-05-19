# Devcontainer Container Runtime Detection — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the `use_podman: bool` global setting with a `ContainerRuntimeHint` enum (Auto/Docker/Podman) that auto-detects a running container runtime instead of checking only for binary presence.

**Architecture:** A `ContainerRuntimeHint` enum in `settings_content` captures user preference; a `ContainerRuntime` enum in `dev_container` represents the resolved fact. Detection runs `{cli} ps -q` to verify daemon liveness. The resolved `ContainerRuntime` is threaded explicitly through `spawn_dev_container` and `read_devcontainer_configuration` instead of branching on `use_podman: bool`. Persisted `DevContainerConnection.use_podman` is unchanged.

**Tech Stack:** Rust, GPUI settings system, `util::command::new_command`, smol async, existing `DevContainerError`

---

## File Map

| File | Change |
|------|--------|
| `crates/settings_content/src/settings_content.rs` | Add `ContainerRuntimeHint` enum; add `container_runtime` field to `RemoteSettingsContent`; keep deprecated `use_podman` |
| `crates/dev_container/src/lib.rs` | Add `ContainerRuntime` enum; rewrite `DevContainerSettings`; update `DevContainerContext` |
| `crates/dev_container/src/docker.rs` | `Docker` struct field `docker_cli: String` → `runtime: ContainerRuntime`; update `new`, `is_podman`, `docker_cli` |
| `crates/dev_container/src/devcontainer_api.rs` | Replace `check_for_docker` with `resolve_container_cli`; update `start_dev_container_with_config` |
| `crates/dev_container/src/devcontainer_manifest.rs` | Add `runtime: ContainerRuntime` param to `spawn_dev_container` and `read_devcontainer_configuration`; fix test context struct literal |

---

### Task 1: Add `ContainerRuntimeHint` to settings_content

**Files:**
- Modify: `crates/settings_content/src/settings_content.rs`

- [ ] **Step 1: Write a failing test for the new enum's serde round-trip**

Add to the bottom of `crates/settings_content/src/settings_content.rs`:

```rust
#[cfg(test)]
mod container_runtime_hint_tests {
    use super::*;

    #[test]
    fn container_runtime_hint_serde_round_trip() {
        let cases = [
            (ContainerRuntimeHint::Auto, "\"auto\""),
            (ContainerRuntimeHint::Docker, "\"docker\""),
            (ContainerRuntimeHint::Podman, "\"podman\""),
        ];
        for (hint, expected_json) in cases {
            let serialized = serde_json::to_string(&hint).unwrap();
            assert_eq!(serialized, expected_json);
            let deserialized: ContainerRuntimeHint = serde_json::from_str(&serialized).unwrap();
            assert_eq!(deserialized, hint);
        }
    }

    #[test]
    fn remote_settings_accepts_container_runtime_field() {
        let json = r#"{"container_runtime": "podman"}"#;
        let content: RemoteSettingsContent = serde_json::from_str(json).unwrap();
        assert_eq!(content.container_runtime, Some(ContainerRuntimeHint::Podman));
    }
}
```

- [ ] **Step 2: Run the test to confirm it fails**

```bash
cargo test -p settings_content container_runtime_hint 2>&1 | tail -20
```

Expected: compile error — `ContainerRuntimeHint` not defined.

- [ ] **Step 3: Add the enum and field**

In `crates/settings_content/src/settings_content.rs`, locate the `RemoteSettingsContent` struct (around line 1136) and add the enum just before it:

```rust
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ContainerRuntimeHint {
    #[default]
    Auto,
    Docker,
    Podman,
}
```

Then in `RemoteSettingsContent`, add the new field after `use_podman`:

```rust
pub struct RemoteSettingsContent {
    pub ssh_connections: Option<Vec<SshConnection>>,
    pub wsl_connections: Option<Vec<WslConnection>>,
    pub dev_container_connections: Option<Vec<DevContainerConnection>>,
    pub read_ssh_config: Option<bool>,
    /// Deprecated: use `container_runtime` instead.
    pub use_podman: Option<bool>,
    pub container_runtime: Option<ContainerRuntimeHint>,
}
```

- [ ] **Step 4: Run the tests to confirm they pass**

```bash
cargo test -p settings_content container_runtime_hint 2>&1 | tail -20
```

Expected: `test container_runtime_hint_tests::container_runtime_hint_serde_round_trip ... ok` and `test container_runtime_hint_tests::remote_settings_accepts_container_runtime_field ... ok`.

- [ ] **Step 5: Commit**

```bash
git add crates/settings_content/src/settings_content.rs
git commit -m "settings_content: Add ContainerRuntimeHint enum and container_runtime setting"
```

---

### Task 2: Add `ContainerRuntime` enum and update `DevContainerSettings`

**Files:**
- Modify: `crates/dev_container/src/lib.rs`

- [ ] **Step 1: Write failing tests for the migration shim and enum**

Add to `crates/dev_container/src/lib.rs` at the bottom:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_runtime_cli_names() {
        assert_eq!(ContainerRuntime::Docker.cli_name(), "docker");
        assert_eq!(ContainerRuntime::Podman.cli_name(), "podman");
    }

    #[test]
    fn resolve_hint_container_runtime_takes_precedence() {
        let runtime = DevContainerSettings::resolve_runtime(
            Some(ContainerRuntimeHint::Docker),
            None,
        );
        assert_eq!(runtime, ContainerRuntimeHint::Docker);
    }

    #[test]
    fn resolve_hint_use_podman_true_maps_to_podman() {
        let runtime = DevContainerSettings::resolve_runtime(None, Some(true));
        assert_eq!(runtime, ContainerRuntimeHint::Podman);
    }

    #[test]
    fn resolve_hint_use_podman_false_maps_to_auto() {
        let runtime = DevContainerSettings::resolve_runtime(None, Some(false));
        assert_eq!(runtime, ContainerRuntimeHint::Auto);
    }

    #[test]
    fn resolve_hint_defaults_to_auto() {
        let runtime = DevContainerSettings::resolve_runtime(None, None);
        assert_eq!(runtime, ContainerRuntimeHint::Auto);
    }
}
```

- [ ] **Step 2: Run to confirm failure**

```bash
cargo test -p dev_container 2>&1 | grep -E "error|FAILED" | head -20
```

Expected: compile errors — `ContainerRuntime`, `resolve_runtime` not defined.

- [ ] **Step 3: Add `ContainerRuntime` enum and update the settings structs**

Replace the contents of `crates/dev_container/src/lib.rs` from `pub struct DevContainerContext` through the end of the `impl Settings for DevContainerSettings` block with:

```rust
#[derive(Clone, Debug, PartialEq)]
pub enum ContainerRuntime {
    Docker,
    Podman,
}

impl ContainerRuntime {
    pub fn cli_name(&self) -> &'static str {
        match self {
            ContainerRuntime::Docker => "docker",
            ContainerRuntime::Podman => "podman",
        }
    }
}

pub struct DevContainerContext {
    pub project_directory: Arc<Path>,
    pub container_runtime: ContainerRuntimeHint,
    pub fs: Arc<dyn Fs>,
    pub http_client: Arc<dyn HttpClient>,
    pub environment: WeakEntity<ProjectEnvironment>,
}

impl DevContainerContext {
    pub fn from_workspace(workspace: &Workspace, cx: &App) -> Option<Self> {
        let project_directory = workspace.project().read(cx).active_project_directory(cx)?;
        let container_runtime = DevContainerSettings::get_global(cx).container_runtime.clone();
        let http_client = cx.http_client().clone();
        let fs = workspace.app_state().fs.clone();
        let environment = workspace.project().read(cx).environment().downgrade();
        Some(Self {
            project_directory,
            container_runtime,
            fs,
            http_client,
            environment,
        })
    }

    pub async fn environment(&self, cx: &mut impl AppContext) -> HashMap<String, String> {
        let Ok(task) = self.environment.update(cx, |this, cx| {
            this.local_directory_environment(&Shell::System, self.project_directory.clone(), cx)
        }) else {
            return HashMap::default();
        };
        task.await
            .map(|env| env.into_iter().collect::<std::collections::HashMap<_, _>>())
            .unwrap_or_default()
    }
}

#[derive(RegisterSetting)]
struct DevContainerSettings {
    container_runtime: ContainerRuntimeHint,
}

impl DevContainerSettings {
    pub(crate) fn resolve_runtime(
        explicit: Option<ContainerRuntimeHint>,
        use_podman: Option<bool>,
    ) -> ContainerRuntimeHint {
        if let Some(hint) = explicit {
            return hint;
        }
        match use_podman {
            Some(true) => ContainerRuntimeHint::Podman,
            _ => ContainerRuntimeHint::Auto,
        }
    }
}

impl Settings for DevContainerSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        Self {
            container_runtime: Self::resolve_runtime(
                content.remote.container_runtime.clone(),
                content.remote.use_podman,
            ),
        }
    }
}
```

Add the import for `ContainerRuntimeHint` at the top of `lib.rs`. The `settings` crate glob-re-exports all of `settings_content`, so it is available there. Add alongside the existing settings imports:

```rust
use settings::ContainerRuntimeHint;
```

And remove the `pub fn use_podman(cx: &App) -> bool` function entirely (it has no external callers).

- [ ] **Step 4: Run the tests to confirm they pass**

```bash
cargo test -p dev_container 2>&1 | grep -E "test.*ok|test.*FAILED|error" | head -30
```

Expected: all 5 new tests pass. There will be compile errors in other files (`devcontainer_api.rs`, `devcontainer_manifest.rs`) referencing the old `use_podman` field — that is expected and will be fixed in later tasks.

- [ ] **Step 5: Commit**

```bash
git add crates/dev_container/src/lib.rs
git commit -m "dev_container: Add ContainerRuntime enum and replace use_podman with ContainerRuntimeHint"
```

---

### Task 3: Update `Docker` struct to use `ContainerRuntime`

**Files:**
- Modify: `crates/dev_container/src/docker.rs`

This task has no new unit tests — the existing manifest tests cover `Docker` behavior end-to-end.

- [ ] **Step 1: Update the `Docker` struct fields**

In `crates/dev_container/src/docker.rs`, find the `Docker` struct (around line 176) and replace it:

```rust
pub(crate) struct Docker {
    runtime: ContainerRuntime,
    has_buildx: bool,
}
```

- [ ] **Step 2: Update `Docker::new`**

Replace the `impl Docker` block's `new` method (lines 188–207):

```rust
pub(crate) async fn new(runtime: ContainerRuntime) -> Self {
    let has_buildx = match &runtime {
        ContainerRuntime::Podman => false,
        ContainerRuntime::Docker => {
            let output = Command::new("docker")
                .args(["buildx", "version"])
                .output()
                .await;
            output.map(|o| o.status.success()).unwrap_or(false)
        }
    };
    if !has_buildx && runtime == ContainerRuntime::Docker {
        log::info!(
            "docker buildx not found; dev container builds will use the scratch-image fallback"
        );
    }
    Self { runtime, has_buildx }
}
```

- [ ] **Step 3: Update `is_podman` and `docker_cli`**

Replace the two methods (lines 209–211 and 406–408):

```rust
fn is_podman(&self) -> bool {
    self.runtime == ContainerRuntime::Podman
}
```

```rust
fn docker_cli(&self) -> String {
    self.runtime.cli_name().to_string()
}
```

- [ ] **Step 4: Add import for `ContainerRuntime`**

In `crates/dev_container/src/docker.rs`, the existing `use crate::` block (lines 7–10) already imports `DevContainerError` and others. Add `ContainerRuntime` to it:

```rust
use crate::{
    ContainerRuntime,
    command_json::evaluate_json_command,
    devcontainer_api::DevContainerError,
    devcontainer_json::MountDefinition,
};
```

- [ ] **Step 5: Verify it compiles (compile check only — full tests need later tasks)**

```bash
cargo check -p dev_container 2>&1 | grep -E "^error" | head -20
```

Expected: errors only in `devcontainer_api.rs` and `devcontainer_manifest.rs` — not in `docker.rs`.

- [ ] **Step 6: Commit**

```bash
git add crates/dev_container/src/docker.rs
git commit -m "dev_container: Update Docker struct to use ContainerRuntime enum"
```

---

### Task 4: Replace `check_for_docker` with `resolve_container_cli`

**Files:**
- Modify: `crates/dev_container/src/devcontainer_api.rs`

- [ ] **Step 1: Remove `check_for_docker` and add `resolve_container_cli`**

In `crates/dev_container/src/devcontainer_api.rs`, find `check_for_docker` (around line 371) and replace the entire function with:

```rust
async fn probe_cli(cli: &str) -> Result<(), DevContainerError> {
    log::debug!("devcontainer: probing {} daemon", cli);
    let output = util::command::new_command(cli)
        .args(["ps", "-q"])
        .output()
        .await
        .map_err(|e| {
            log::warn!("devcontainer: {} not accessible: {e:#}", cli);
            DevContainerError::DockerNotAvailable
        })?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        log::warn!("devcontainer: {} not accessible: {}", cli, stderr.trim());
        Err(DevContainerError::DockerNotAvailable)
    }
}

pub(crate) async fn resolve_container_cli(
    hint: ContainerRuntimeHint,
) -> Result<ContainerRuntime, DevContainerError> {
    match hint {
        ContainerRuntimeHint::Docker => {
            probe_cli("docker").await.map_err(|_| {
                log::error!("devcontainer: docker daemon not accessible");
                DevContainerError::DockerNotAvailable
            })?;
            log::info!("devcontainer: using docker");
            Ok(ContainerRuntime::Docker)
        }
        ContainerRuntimeHint::Podman => {
            probe_cli("podman").await.map_err(|_| {
                log::error!("devcontainer: podman not accessible");
                DevContainerError::DockerNotAvailable
            })?;
            log::info!("devcontainer: using podman");
            Ok(ContainerRuntime::Podman)
        }
        ContainerRuntimeHint::Auto => {
            if probe_cli("docker").await.is_ok() {
                log::info!("devcontainer: using docker");
                return Ok(ContainerRuntime::Docker);
            }
            log::warn!("devcontainer: docker not accessible, trying podman");
            if probe_cli("podman").await.is_ok() {
                log::info!("devcontainer: using podman");
                return Ok(ContainerRuntime::Podman);
            }
            log::error!("devcontainer: no container runtime found (tried docker and podman)");
            Err(DevContainerError::DockerNotAvailable)
        }
    }
}
```

- [ ] **Step 2: Update imports in `devcontainer_api.rs`**

`ContainerRuntimeHint` comes from the `settings` crate (which glob-re-exports `settings_content`). `ContainerRuntime` comes from `crate::`.

Update the existing `use settings::` import (line 12):

```rust
use settings::{
    ContainerRuntimeHint, DevContainerConnection, infer_json_indent_size,
    replace_value_in_json_text,
};
```

Update the existing `use crate::` block (around line 18):

```rust
use crate::{
    ContainerRuntime, DevContainerContext, DevContainerFeature, DevContainerTemplate,
    devcontainer_json::{DevContainer, ForwardPort},
    devcontainer_manifest::{read_devcontainer_configuration, spawn_dev_container},
    devcontainer_templates_repository, get_latest_oci_manifest, get_oci_token, ghcr_registry,
    oci::download_oci_tarball,
};
```

- [ ] **Step 3: Verify compile**

```bash
cargo check -p dev_container 2>&1 | grep "^error" | head -20
```

Expected: remaining errors only in `start_dev_container_with_config` (still calls `check_for_docker` and uses `context.use_podman`). Those are fixed in the next task.

- [ ] **Step 4: Commit**

```bash
git add crates/dev_container/src/devcontainer_api.rs
git commit -m "dev_container: Replace check_for_docker with resolve_container_cli"
```

---

### Task 5: Wire `ContainerRuntime` through `start_dev_container_with_config` and the manifest

**Files:**
- Modify: `crates/dev_container/src/devcontainer_api.rs`
- Modify: `crates/dev_container/src/devcontainer_manifest.rs`

- [ ] **Step 1: Update `spawn_dev_container` signature**

In `crates/dev_container/src/devcontainer_manifest.rs`, find `spawn_dev_container` (around line 2311) and update its signature and body:

```rust
pub(crate) async fn spawn_dev_container(
    context: &DevContainerContext,
    environment: HashMap<String, String>,
    config: DevContainerConfig,
    local_project_path: &Path,
    runtime: ContainerRuntime,
) -> Result<DevContainerUp, DevContainerError> {
    let docker = Docker::new(runtime).await;
    // ... rest of function body unchanged
```

- [ ] **Step 2: Update `read_devcontainer_configuration` signature**

In `crates/dev_container/src/devcontainer_manifest.rs`, find `read_devcontainer_configuration` (around line 2288) and update its signature and body:

```rust
pub(crate) async fn read_devcontainer_configuration(
    config: DevContainerConfig,
    context: &DevContainerContext,
    environment: HashMap<String, String>,
    runtime: ContainerRuntime,
) -> Result<DevContainer, DevContainerError> {
    let docker = Docker::new(runtime).await;
    // ... rest of function body unchanged
```

- [ ] **Step 3: Add `ContainerRuntime` import to manifest**

In `crates/dev_container/src/devcontainer_manifest.rs`, update the existing `use crate::` block (lines 15–32). Add `ContainerRuntime` as the first item:

```rust
use crate::{
    ContainerRuntime, DevContainerConfig, DevContainerContext,
    command_json::{CommandRunner, DefaultCommandRunner},
    devcontainer_api::{DevContainerError, DevContainerUp},
    // ... rest unchanged
};
```

- [ ] **Step 4: Update `start_dev_container_with_config` in `devcontainer_api.rs`**

Find `start_dev_container_with_config` (around line 266). Replace the `check_for_docker(context.use_podman).await?;` call and all `context.use_podman` references:

```rust
pub async fn start_dev_container_with_config(
    context: DevContainerContext,
    config: Option<DevContainerConfig>,
    environment: HashMap<String, String>,
) -> Result<(DevContainerConnection, String), DevContainerError> {
    let runtime = resolve_container_cli(context.container_runtime.clone()).await?;

    let Some(actual_config) = config.clone() else {
        return Err(DevContainerError::NotInValidProject);
    };

    match spawn_dev_container(
        &context,
        environment.clone(),
        actual_config.clone(),
        context.project_directory.clone().as_ref(),
        runtime.clone(),
    )
    .await
    {
        Ok(DevContainerUp {
            container_id,
            remote_workspace_folder,
            remote_user,
            extension_ids,
            remote_env,
            ..
        }) => {
            let parsed_config = read_devcontainer_configuration(
                actual_config,
                &context,
                environment,
                runtime.clone(),
            )
            .await;

            let project_name = match &parsed_config {
                Ok(DevContainer {
                    name: Some(name), ..
                }) => name.clone(),
                _ => get_backup_project_name(&remote_workspace_folder, &container_id),
            };

            let connection = DevContainerConnection {
                name: project_name,
                container_id: container_id.clone(),
                use_podman: runtime == ContainerRuntime::Podman,
                remote_user,
                extension_ids,
                remote_env: remote_env.into_iter().collect(),
            };

            let forward_ports = parsed_config
                .as_ref()
                .ok()
                .and_then(|c| c.forward_ports.as_deref())
                .unwrap_or_default();
            let specs = build_forward_specs(forward_ports);
            if !specs.is_empty() {
                match std::env::current_exe() {
                    Err(e) => {
                        log::error!("devcontainer: cannot start port forwarding: {e:#}");
                    }
                    Ok(current_exe) => {
                        let docker_cli = runtime.cli_name();
                        let mut args = vec![
                            "--docker-proxy".to_string(),
                            "--docker-cli".to_string(),
                            docker_cli.to_string(),
                            "--container".to_string(),
                            container_id.clone(),
                        ];
                        for (local, host, remote) in &specs {
                            args.push("--docker-proxy-forward".to_string());
                            args.push(format!("{local}:{host}:{remote}"));
                        }
                        let mut command = util::command::new_std_command(&current_exe);
                        command.args(&args);
                        match util::process::Child::spawn(
                            command,
                            std::process::Stdio::null(),
                            std::process::Stdio::null(),
                            std::process::Stdio::null(),
                        ) {
                            Ok(mut child) => {
                                log::info!(
                                    "devcontainer: started port forwarding for {} port(s)",
                                    specs.len()
                                );
                                // smol::process::Child kills the process when dropped, so we
                                // reap it in a background task rather than holding it alive here.
                                smol::spawn(async move { child.status().await.ok(); }).detach();
                            }
                            Err(e) => {
                                log::error!("devcontainer: failed to start port forwarding: {e:#}");
                            }
                        }
                    }
                }
            }

            Ok((connection, container_id))
        }
        Err(e) => Err(e),
    }
}
```

- [ ] **Step 5: Fix the test `DevContainerContext` struct literal in `devcontainer_manifest.rs`**

Find the test helper around line 2950 that constructs `DevContainerContext`:

```rust
let context = DevContainerContext {
    project_directory: SanitizedPath::cast_arc(project_path),
    use_podman: false,          // ← remove this line
    container_runtime: ContainerRuntimeHint::Auto,  // ← add this line
    fs: fs.clone(),
    http_client: http_client.clone(),
    environment: project_environment.downgrade(),
};
```

- [ ] **Step 6: Build and run all devcontainer tests**

```bash
cargo test -p dev_container 2>&1 | tail -40
```

Expected: all tests pass with no compile errors.

- [ ] **Step 7: Run clippy**

```bash
./script/clippy 2>&1 | grep "dev_container\|settings_content" | head -30
```

Expected: no warnings or errors in the changed crates.

- [ ] **Step 8: Commit**

```bash
git add crates/dev_container/src/devcontainer_api.rs crates/dev_container/src/devcontainer_manifest.rs
git commit -m "dev_container: Wire ContainerRuntime through spawn and manifest, replacing use_podman branches"
```
