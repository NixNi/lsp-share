use crate::config::{BinaryMatching, ClientRole};
use crate::ipc::{self, IpcReadHalf, IpcWriteHalf};
use crate::paths;
use crate::protocol::frame::{
    read_ipc_frame, read_lsp_frame, write_ipc_frame, write_lsp_frame, FrameType, HelloAckPayload,
    HelloMode, HelloPayload, PROTOCOL_VERSION,
};
use crate::protocol::jsonrpc::Envelope;
use crate::trace::TraceLogger;
use anyhow::{anyhow, Context, Result};
use std::path::PathBuf;
use tokio::io::{AsyncWriteExt, BufReader};

pub struct ProxyOptions {
    pub role: ClientRole,
    pub match_by: Option<BinaryMatching>,
    pub key: Option<String>,
    pub root: Option<String>,
    pub trace: Option<PathBuf>,
    pub config: Option<PathBuf>,
    pub command: String,
    pub args: Vec<String>,
}

async fn connect_and_handshake(
    opts: &ProxyOptions,
    hello_bytes: &[u8],
) -> Result<(IpcReadHalf, IpcWriteHalf, HelloAckPayload)> {
    let mut last_err = None;
    for attempt in 0..2 {
        let (mut ipc_read, mut ipc_write) =
            ipc::connect_or_start_daemon(opts.config.as_deref(), opts.trace.as_deref()).await?;

        if let Err(e) = write_ipc_frame(&mut ipc_write, FrameType::Hello, hello_bytes).await {
            last_err = Some(anyhow!("failed to send HELLO to daemon: {e}"));
            if attempt == 0 {
                continue;
            }
            break;
        }

        let mut ack_buf = Vec::with_capacity(512);
        match read_ipc_frame(&mut ipc_read, &mut ack_buf).await {
            Ok(Some(FrameType::HelloAck)) => {
                let ack: HelloAckPayload =
                    serde_json::from_slice(&ack_buf).context("failed to parse HELLO_ACK")?;
                return Ok((ipc_read, ipc_write, ack));
            }
            Ok(None) if attempt == 0 => {
                // Daemon may have just exited on idle timeout between connect() and HELLO.
                last_err = Some(anyhow!("daemon closed connection during handshake"));
                continue;
            }
            Ok(_) => {
                return Err(anyhow!("unexpected handshake response from daemon"));
            }
            Err(e) if attempt == 0 => {
                last_err = Some(anyhow!("failed to read HELLO_ACK: {e}"));
                continue;
            }
            Err(e) => {
                return Err(e).context("failed to read HELLO_ACK");
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("failed to handshake with daemon")))
}

pub async fn run_proxy(opts: ProxyOptions) -> Result<()> {
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".to_string());

    let hello = HelloPayload {
        protocol_version: PROTOCOL_VERSION,
        mode: HelloMode::Lsp,
        client_pid: std::process::id(),
        role: opts.role,
        command: opts.command.clone(),
        args: opts.args.clone(),
        cwd,
        env: paths::capture_relevant_env(),
        match_by: opts.match_by,
        key_override: opts.key.clone(),
        root_override: opts.root.clone(),
    };

    let hello_bytes = serde_json::to_vec(&hello).context("failed to serialize HELLO")?;
    let (mut ipc_read, mut ipc_write, ack) = connect_and_handshake(&opts, &hello_bytes).await?;
    if !ack.ok {
        return Err(anyhow!(
            "daemon rejected connection: {}",
            ack.error.unwrap_or_else(|| "unknown error".to_string())
        ));
    }

    let trace = TraceLogger::open(opts.trace.as_deref())?;
    let client_id = ack.client_id;

    let trace_in = trace.clone();
    let c2d = tokio::spawn(async move {
        let mut stdin = BufReader::with_capacity(64 * 1024, tokio::io::stdin());
        let mut buf = Vec::with_capacity(16 * 1024);
        loop {
            match read_lsp_frame(&mut stdin, &mut buf).await {
                Ok(true) => {
                    let is_exit = Envelope::parse(&buf)
                        .ok()
                        .is_some_and(|e| e.id.is_none() && e.method.as_deref() == Some("exit"));
                    trace_in.log("proxy", Some(client_id), "P->D", "lsp", None, buf.len());
                    if write_ipc_frame(&mut ipc_write, FrameType::LspMessage, &buf)
                        .await
                        .is_err()
                    {
                        break;
                    }
                    if is_exit {
                        // Clients commonly send `exit` and wait for the server process to terminate
                        // before closing stdin, so close IPC and exit immediately.
                        let _ = ipc_write.shutdown().await;
                        break;
                    }
                }
                Ok(false) => {
                    let _ = ipc_write.shutdown().await;
                    break;
                }
                Err(_) => {
                    let _ = ipc_write.shutdown().await;
                    break;
                }
            }
        }
    });

    let trace_out = trace;
    let d2c = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        let mut buf = Vec::with_capacity(16 * 1024);
        while let Ok(Some(ft)) = read_ipc_frame(&mut ipc_read, &mut buf).await {
            if ft != FrameType::LspMessage {
                continue;
            }
            trace_out.log("proxy", Some(client_id), "D->P", "lsp", None, buf.len());
            if write_lsp_frame(&mut stdout, &buf).await.is_err() {
                break;
            }
        }
    });

    tokio::select! {
        _ = c2d => {
            // Allow any immediate final response (e.g., if shutdown was just sent) to drain briefly.
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        _ = d2c => {}
    }

    Ok(())
}
