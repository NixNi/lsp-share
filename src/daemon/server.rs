use crate::daemon::instance::ActorCmd;
use crate::daemon::pool::Pool;
use crate::ipc::{IpcReadHalf, IpcWriteHalf};
use crate::protocol::frame::{
    read_ipc_frame, write_ipc_frame, ControlRequest, ControlResponse, FrameType, HelloAckPayload,
    HelloMode, HelloPayload, PROTOCOL_VERSION,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, watch};

static NEXT_CLIENT_ID: AtomicU64 = AtomicU64::new(1);

pub async fn handle_connection(
    mut read_half: IpcReadHalf,
    mut write_half: IpcWriteHalf,
    pool: Arc<Pool>,
    shutdown_tx: watch::Sender<bool>,
) {
    let mut buf = Vec::with_capacity(16 * 1024);
    let Ok(Some(FrameType::Hello)) = read_ipc_frame(&mut read_half, &mut buf).await else {
        return;
    };
    let Ok(hello) = serde_json::from_slice::<HelloPayload>(&buf) else {
        return;
    };

    if hello.protocol_version != PROTOCOL_VERSION {
        let ack = HelloAckPayload {
            protocol_version: PROTOCOL_VERSION,
            ok: false,
            client_id: 0,
            error: Some(format!(
                "protocol version mismatch: client={} daemon={}",
                hello.protocol_version, PROTOCOL_VERSION
            )),
        };
        if let Ok(bytes) = serde_json::to_vec(&ack) {
            let _ = write_ipc_frame(&mut write_half, FrameType::HelloAck, &bytes).await;
        }
        return;
    }

    let client_id = NEXT_CLIENT_ID.fetch_add(1, Ordering::Relaxed);
    let ack = HelloAckPayload {
        protocol_version: PROTOCOL_VERSION,
        ok: true,
        client_id,
        error: None,
    };
    let Ok(ack_bytes) = serde_json::to_vec(&ack) else {
        return;
    };
    if write_ipc_frame(&mut write_half, FrameType::HelloAck, &ack_bytes)
        .await
        .is_err()
    {
        return;
    }

    match hello.mode {
        HelloMode::Control => {
            handle_control_session(read_half, write_half, pool, shutdown_tx).await;
        }
        HelloMode::Lsp => {
            handle_lsp_session(client_id, hello, read_half, write_half, pool).await;
        }
    }
}

async fn handle_control_session(
    mut read_half: IpcReadHalf,
    mut write_half: IpcWriteHalf,
    pool: Arc<Pool>,
    shutdown_tx: watch::Sender<bool>,
) {
    let mut buf = Vec::with_capacity(4096);
    while let Ok(Some(ft)) = read_ipc_frame(&mut read_half, &mut buf).await {
        if ft != FrameType::ControlReq {
            continue;
        }
        let resp = match serde_json::from_slice::<ControlRequest>(&buf) {
            Ok(ControlRequest::Status) => ControlResponse::Status(pool.status().await),
            Ok(ControlRequest::Stop {
                instance_id,
                all,
                daemon,
            }) => {
                if daemon {
                    let count = pool.stop_all().await;
                    let _ = shutdown_tx.send(true);
                    ControlResponse::Ok {
                        message: format!("stopped {count} instance(s); daemon shutting down"),
                    }
                } else if all {
                    let count = pool.stop_all().await;
                    ControlResponse::Ok {
                        message: format!("stopped {count} instance(s)"),
                    }
                } else if let Some(id) = instance_id {
                    match pool.stop_instance(&id).await {
                        Ok(msg) => ControlResponse::Ok { message: msg },
                        Err(e) => ControlResponse::Error {
                            message: format!("{e:#}"),
                        },
                    }
                } else {
                    ControlResponse::Error {
                        message: "specify an instance ID, --all, or --daemon".to_string(),
                    }
                }
            }
            Ok(ControlRequest::Restart { instance_id }) => {
                match pool.restart_instance(&instance_id).await {
                    Ok(msg) => ControlResponse::Ok { message: msg },
                    Err(e) => ControlResponse::Error {
                        message: format!("{e:#}"),
                    },
                }
            }
            Err(e) => ControlResponse::Error {
                message: format!("invalid control request: {e}"),
            },
        };
        if let Ok(bytes) = serde_json::to_vec(&resp) {
            if write_ipc_frame(&mut write_half, FrameType::ControlResp, &bytes)
                .await
                .is_err()
            {
                break;
            }
        }
    }
}

async fn handle_lsp_session(
    client_id: u64,
    hello: HelloPayload,
    mut read_half: IpcReadHalf,
    mut write_half: IpcWriteHalf,
    pool: Arc<Pool>,
) {
    let mut buf = Vec::with_capacity(32 * 1024);
    let first_msg = loop {
        match read_ipc_frame(&mut read_half, &mut buf).await {
            Ok(Some(FrameType::LspMessage)) => break buf.clone(),
            Ok(Some(_)) => continue,
            Ok(None) | Err(_) => return,
        }
    };

    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(512);
    let writer_task = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if write_ipc_frame(&mut write_half, FrameType::LspMessage, &msg)
                .await
                .is_err()
            {
                break;
            }
        }
    });

    let Ok((_inst_id, actor_tx)) = pool
        .attach_on_initialize(client_id, hello, first_msg, out_tx)
        .await
    else {
        writer_task.abort();
        return;
    };

    while let Ok(Some(ft)) = read_ipc_frame(&mut read_half, &mut buf).await {
        if ft != FrameType::LspMessage {
            continue;
        }
        if actor_tx
            .send(ActorCmd::ClientMessage {
                client_id,
                payload: buf.clone(),
            })
            .await
            .is_err()
        {
            break;
        }
    }

    let _ = actor_tx
        .send(ActorCmd::ClientDisconnect { client_id })
        .await;
    let _ = writer_task.await;
}
