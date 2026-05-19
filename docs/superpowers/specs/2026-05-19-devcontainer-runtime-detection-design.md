# Devcontainer Container Runtime Detection

**Date:** 2026-05-19

## Problem

`check_for_docker` verifies only that the CLI binary exists (`--version`), not that the daemon is reachable. When Docker Desktop or OrbStack is not running, the first real command (`docker ps` inside the manifest) fails with a confusing socket-not-found error. There is also no way to express "use whichever runtime is available" — the existing `use_podman: bool` setting forces an explicit choice between docker and podman with no auto-detection.

## Goals

- Auto-detect which container runtime is running (docker-compatible first, podman second)
- Let users pin a specific runtime when they have multiple installed
- Produce clear, traceable log output so failures are diagnosable
- Keep existing persisted connections (`DevContainerConnection.use_podman`) working unchanged

## Out of Scope

- OrbStack-specific handling — OrbStack exposes the standard Docker API and `docker` CLI; it requires no special code path
- Changes to the SSH remote transport (`DockerExecConnection.use_podman`)

---

## Type Design

### `ContainerRuntimeHint` (settings layer, in `settings_content`)

User preference — may include `Auto`.

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

### `ContainerRuntime` (resolved fact, in `dev_container`)

The concrete runtime chosen after detection. Never `Auto`.

```rust
#[derive(Clone, Debug, PartialEq)]
pub enum ContainerRuntime {
    Docker,
    Podman,
}

impl ContainerRuntime {
    fn cli_name(&self) -> &'static str {
        match self {
            ContainerRuntime::Docker => "docker",
            ContainerRuntime::Podman => "podman",
        }
    }
}
```

---

## Settings Changes

### `RemoteSettingsContent` (`settings_content`)

- Add `container_runtime: Option<ContainerRuntimeHint>`
- Keep `use_podman: Option<bool>` (deprecated — still parsed so existing configs do not break)

### `DevContainerSettings` (`dev_container/src/lib.rs`)

Resolution priority in `from_settings`:

1. `container_runtime` is set → use it
2. `use_podman: true` → `ContainerRuntimeHint::Podman`
3. Otherwise → `ContainerRuntimeHint::Auto`

### `DevContainerContext`

- Remove `use_podman: bool`
- Add `container_runtime: ContainerRuntimeHint`

### Persistence (`DevContainerConnection`, SQLite)

`use_podman: bool` stays unchanged. On reconnect, reconstruct `ContainerRuntime` from it:
`true → Podman`, `false → Docker`. Detection is skipped on reconnect.

---

## Detection Function

```
async fn resolve_container_cli(hint: ContainerRuntimeHint) -> Result<ContainerRuntime, DevContainerError>
```

Probes liveness with `{cli} ps -q` (not `--version`) — this proves the daemon is reachable.

| Hint | Behaviour |
|------|-----------|
| `Auto` | Probe docker → on failure probe podman → on failure return `DockerNotAvailable` |
| `Docker` | Probe docker only → on failure return `DockerNotAvailable` |
| `Podman` | Probe podman only → on failure return `DockerNotAvailable` |

### Logging

| Event | Level | Message |
|-------|-------|---------|
| Before each probe | `debug` | `"devcontainer: probing {runtime} daemon"` |
| Probe succeeds | `info` | `"devcontainer: using {runtime}"` |
| Probe fails in Auto (trying next) | `warn` | `"devcontainer: {runtime} not accessible ({error}), trying next"` |
| All probes exhausted | `error` | `"devcontainer: no container runtime found"` |
| Pinned runtime not accessible | `error` | `"devcontainer: {runtime} daemon not accessible: {error}"` |

---

## Threading

`resolve_container_cli` is called once at the top of `start_dev_container_with_config`:

```
let runtime = resolve_container_cli(context.container_runtime).await?;
```

`runtime: ContainerRuntime` is passed explicitly to:
- `spawn_dev_container(..., runtime: ContainerRuntime)`
- `read_devcontainer_configuration(..., runtime: ContainerRuntime)`

Both call `Docker::new(runtime)` instead of branching on `use_podman`.

Port-forwarding CLI selection matches on `runtime` instead of the `use_podman` branch.

`DevContainerConnection.use_podman` is set from `runtime == ContainerRuntime::Podman`.

---

## `Docker` struct changes

`Docker::new` takes `runtime: ContainerRuntime` instead of `docker_cli: &str`. Internal `is_podman()` check moves to a match on the enum. `cli_name()` is used wherever `Command::new` needs the binary name.

---

## Error Messages

- `Auto` exhausted: `"No container runtime found (tried docker and podman)"`
- Pinned `Docker` not accessible: `"docker daemon not accessible"`
- Pinned `Podman` not accessible: `"podman not accessible"`
