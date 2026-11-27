use crate::paths::{self, IpcEndpoint};
use anyhow::{anyhow, Context, Result};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};

pub type IpcReadHalf = OwnedReadHalf;
pub type IpcWriteHalf = OwnedWriteHalf;

pub struct LocalListener {
    listener: UnixListener,
}

impl LocalListener {
    pub fn bind(endpoint: &IpcEndpoint) -> Result<Self> {
        let IpcEndpoint::UnixSocket(path) = endpoint;
        if let Some(parent) = path.parent() {
            paths::ensure_secure_dir(parent)?;
        }
        if path.exists() {
            // Check if an active daemon is already listening before removing stale socket.
            if std::os::unix::net::UnixStream::connect(path).is_ok() {
                return Err(anyhow!(
                    "lsp-share daemon is already running at {}",
                    path.display()
                ));
            }
            let _ = std::fs::remove_file(path);
        }

        let listener = UnixListener::bind(path)
            .with_context(|| format!("failed to bind Unix socket {}", path.display()))?;
        let perms = std::fs::Permissions::from_mode(0o600);
        let _ = std::fs::set_permissions(path, perms);
        Ok(Self { listener })
    }

    pub async fn accept(&self) -> Result<(IpcReadHalf, IpcWriteHalf)> {
        let (stream, _) = self.listener.accept().await?;
        Ok(stream.into_split())
    }
}

pub async fn try_connect(endpoint: &IpcEndpoint) -> Result<(IpcReadHalf, IpcWriteHalf)> {
    let IpcEndpoint::UnixSocket(path) = endpoint;
    let stream = UnixStream::connect(path)
        .await
        .with_context(|| format!("failed to connect to {}", path.display()))?;
    Ok(stream.into_split())
}

pub struct FileLock {
    _file: File,
}

impl FileLock {
    pub async fn acquire(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            paths::ensure_secure_dir(parent)?;
        }
        let path_owned = path.to_path_buf();
        let file = tokio::task::spawn_blocking(move || -> Result<File> {
            let file = OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(&path_owned)
                .with_context(|| format!("failed to open lockfile {}", path_owned.display()))?;
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
            if rc != 0 {
                return Err(std::io::Error::last_os_error()).context("flock failed");
            }
            Ok(file)
        })
        .await
        .context("lock task panicked")??;
        Ok(Self { _file: file })
    }
}

pub fn configure_detached_daemon_command(cmd: &mut std::process::Command) {
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[derive(Clone, Default)]
pub struct ChildTracker;

impl ChildTracker {
    pub fn new() -> Result<Self> {
        Ok(Self)
    }

    pub fn track_pid(&self, _pid: u32) -> Result<()> {
        Ok(())
    }
}

pub fn build_lsp_command(program: &Path, args: &[String]) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args);
    cmd.process_group(0);
    cmd
}

pub fn terminate_child_process(pid: u32, force: bool) {
    let sig = if force { libc::SIGKILL } else { libc::SIGTERM };
    unsafe {
        // Negative PID targets the child's entire process group.
        libc::kill(-(pid as libc::pid_t), sig);
    }
}
