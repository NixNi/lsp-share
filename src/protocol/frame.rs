use crate::config::{BinaryMatching, ClientRole};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io;
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt,
};

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_FRAME_SIZE: usize = 64 * 1024 * 1024; // 64 MiB

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameType {
    Hello = 0x01,
    HelloAck = 0x02,
    LspMessage = 0x03,
    ControlReq = 0x04,
    ControlResp = 0x05,
}

impl FrameType {
    pub fn from_byte(b: u8) -> io::Result<Self> {
        match b {
            0x01 => Ok(Self::Hello),
            0x02 => Ok(Self::HelloAck),
            0x03 => Ok(Self::LspMessage),
            0x04 => Ok(Self::ControlReq),
            0x05 => Ok(Self::ControlResp),
            other => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown IPC frame type 0x{other:02x}"),
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HelloMode {
    Lsp,
    Control,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloPayload {
    pub protocol_version: u32,
    pub mode: HelloMode,
    pub client_pid: u32,
    #[serde(default)]
    pub role: ClientRole,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub match_by: Option<BinaryMatching>,
    #[serde(default)]
    pub key_override: Option<String>,
    #[serde(default)]
    pub root_override: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloAckPayload {
    pub protocol_version: u32,
    pub ok: bool,
    pub client_id: u64,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ControlRequest {
    Status,
    Stop {
        #[serde(default)]
        instance_id: Option<String>,
        #[serde(default)]
        all: bool,
        #[serde(default)]
        daemon: bool,
    },
    Restart {
        instance_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientStatusInfo {
    pub client_id: u64,
    pub pid: u32,
    pub role: String,
    pub client_name: Option<String>,
    pub requested_command: String,
    pub matched_by_name: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceStatusInfo {
    pub id: String,
    pub server_name: String,
    pub root: String,
    pub binary_path: String,
    pub lsp_pid: Option<u32>,
    pub state: String,
    pub open_docs: usize,
    pub idle_secs: Option<u64>,
    pub uptime_secs: u64,
    pub clients: Vec<ClientStatusInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatusPayload {
    pub daemon_pid: u32,
    pub uptime_secs: u64,
    pub endpoint: String,
    pub instances: Vec<InstanceStatusInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ControlResponse {
    Status(DaemonStatusPayload),
    Ok { message: String },
    Error { message: String },
}

pub async fn write_ipc_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    frame_type: FrameType,
    payload: &[u8],
) -> io::Result<()> {
    if payload.len() > MAX_FRAME_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "IPC payload exceeds MAX_FRAME_SIZE",
        ));
    }
    let mut header = [0u8; 5];
    header[0] = frame_type as u8;
    header[1..5].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    writer.write_all(&header).await?;
    writer.write_all(payload).await?;
    writer.flush().await?;
    Ok(())
}

pub async fn read_ipc_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
) -> io::Result<Option<FrameType>> {
    let mut header = [0u8; 5];
    match reader.read_exact(&mut header).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let frame_type = FrameType::from_byte(header[0])?;
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if len > MAX_FRAME_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("IPC frame length {len} exceeds maximum"),
        ));
    }
    buf.resize(len, 0);
    reader.read_exact(buf).await?;
    Ok(Some(frame_type))
}

/// Reads one LSP `Content-Length` framed JSON-RPC message from a buffered reader.
/// Returns `Ok(false)` on clean EOF before any header line.
pub async fn read_lsp_frame<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
) -> io::Result<bool> {
    let mut content_length: Option<usize> = None;
    let mut line = String::new();
    let mut saw_any_header = false;

    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            if !saw_any_header {
                return Ok(false);
            }
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "EOF while reading LSP headers",
            ));
        }
        saw_any_header = true;
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, val)) = trimmed.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                let len: usize = val.trim().parse().map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("invalid Content-Length header: {val}"),
                    )
                })?;
                if len > MAX_FRAME_SIZE {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("LSP Content-Length {len} exceeds maximum"),
                    ));
                }
                content_length = Some(len);
            }
        }
    }

    let len = content_length.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "missing Content-Length header in LSP frame",
        )
    })?;
    buf.resize(len, 0);
    reader.read_exact(buf).await?;
    Ok(true)
}

pub async fn write_lsp_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    payload: &[u8],
) -> io::Result<()> {
    let header = format!("Content-Length: {}\r\n\r\n", payload.len());
    writer.write_all(header.as_bytes()).await?;
    writer.write_all(payload).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    #[tokio::test]
    async fn lsp_frame_roundtrip_with_extra_headers() {
        let raw = b"Content-Type: application/vscode-jsonrpc; charset=utf-8\r\nContent-Length: 17\r\n\r\n{\"jsonrpc\":\"2.0\"}Content-Length: 2\r\n\r\n{}";
        let mut reader = BufReader::new(&raw[..]);
        let mut buf = Vec::new();
        assert!(read_lsp_frame(&mut reader, &mut buf).await.unwrap());
        assert_eq!(buf, br#"{"jsonrpc":"2.0"}"#);
        assert!(read_lsp_frame(&mut reader, &mut buf).await.unwrap());
        assert_eq!(buf, b"{}");
        assert!(!read_lsp_frame(&mut reader, &mut buf).await.unwrap());
    }

    #[tokio::test]
    async fn ipc_frame_roundtrip() {
        let mut wire = Vec::new();
        write_ipc_frame(&mut wire, FrameType::LspMessage, br#"{"hello":1}"#)
            .await
            .unwrap();
        let mut cursor = &wire[..];
        let mut buf = Vec::new();
        let ft = read_ipc_frame(&mut cursor, &mut buf)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ft, FrameType::LspMessage);
        assert_eq!(buf, br#"{"hello":1}"#);
        assert!(read_ipc_frame(&mut cursor, &mut buf)
            .await
            .unwrap()
            .is_none());
    }
}
