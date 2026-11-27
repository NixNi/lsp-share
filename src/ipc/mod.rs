#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::{
    build_lsp_command, configure_detached_daemon_command, terminate_child_process, try_connect,
    ChildTracker, FileLock, IpcReadHalf, IpcWriteHalf, LocalListener,
};

#[cfg(windows)]
pub use windows::{
    build_lsp_command, configure_detached_daemon_command, terminate_child_process, try_connect,
    ChildTracker, FileLock, IpcReadHalf, IpcWriteHalf, LocalListener,
};

use crate::paths;
use anyhow::{anyhow, Context, Result};
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

pub async fn connect_or_start_daemon(
    config_override: Option<&Path>,
    trace_override: Option<&Path>,
) -> Result<(IpcReadHalf, IpcWriteHalf)> {
    let endpoint = paths::ipc_endpoint();
    if let Ok(conn) = try_connect(&endpoint).await {
        return Ok(conn);
    }

    let lock_path = paths::lock_file_path();
    let _lock = FileLock::acquire(&lock_path).await?;

    // Double-checked locking: another caller may have started the daemon while we waited.
    if let Ok(conn) = try_connect(&endpoint).await {
        return Ok(conn);
    }

    #[cfg(unix)]
    {
        let paths::IpcEndpoint::UnixSocket(ref sock_path) = endpoint;
        if sock_path.exists() {
            let _ = std::fs::remove_file(sock_path);
        }
    }

    spawn_detached_daemon(config_override, trace_override)?;

    let deadline = Instant::now() + Duration::from_secs(3);
    let mut backoff_ms = 5u64;
    let mut last_err = None;
    while Instant::now() < deadline {
        match try_connect(&endpoint).await {
            Ok(conn) => return Ok(conn),
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms * 2).min(50);
            }
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow!("timed out waiting for lsp-share daemon")))
}

fn spawn_detached_daemon(
    config_override: Option<&Path>,
    trace_override: Option<&Path>,
) -> Result<()> {
    let exe_raw = std::env::current_exe().context("failed to locate current lsp-share binary")?;
    let exe = std::fs::canonicalize(&exe_raw).unwrap_or(exe_raw);
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("daemon").arg("--foreground");
    if let Some(cfg) = config_override {
        cmd.arg("--config").arg(cfg);
    }
    if let Some(tr) = trace_override {
        cmd.arg("--trace").arg(tr);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    configure_detached_daemon_command(&mut cmd);
    cmd.spawn()
        .context("failed to spawn detached lsp-share daemon")?;
    Ok(())
}
