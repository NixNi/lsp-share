use crate::config::ClientRole;
use crate::ipc;
use crate::paths;
use crate::protocol::frame::{
    read_ipc_frame, write_ipc_frame, ControlRequest, ControlResponse, FrameType, HelloAckPayload,
    HelloMode, HelloPayload, PROTOCOL_VERSION,
};
use anyhow::{anyhow, Context, Result};
use std::collections::BTreeMap;

async fn send_control_request(req: ControlRequest) -> Result<Option<ControlResponse>> {
    let endpoint = paths::ipc_endpoint();
    let Ok((mut read_half, mut write_half)) = ipc::try_connect(&endpoint).await else {
        return Ok(None);
    };

    let hello = HelloPayload {
        protocol_version: PROTOCOL_VERSION,
        mode: HelloMode::Control,
        client_pid: std::process::id(),
        role: ClientRole::Auto,
        command: String::new(),
        args: Vec::new(),
        cwd: String::new(),
        env: BTreeMap::new(),
        match_by: None,
        key_override: None,
        root_override: None,
    };
    let hello_bytes = serde_json::to_vec(&hello)?;
    write_ipc_frame(&mut write_half, FrameType::Hello, &hello_bytes).await?;

    let mut buf = Vec::with_capacity(4096);
    let Some(FrameType::HelloAck) = read_ipc_frame(&mut read_half, &mut buf).await? else {
        return Err(anyhow!("unexpected handshake response from daemon"));
    };
    let ack: HelloAckPayload = serde_json::from_slice(&buf)?;
    if !ack.ok {
        return Err(anyhow!(
            "daemon rejected control connection: {}",
            ack.error.unwrap_or_default()
        ));
    }

    let req_bytes = serde_json::to_vec(&req)?;
    write_ipc_frame(&mut write_half, FrameType::ControlReq, &req_bytes).await?;

    let Some(FrameType::ControlResp) = read_ipc_frame(&mut read_half, &mut buf).await? else {
        return Err(anyhow!("daemon closed control connection without response"));
    };
    let resp: ControlResponse =
        serde_json::from_slice(&buf).context("failed to parse control response")?;
    Ok(Some(resp))
}

pub async fn run_status(json_output: bool) -> Result<()> {
    match send_control_request(ControlRequest::Status).await? {
        None => {
            if json_output {
                println!(r#"{{"running":false,"instances":[]}}"#);
            } else {
                println!(
                    "lsp-share daemon is not running (endpoint: {})",
                    paths::ipc_endpoint_display()
                );
            }
            Ok(())
        }
        Some(ControlResponse::Status(status)) => {
            if json_output {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                println!(
                    "Daemon: PID {} | uptime {}s | endpoint {}",
                    status.daemon_pid, status.uptime_secs, status.endpoint
                );
                if status.instances.is_empty() {
                    println!("No active LSP instances.");
                } else {
                    println!(
                        "{:<22} {:<16} {:<8} {:<8} {:<6} ROOT",
                        "ID", "SERVER", "PID", "CLIENTS", "DOCS"
                    );
                    for inst in &status.instances {
                        let pid_str = inst
                            .lsp_pid
                            .map(|p| p.to_string())
                            .unwrap_or_else(|| "-".to_string());
                        println!(
                            "{:<22} {:<16} {:<8} {:<8} {:<6} {} ({})",
                            inst.id,
                            inst.server_name,
                            pid_str,
                            inst.clients.len(),
                            inst.open_docs,
                            inst.root,
                            inst.state
                        );
                        for c in &inst.clients {
                            let name = c.client_name.as_deref().unwrap_or("-");
                            let note = if c.matched_by_name {
                                " (matched by name)"
                            } else {
                                ""
                            };
                            println!(
                                "  └─ #{} [{}] {} (pid {}) cmd={}{}",
                                c.client_id, c.role, name, c.pid, c.requested_command, note
                            );
                        }
                    }
                }
            }
            Ok(())
        }
        Some(ControlResponse::Error { message }) => Err(anyhow!("{message}")),
        Some(ControlResponse::Ok { message }) => {
            println!("{message}");
            Ok(())
        }
    }
}

pub async fn run_stop(instance_id: Option<String>, all: bool, daemon: bool) -> Result<()> {
    match send_control_request(ControlRequest::Stop {
        instance_id,
        all,
        daemon,
    })
    .await?
    {
        None => {
            println!("lsp-share daemon is not running");
            Ok(())
        }
        Some(ControlResponse::Ok { message }) => {
            println!("{message}");
            Ok(())
        }
        Some(ControlResponse::Error { message }) => Err(anyhow!("{message}")),
        Some(ControlResponse::Status(_)) => Ok(()),
    }
}

pub async fn run_restart(instance_id: String) -> Result<()> {
    match send_control_request(ControlRequest::Restart { instance_id }).await? {
        None => Err(anyhow!("lsp-share daemon is not running")),
        Some(ControlResponse::Ok { message }) => {
            println!("{message}");
            Ok(())
        }
        Some(ControlResponse::Error { message }) => Err(anyhow!("{message}")),
        Some(ControlResponse::Status(_)) => Ok(()),
    }
}
