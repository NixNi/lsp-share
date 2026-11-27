pub mod documents;
pub mod instance;
pub mod pool;
pub mod server;

use crate::config::Config;
use crate::daemon::pool::Pool;
use crate::ipc::{ChildTracker, LocalListener};
use crate::paths;
use crate::trace::TraceLogger;
use anyhow::{Context, Result};
use std::path::Path;
use tokio::sync::watch;

pub async fn run_daemon(
    config_override: Option<&Path>,
    idle_timeout_override: Option<u64>,
    log_level_override: Option<&str>,
    trace_override: Option<&Path>,
    foreground: bool,
) -> Result<()> {
    if !foreground {
        // Spawn self detached with --foreground and return immediately.
        let exe_raw = std::env::current_exe().context("failed to get current exe")?;
        let exe = std::fs::canonicalize(&exe_raw).unwrap_or(exe_raw);
        let mut cmd = std::process::Command::new(exe);
        cmd.arg("daemon").arg("--foreground");
        if let Some(c) = config_override {
            cmd.arg("--config").arg(c);
        }
        if let Some(t) = idle_timeout_override {
            cmd.arg("--idle-timeout").arg(t.to_string());
        }
        if let Some(l) = log_level_override {
            cmd.arg("--log-level").arg(l);
        }
        if let Some(tr) = trace_override {
            cmd.arg("--trace").arg(tr);
        }
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        crate::ipc::configure_detached_daemon_command(&mut cmd);
        cmd.spawn().context("failed to launch background daemon")?;
        return Ok(());
    }

    let mut cfg = Config::load(config_override)?;
    if let Some(idle) = idle_timeout_override {
        cfg.daemon.idle_timeout_secs = idle;
    }
    if let Some(lvl) = log_level_override {
        cfg.daemon.log_level = lvl.to_string();
    }

    init_daemon_logging(&cfg.daemon.log_level);

    let trace_path = trace_override.or(cfg.daemon.trace_file.as_deref());
    let trace = TraceLogger::open(trace_path)?;
    let child_tracker = ChildTracker::new()?;

    let endpoint = paths::ipc_endpoint();
    let listener = LocalListener::bind(&endpoint)?;
    tracing::info!(
        pid = std::process::id(),
        endpoint = %paths::ipc_endpoint_display(),
        "lsp-share daemon listening"
    );

    let (pool, mut exit_rx) = Pool::new(cfg, trace, child_tracker);
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

    let pool_cleanup = pool.clone();
    tokio::spawn(async move {
        while let Some(exited_id) = exit_rx.recv().await {
            pool_cleanup.remove_exited(&exited_id).await;
        }
    });

    loop {
        tokio::select! {
            accept_res = listener.accept() => {
                match accept_res {
                    Ok((read_half, write_half)) => {
                        let pool_c = pool.clone();
                        let stx = shutdown_tx.clone();
                        tokio::spawn(async move {
                            server::handle_connection(read_half, write_half, pool_c, stx).await;
                        });
                    }
                    Err(e) => {
                        tracing::error!("IPC accept error: {e}");
                    }
                }
            }
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    tracing::info!("daemon shutdown requested via control channel");
                    break;
                }
            }
            _ = wait_termination_signal() => {
                tracing::info!("received termination signal, stopping all LSP instances");
                pool.stop_all().await;
                break;
            }
        }
    }

    #[cfg(unix)]
    {
        let paths::IpcEndpoint::UnixSocket(sock_path) = paths::ipc_endpoint();
        let _ = std::fs::remove_file(sock_path);
    }

    Ok(())
}

async fn wait_termination_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate()).ok();
        let ctrl_c = tokio::signal::ctrl_c();
        match sigterm {
            Some(ref mut s) => {
                tokio::select! {
                    _ = ctrl_c => {}
                    _ = s.recv() => {}
                }
            }
            None => {
                let _ = ctrl_c.await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn init_daemon_logging(level: &str) {
    let log_path = paths::daemon_log_path();
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let filter = tracing_subscriber::EnvFilter::try_new(level)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    if let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(file))
            .try_init();
    } else {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(std::io::stderr)
            .try_init();
    }
}
