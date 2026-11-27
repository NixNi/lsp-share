use crate::protocol::frame::{read_lsp_frame, write_lsp_frame};
use crate::protocol::jsonrpc::{self, Envelope, MessageKind};
use anyhow::Result;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use tokio::io::BufReader;

/// Built-in deterministic mock LSP server used by cross-platform integration tests.
/// Logs every received LSP method and lifecycle event as JSONL to `log_file`.
pub async fn run_mock_lsp(log_file: PathBuf, label: Option<String>) -> Result<()> {
    let mut stdin = BufReader::new(tokio::io::stdin());
    let mut stdout = tokio::io::stdout();
    let mut buf = Vec::with_capacity(8192);
    let mut docs: HashMap<String, (i64, String)> = HashMap::new();
    let pid = std::process::id();

    let append_log = |entry: Value| {
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&log_file) {
            let _ = writeln!(f, "{entry}");
        }
    };

    append_log(json!({ "event": "started", "pid": pid, "label": label }));

    while read_lsp_frame(&mut stdin, &mut buf).await? {
        let Ok(env) = Envelope::parse(&buf) else {
            continue;
        };
        let method = env.method.as_deref().unwrap_or("");

        append_log(json!({
            "event": "msg",
            "pid": pid,
            "kind": format!("{:?}", env.kind()),
            "method": method,
            "id": env.id.map(|r| r.get().to_string()),
            "params": env.params.and_then(|p| serde_json::from_str::<Value>(p.get()).ok()),
        }));

        match env.kind() {
            MessageKind::Request => {
                let Some(id_raw) = env.id else { continue };
                match method {
                    "initialize" => {
                        let res = json!({
                            "capabilities": {
                                "textDocumentSync": 2,
                                "hoverProvider": true,
                                "completionProvider": {}
                            },
                            "serverInfo": {
                                "name": "mock-lsp",
                                "pid": pid,
                                "label": label
                            }
                        });
                        let reply = jsonrpc::build_ok_response(id_raw, &res);
                        write_lsp_frame(&mut stdout, &reply).await?;
                    }
                    "shutdown" => {
                        let reply = jsonrpc::build_ok_null_response(id_raw);
                        write_lsp_frame(&mut stdout, &reply).await?;
                    }
                    "textDocument/hover" => {
                        let params_val = env
                            .params
                            .and_then(|p| serde_json::from_str::<Value>(p.get()).ok())
                            .unwrap_or_default();
                        let uri = params_val
                            .get("textDocument")
                            .and_then(|t| t.get("uri"))
                            .and_then(|u| u.as_str())
                            .unwrap_or("");
                        let doc_text = docs.get(uri).map(|(_, t)| t.as_str()).unwrap_or("");
                        let reply = jsonrpc::build_ok_response(
                            id_raw,
                            &json!({
                                "contents": format!("pid={pid};len={};text={doc_text}", doc_text.len())
                            }),
                        );
                        write_lsp_frame(&mut stdout, &reply).await?;
                    }
                    "mock/getConfig" => {
                        let cfg_req = jsonrpc::build_request(
                            "9001",
                            "workspace/configuration",
                            &json!({
                                "items": [{ "section": "python.pythonPath" }]
                            }),
                        );
                        write_lsp_frame(&mut stdout, &cfg_req).await?;

                        // Wait for workspace/configuration response from daemon/client
                        let mut resp_buf = Vec::new();
                        if read_lsp_frame(&mut stdin, &mut resp_buf).await? {
                            let resp_val: Value =
                                serde_json::from_slice(&resp_buf).unwrap_or_default();
                            let reply = jsonrpc::build_ok_response(
                                id_raw,
                                &json!({ "config": resp_val["result"] }),
                            );
                            write_lsp_frame(&mut stdout, &reply).await?;
                        }
                    }
                    "mock/slowEcho" => {
                        let params_val = env
                            .params
                            .and_then(|p| serde_json::from_str::<Value>(p.get()).ok())
                            .unwrap_or_default();
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                        let reply = jsonrpc::build_ok_response(
                            id_raw,
                            &json!({ "echo": params_val, "pid": pid }),
                        );
                        write_lsp_frame(&mut stdout, &reply).await?;
                    }
                    _ => {
                        let reply = jsonrpc::build_ok_null_response(id_raw);
                        write_lsp_frame(&mut stdout, &reply).await?;
                    }
                }
            }
            MessageKind::Notification => match method {
                "textDocument/didOpen" => {
                    if let Some(p) = env
                        .params
                        .and_then(|p| serde_json::from_str::<Value>(p.get()).ok())
                    {
                        if let Some(td) = p.get("textDocument") {
                            let uri = td.get("uri").and_then(|u| u.as_str()).unwrap_or("");
                            let ver = td.get("version").and_then(|v| v.as_i64()).unwrap_or(1);
                            let text = td.get("text").and_then(|t| t.as_str()).unwrap_or("");
                            docs.insert(uri.to_string(), (ver, text.to_string()));

                            let diag = jsonrpc::build_notification(
                                "textDocument/publishDiagnostics",
                                &json!({
                                    "uri": uri,
                                    "diagnostics": [{
                                        "range": {
                                            "start": { "line": 0, "character": 0 },
                                            "end": { "line": 0, "character": 1 }
                                        },
                                        "severity": 2,
                                        "message": format!("mock-diag-pid-{pid}")
                                    }]
                                }),
                            );
                            write_lsp_frame(&mut stdout, &diag).await?;
                        }
                    }
                }
                "textDocument/didChange" => {
                    if let Some(p) = env
                        .params
                        .and_then(|p| serde_json::from_str::<Value>(p.get()).ok())
                    {
                        let uri = p
                            .get("textDocument")
                            .and_then(|t| t.get("uri"))
                            .and_then(|u| u.as_str())
                            .unwrap_or("");
                        let ver = p
                            .get("textDocument")
                            .and_then(|t| t.get("version"))
                            .and_then(|v| v.as_i64())
                            .unwrap_or(1);
                        if let Some(changes) = p.get("contentChanges").and_then(|c| c.as_array()) {
                            if let Some(last) = changes.last() {
                                if last.get("range").is_none() {
                                    if let Some(txt) = last.get("text").and_then(|t| t.as_str()) {
                                        docs.insert(uri.to_string(), (ver, txt.to_string()));
                                    }
                                }
                            }
                        }
                    }
                }
                "textDocument/didClose" => {
                    if let Some(p) = env
                        .params
                        .and_then(|p| serde_json::from_str::<Value>(p.get()).ok())
                    {
                        if let Some(uri) = p
                            .get("textDocument")
                            .and_then(|t| t.get("uri"))
                            .and_then(|u| u.as_str())
                        {
                            docs.remove(uri);
                        }
                    }
                }
                "exit" => break,
                _ => {}
            },
            _ => {}
        }
    }

    Ok(())
}
