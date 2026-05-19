use anyhow::{Context as _, Result};

pub struct ForwardSpec {
    pub local_port: u16,
    pub remote_host: String,
    pub remote_port: u16,
}

pub fn parse_forward_spec(spec: &str) -> Result<ForwardSpec> {
    let parts: Vec<&str> = spec.splitn(3, ':').collect();
    anyhow::ensure!(
        parts.len() == 3,
        "invalid forward spec '{spec}': expected local_port:remote_host:remote_port"
    );
    let local_port: u16 = parts[0]
        .parse()
        .with_context(|| format!("invalid local port '{}' in '{spec}'", parts[0]))?;
    let remote_port: u16 = parts[2]
        .parse()
        .with_context(|| format!("invalid remote port '{}' in '{spec}'", parts[2]))?;
    Ok(ForwardSpec {
        local_port,
        remote_host: parts[1].to_string(),
        remote_port,
    })
}

pub fn main(docker_cli: &str, container_id: &str, forwards: &[ForwardSpec]) -> Result<()> {
    smol::block_on(async {
        let mut listener_tasks = Vec::new();

        for spec in forwards {
            let listener = smol::net::TcpListener::bind(format!("127.0.0.1:{}", spec.local_port))
                .await
                .with_context(|| {
                    format!("docker-proxy: port {} already in use", spec.local_port)
                })?;
            log::info!(
                "docker-proxy: listening on 127.0.0.1:{} → {}:{}",
                spec.local_port,
                spec.remote_host,
                spec.remote_port,
            );

            let docker_cli = docker_cli.to_string();
            let container_id = container_id.to_string();
            let remote_host = spec.remote_host.clone();
            let remote_port = spec.remote_port;

            listener_tasks.push(smol::spawn(async move {
                loop {
                    match listener.accept().await {
                        Ok((stream, peer)) => {
                            log::debug!("docker-proxy: connection from {peer}");
                            let docker_cli = docker_cli.clone();
                            let container_id = container_id.clone();
                            let remote_host = remote_host.clone();
                            smol::spawn(async move {
                                if let Err(error) = proxy_connection(
                                    stream,
                                    &docker_cli,
                                    &container_id,
                                    &remote_host,
                                    remote_port,
                                )
                                .await
                                {
                                    log::debug!("docker-proxy: connection closed: {error:#}");
                                }
                            })
                            .detach();
                        }
                        Err(error) => {
                            log::error!("docker-proxy: accept error: {error}");
                            break;
                        }
                    }
                }
            }));
        }

        for task in listener_tasks {
            task.await;
        }
        Ok(())
    })
}

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

async fn proxy_connection(
    tcp_stream: smol::net::TcpStream,
    docker_cli: &str,
    container_id: &str,
    remote_host: &str,
    remote_port: u16,
) -> Result<()> {
    use smol::process::{Command, Stdio};

    let bridge_cmd = build_bridge_command(remote_host, remote_port);

    let mut child = Command::new(docker_cli)
        .args(["exec", "-i", container_id, "bash", "-c", &bridge_cmd])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to spawn docker exec for port bridge")?;

    let mut child_stdin = child.stdin.take().context("docker exec has no stdin")?;
    let mut child_stdout = child.stdout.take().context("docker exec has no stdout")?;

    let (mut tcp_reader, mut tcp_writer) = futures_lite::io::split(tcp_stream);

    let tcp_to_container = futures_lite::io::copy(&mut tcp_reader, &mut child_stdin);
    let container_to_tcp = futures_lite::io::copy(&mut child_stdout, &mut tcp_writer);

    // race: when either direction closes, cancel the other to avoid half-close deadlock
    if let Err(error) = futures_lite::future::race(tcp_to_container, container_to_tcp).await {
        log::debug!("docker-proxy: connection copy ended: {error}");
    }

    if let Err(e) = child.kill() {
        log::debug!("docker-proxy: failed to kill docker exec process: {e}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_spec() {
        let spec = parse_forward_spec("54321:127.0.0.1:5678").unwrap();
        assert_eq!(spec.local_port, 54321);
        assert_eq!(spec.remote_host, "127.0.0.1");
        assert_eq!(spec.remote_port, 5678);
    }

    #[test]
    fn parses_hostname_spec() {
        let spec = parse_forward_spec("8080:localhost:8080").unwrap();
        assert_eq!(spec.local_port, 8080);
        assert_eq!(spec.remote_host, "localhost");
        assert_eq!(spec.remote_port, 8080);
    }

    #[test]
    fn rejects_bad_port() {
        assert!(parse_forward_spec("not-a-port:127.0.0.1:5678").is_err());
    }

    #[test]
    fn rejects_wrong_segment_count() {
        assert!(parse_forward_spec("54321:5678").is_err());
    }

    #[test]
    fn rejects_remote_port_out_of_range() {
        assert!(parse_forward_spec("54321:127.0.0.1:99999").is_err());
    }

    #[test]
    fn bridge_command_retries_until_remote_port_is_ready() {
        let cmd = build_bridge_command("127.0.0.1", 37095);
        assert!(
            cmd.contains("until exec 3<>/dev/tcp/127.0.0.1/37095 2>/dev/null"),
            "bridge must keep the connect-retry loop: {cmd}"
        );
        assert!(
            cmd.contains("[ $i -ge 100 ] && exit 1"),
            "bridge must bound the retry loop: {cmd}"
        );
    }

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
}
