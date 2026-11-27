use crate::config::{ClientRole, OpenConflictPolicy};
use crate::daemon::documents::DocumentManager;
use crate::ipc::{self, ChildTracker};
use crate::paths;
use crate::protocol::frame::{
    read_lsp_frame, write_lsp_frame, ClientStatusInfo, InstanceStatusInfo,
};
use crate::protocol::jsonrpc::{self, Envelope, MessageKind};
use crate::trace::TraceLogger;
use anyhow::{Context, Result};
use serde_json::value::RawValue;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Child;
use tokio::sync::{mpsc, oneshot};

pub struct ClientJoin {
    pub client_id: u64,
    pub pid: u32,
    pub role: ClientRole,
    pub client_name: Option<String>,
    pub requested_command: String,
    pub matched_by_name: bool,
    pub init_bytes: Vec<u8>,
    pub tx: mpsc::Sender<Vec<u8>>,
}

pub enum ActorCmd {
    ClientJoin(ClientJoin),
    ClientMessage {
        client_id: u64,
        payload: Vec<u8>,
    },
    ClientDisconnect {
        client_id: u64,
    },
    GetStatus {
        reply: oneshot::Sender<InstanceStatusInfo>,
    },
    Shutdown {
        reply: oneshot::Sender<()>,
    },
    Restart {
        reply: oneshot::Sender<Result<()>>,
    },
}

pub struct InstanceActorConfig {
    pub id: String,
    pub server_name: String,
    pub root: String,
    pub binary_path: PathBuf,
    pub launch_args: Vec<String>,
    pub env: std::collections::BTreeMap<String, String>,
    pub force_utf16: bool,
    pub idle_timeout_secs: u64,
    pub open_conflict_policy: OpenConflictPolicy,
    pub event_dedup_window_ms: u64,
    pub trace: TraceLogger,
    pub child_tracker: ChildTracker,
    pub on_exit_notify: mpsc::UnboundedSender<String>,
}

enum LifecycleState {
    Initializing {
        primary_init_orig_id: String,
        queued_joins: Vec<ClientJoin>,
    },
    Running,
    Restarting {
        waiters: Vec<oneshot::Sender<Result<()>>>,
    },
    ShuttingDown,
}

struct ConnectedClient {
    client_id: u64,
    pid: u32,
    role: ClientRole,
    client_name: Option<String>,
    requested_command: String,
    matched_by_name: bool,
    tx: mpsc::Sender<Vec<u8>>,
    supports_work_done_progress: bool,
    supports_dynamic_registration: bool,
    supports_configuration: bool,
    workspace_folders: Vec<Value>,
    created_progress_tokens: HashSet<String>,
    shutting_down: bool,
}

struct PendingC2S {
    client_id: u64,
    orig_id_json: String,
    method: String,
}

enum S2CKind {
    WorkspaceConfiguration {
        items: Vec<(Option<String>, Option<String>)>,
    },
    ApplyEdit,
}

struct PendingS2C {
    server_id_json: String,
    routed_client_id: u64,
    kind: S2CKind,
}

struct ChildProcessHandle {
    child: Child,
    pid: Option<u32>,
    stdin_tx: mpsc::Sender<Vec<u8>>,
    stdout_rx: mpsc::Receiver<Option<Vec<u8>>>,
    recent_stderr: Arc<Mutex<VecDeque<String>>>,
}

pub fn spawn_instance_actor(
    cfg: InstanceActorConfig,
    first_client: ClientJoin,
) -> mpsc::Sender<ActorCmd> {
    let (cmd_tx, cmd_rx) = mpsc::channel(512);
    tokio::spawn(async move {
        run_actor(cfg, first_client, cmd_rx).await;
    });
    cmd_tx
}

async fn spawn_lsp_child(cfg: &InstanceActorConfig) -> Result<ChildProcessHandle> {
    let mut cmd = ipc::build_lsp_command(&cfg.binary_path, &cfg.launch_args);
    let native_root = paths::to_native_os_path(&cfg.root);
    if native_root.is_dir() {
        cmd.current_dir(&native_root);
    } else {
        tracing::warn!(
            instance = %cfg.id,
            root = %native_root.display(),
            "workspace root directory does not exist; spawning LSP in daemon cwd"
        );
    }
    for (k, v) in &cfg.env {
        cmd.env(k, v);
    }
    cmd.env("LSP_SHARE_SPAWNED_BY_DAEMON", "1");
    cmd.env_remove("LSP_SHARE_INTERNAL_CLI");
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().with_context(|| {
        format!(
            "failed to spawn LSP binary {} in {}",
            cfg.binary_path.display(),
            native_root.display()
        )
    })?;

    let pid = child.id();
    if let Some(p) = pid {
        if let Err(e) = cfg.child_tracker.track_pid(p) {
            tracing::warn!(instance = %cfg.id, pid = p, "failed to track child pid: {e}");
        }
    }

    let stdin = child.stdin.take().context("missing child stdin")?;
    let stdout = child.stdout.take().context("missing child stdout")?;
    let stderr = child.stderr.take();

    let (stdin_tx, mut stdin_rx) = mpsc::channel::<Vec<u8>>(512);
    tokio::spawn(async move {
        let mut stdin = stdin;
        while let Some(msg) = stdin_rx.recv().await {
            if write_lsp_frame(&mut stdin, &msg).await.is_err() {
                break;
            }
        }
    });

    let (stdout_tx, stdout_rx) = mpsc::channel::<Option<Vec<u8>>>(512);
    tokio::spawn(async move {
        let mut reader = BufReader::with_capacity(64 * 1024, stdout);
        let mut buf = Vec::with_capacity(16 * 1024);
        loop {
            match read_lsp_frame(&mut reader, &mut buf).await {
                Ok(true) => {
                    if stdout_tx.send(Some(buf.clone())).await.is_err() {
                        break;
                    }
                }
                Ok(false) => {
                    let _ = stdout_tx.send(None).await;
                    break;
                }
                Err(e) => {
                    tracing::warn!("error reading LSP stdout frame: {e}");
                    let _ = stdout_tx.send(None).await;
                    break;
                }
            }
        }
    });

    let recent_stderr = Arc::new(Mutex::new(VecDeque::with_capacity(20)));
    if let Some(err_stream) = stderr {
        let inst_id = cfg.id.clone();
        let buf_ref = recent_stderr.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(err_stream);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let trimmed = line.trim_end();
                        if !trimmed.is_empty() {
                            tracing::warn!(instance = %inst_id, "lsp stderr: {trimmed}");
                            if let Ok(mut q) = buf_ref.lock() {
                                if q.len() >= 20 {
                                    q.pop_front();
                                }
                                let capped: String = trimmed.chars().take(512).collect();
                                q.push_back(capped);
                            }
                        }
                    }
                }
            }
        });
    }

    Ok(ChildProcessHandle {
        child,
        pid,
        stdin_tx,
        stdout_rx,
        recent_stderr,
    })
}

async fn collect_child_exit_detail(
    child_opt: &mut Option<ChildProcessHandle>,
    binary_path: &std::path::Path,
) -> String {
    let Some(mut handle) = child_opt.take() else {
        return format!(
            "Underlying language server ({}) exited during initialize",
            binary_path.display()
        );
    };
    drop(handle.stdin_tx);
    let status = tokio::time::timeout(Duration::from_millis(150), handle.child.wait())
        .await
        .ok()
        .and_then(|r| r.ok());
    // Give the stderr reader task a brief tick to finish flushing the final line.
    tokio::time::sleep(Duration::from_millis(20)).await;
    let stderr_joined = handle
        .recent_stderr
        .lock()
        .map(|q| q.iter().cloned().collect::<Vec<_>>().join(" | "))
        .unwrap_or_default();

    let status_desc = match status {
        Some(st) => match st.code() {
            Some(code) => format!("exit code {code}"),
            None => format!("{st}"),
        },
        None => "exited".to_string(),
    };

    if stderr_joined.is_empty() {
        format!(
            "Underlying language server ({}) exited during initialize ({status_desc})",
            binary_path.display()
        )
    } else {
        format!(
            "Underlying language server ({}) exited during initialize ({status_desc}): {stderr_joined}",
            binary_path.display()
        )
    }
}

async fn graceful_stop_child(child_opt: &mut Option<ChildProcessHandle>) {
    let Some(mut handle) = child_opt.take() else {
        return;
    };
    let shutdown_req = json!({
        "jsonrpc": "2.0",
        "id": 9_999_999_u64,
        "method": "shutdown",
        "params": Value::Null
    });
    if let Ok(bytes) = serde_json::to_vec(&shutdown_req) {
        let _ = handle.stdin_tx.send(bytes).await;
    }
    let exit_notif = jsonrpc::build_notification("exit", &Value::Null);
    tokio::time::sleep(Duration::from_millis(80)).await;
    let _ = handle.stdin_tx.send(exit_notif).await;
    drop(handle.stdin_tx);

    let wait_res = tokio::time::timeout(Duration::from_secs(2), handle.child.wait()).await;
    if wait_res.is_err() {
        if let Some(pid) = handle.pid {
            ipc::terminate_child_process(pid, false);
        }
        let wait_term = tokio::time::timeout(Duration::from_secs(1), handle.child.wait()).await;
        if wait_term.is_err() {
            if let Some(pid) = handle.pid {
                ipc::terminate_child_process(pid, true);
            }
            let _ = handle.child.kill().await;
            let _ = handle.child.wait().await;
        }
    }
}

struct ActorState {
    cfg: InstanceActorConfig,
    started_at: Instant,
    idle_since: Option<Instant>,
    lifecycle: LifecycleState,
    child: Option<ChildProcessHandle>,
    clients: HashMap<u64, ConnectedClient>,
    client_order: Vec<u64>,
    docs: DocumentManager,
    next_server_id: u64,
    next_s2c_id: u64,
    pending_c2s: HashMap<u64, PendingC2S>,
    pending_s2c: HashMap<String, PendingS2C>,
    cached_init_params: Option<Value>,
    cached_init_result_raw: Option<Box<RawValue>>,
    dynamic_registrations: HashMap<String, Value>,
    active_progress_tokens: HashMap<String, Value>,
    config_cache: HashMap<(Option<String>, Option<String>), Value>,
    recent_exec_commands: VecDeque<(Instant, u64)>,
    sent_initialized_to_server: bool,
}

impl ActorState {
    fn primary_client_id(&self) -> Option<u64> {
        let editor = self.client_order.iter().find(|&&cid| {
            self.clients
                .get(&cid)
                .is_some_and(|c| !c.shutting_down && c.role == ClientRole::Editor)
        });
        if let Some(&cid) = editor {
            return Some(cid);
        }
        self.client_order
            .iter()
            .find(|&&cid| self.clients.get(&cid).is_some_and(|c| !c.shutting_down))
            .copied()
    }

    fn primary_config_client_id(&self) -> Option<u64> {
        let editor = self.client_order.iter().find(|&&cid| {
            self.clients.get(&cid).is_some_and(|c| {
                !c.shutting_down && c.supports_configuration && c.role == ClientRole::Editor
            })
        });
        if let Some(&cid) = editor {
            return Some(cid);
        }
        self.client_order
            .iter()
            .find(|&&cid| {
                self.clients
                    .get(&cid)
                    .is_some_and(|c| !c.shutting_down && c.supports_configuration)
            })
            .copied()
    }

    fn alloc_server_id(&mut self) -> u64 {
        let id = self.next_server_id;
        self.next_server_id += 1;
        id
    }

    fn alloc_s2c_id(&mut self, ignore: bool) -> String {
        let id = self.next_s2c_id;
        self.next_s2c_id += 1;
        if ignore {
            format!("\"__lspshare_ignore_{id}\"")
        } else {
            format!("\"__lspshare_s2c_{id}\"")
        }
    }

    fn send_to_server(
        &self,
        bytes: Vec<u8>,
        client_id: Option<u64>,
        action: &str,
        method: Option<&str>,
    ) {
        self.cfg
            .trace
            .log(&self.cfg.id, client_id, "D->S", action, method, bytes.len());
        if let Some(ref child) = self.child {
            let _ = child.stdin_tx.try_send(bytes);
        }
    }

    fn send_to_client(&self, client_id: u64, bytes: Vec<u8>, action: &str, method: Option<&str>) {
        self.cfg.trace.log(
            &self.cfg.id,
            Some(client_id),
            "D->C",
            action,
            method,
            bytes.len(),
        );
        if let Some(c) = self.clients.get(&client_id) {
            let _ = c.tx.try_send(bytes);
        }
    }

    fn broadcast_clients<F>(&self, bytes: &[u8], action: &str, method: Option<&str>, mut pred: F)
    where
        F: FnMut(&ConnectedClient) -> bool,
    {
        for &cid in &self.client_order {
            if let Some(client) = self.clients.get(&cid) {
                if !client.shutting_down && pred(client) {
                    self.cfg.trace.log(
                        &self.cfg.id,
                        Some(cid),
                        "D->C",
                        action,
                        method,
                        bytes.len(),
                    );
                    let _ = client.tx.try_send(bytes.to_vec());
                }
            }
        }
    }

    fn extract_client_meta(
        join: &ClientJoin,
    ) -> (ClientRole, bool, bool, bool, Vec<Value>, Option<String>) {
        let Ok(val) = serde_json::from_slice::<Value>(&join.init_bytes) else {
            let role = if join.role == ClientRole::Auto {
                ClientRole::Agent
            } else {
                join.role
            };
            return (
                role,
                false,
                false,
                false,
                Vec::new(),
                join.client_name.clone(),
            );
        };
        let params = val.get("params");
        let detected_name = params
            .and_then(|p| p.get("clientInfo"))
            .and_then(|ci| ci.get("name"))
            .and_then(|n| n.as_str())
            .map(ToString::to_string)
            .or_else(|| join.client_name.clone());

        let role = match join.role {
            ClientRole::Editor => ClientRole::Editor,
            ClientRole::Agent => ClientRole::Agent,
            ClientRole::Auto => infer_role(detected_name.as_deref()),
        };

        let caps = params.and_then(|p| p.get("capabilities"));
        let wdp = caps
            .and_then(|c| c.get("window"))
            .and_then(|w| w.get("workDoneProgress"))
            .and_then(|b| b.as_bool())
            .unwrap_or(false);
        let dyn_reg = caps
            .and_then(|c| c.get("workspace"))
            .and_then(|w| w.get("didChangeConfiguration"))
            .and_then(|d| d.get("dynamicRegistration"))
            .and_then(|b| b.as_bool())
            .unwrap_or(false)
            || caps
                .and_then(|c| c.get("textDocument"))
                .and_then(|td| td.get("formatting"))
                .and_then(|f| f.get("dynamicRegistration"))
                .and_then(|b| b.as_bool())
                .unwrap_or(false);

        let supports_cfg = caps
            .and_then(|c| c.get("workspace"))
            .and_then(|w| w.get("configuration"))
            .and_then(|b| b.as_bool())
            .unwrap_or(false);

        let folders = params
            .and_then(|p| p.get("workspaceFolders"))
            .and_then(|wf| wf.as_array())
            .cloned()
            .unwrap_or_default();

        (role, wdp, dyn_reg, supports_cfg, folders, detected_name)
    }

    fn register_client(&mut self, join: ClientJoin) -> (ConnectedClient, String) {
        let (role, wdp, dyn_reg, supports_cfg, folders, client_name) =
            Self::extract_client_meta(&join);
        let orig_id_json = Envelope::parse(&join.init_bytes)
            .ok()
            .and_then(|e| e.id.map(|r| r.get().to_string()))
            .unwrap_or_else(|| "1".to_string());

        let client = ConnectedClient {
            client_id: join.client_id,
            pid: join.pid,
            role,
            client_name,
            requested_command: join.requested_command,
            matched_by_name: join.matched_by_name,
            tx: join.tx,
            supports_work_done_progress: wdp,
            supports_dynamic_registration: dyn_reg,
            supports_configuration: supports_cfg,
            workspace_folders: folders,
            created_progress_tokens: HashSet::new(),
            shutting_down: false,
        };
        (client, orig_id_json)
    }

    fn handle_join_when_running(&mut self, join: ClientJoin) {
        self.idle_since = None;
        let cid = join.client_id;
        let (client, orig_id_json) = self.register_client(join);
        self.clients.insert(cid, client);
        self.client_order.push(cid);

        if let Some(ref res_raw) = self.cached_init_result_raw {
            let id_rv = RawValue::from_string(orig_id_json)
                .unwrap_or_else(|_| RawValue::from_string("1".to_string()).unwrap());
            let reply = jsonrpc::build_ok_raw_response(&id_rv, res_raw);
            self.send_to_client(cid, reply, "cached_reply", Some("initialize"));
        }
    }

    fn handle_client_message(&mut self, client_id: u64, payload: Vec<u8>) {
        let Ok(env) = Envelope::parse(&payload) else {
            return;
        };
        let method_owned = env.method.as_deref().map(ToString::to_string);
        let method = method_owned.as_deref();
        self.cfg.trace.log(
            &self.cfg.id,
            Some(client_id),
            "C->D",
            "recv",
            method,
            payload.len(),
        );

        match env.kind() {
            MessageKind::Request => {
                let Some(id_raw) = env.id else { return };
                let m = method.unwrap_or("");
                match m {
                    "initialize" => {
                        if let Some(ref res_raw) = self.cached_init_result_raw {
                            let reply = jsonrpc::build_ok_raw_response(id_raw, res_raw);
                            self.send_to_client(
                                client_id,
                                reply,
                                "cached_reply",
                                Some("initialize"),
                            );
                        }
                    }
                    "shutdown" => {
                        if let Some(c) = self.clients.get_mut(&client_id) {
                            c.shutting_down = true;
                        }
                        let reply = jsonrpc::build_ok_null_response(id_raw);
                        self.send_to_client(client_id, reply, "synthetic_reply", Some("shutdown"));
                    }
                    _ => {
                        if m == "workspace/executeCommand" {
                            let now = Instant::now();
                            self.recent_exec_commands.push_back((now, client_id));
                            while let Some((ts, _)) = self.recent_exec_commands.front() {
                                if now.duration_since(*ts) > Duration::from_secs(5) {
                                    self.recent_exec_commands.pop_front();
                                } else {
                                    break;
                                }
                            }
                        }
                        let sid = self.alloc_server_id();
                        let orig_id_json = id_raw.get().to_string();
                        self.pending_c2s.insert(
                            sid,
                            PendingC2S {
                                client_id,
                                orig_id_json,
                                method: m.to_string(),
                            },
                        );
                        let rewritten = jsonrpc::rewrite_request_id(&env, &sid.to_string());
                        self.send_to_server(rewritten, Some(client_id), "forwarded", Some(m));
                    }
                }
            }
            MessageKind::Notification => {
                let m = method.unwrap_or("");
                match m {
                    "initialized" => {
                        if !self.sent_initialized_to_server {
                            self.sent_initialized_to_server = true;
                            self.send_to_server(payload, Some(client_id), "forwarded", Some(m));
                        } else {
                            // Push cached dynamic registrations to newly joined client.
                            let supports_dyn = self
                                .clients
                                .get(&client_id)
                                .is_some_and(|c| c.supports_dynamic_registration);
                            if supports_dyn && !self.dynamic_registrations.is_empty() {
                                let regs: Vec<Value> =
                                    self.dynamic_registrations.values().cloned().collect();
                                let req_id = self.alloc_s2c_id(true);
                                let msg = jsonrpc::build_request(
                                    &req_id,
                                    "client/registerCapability",
                                    &json!({ "registrations": regs }),
                                );
                                self.send_to_client(
                                    client_id,
                                    msg,
                                    "replay_registration",
                                    Some("client/registerCapability"),
                                );
                            }
                        }
                    }
                    "exit" => {
                        self.handle_client_disconnect(client_id);
                    }
                    "$/cancelRequest" => {
                        if let Some(params_raw) = env.params {
                            if let Ok(val) = serde_json::from_str::<Value>(params_raw.get()) {
                                if let Some(orig_id_val) = val.get("id") {
                                    let orig_id_str =
                                        serde_json::to_string(orig_id_val).unwrap_or_default();
                                    let found_sid =
                                        self.pending_c2s.iter().find_map(|(&sid, p)| {
                                            if p.client_id == client_id
                                                && p.orig_id_json == orig_id_str
                                            {
                                                Some(sid)
                                            } else {
                                                None
                                            }
                                        });
                                    if let Some(sid) = found_sid {
                                        let cancel = jsonrpc::build_cancel_notification(sid);
                                        self.send_to_server(
                                            cancel,
                                            Some(client_id),
                                            "forwarded",
                                            Some("$/cancelRequest"),
                                        );
                                    }
                                }
                            }
                        }
                    }
                    "textDocument/didOpen" => {
                        let role = self
                            .clients
                            .get(&client_id)
                            .map(|c| c.role)
                            .unwrap_or(ClientRole::Agent);
                        if let Some(params_raw) = env.params {
                            if let Ok(params_val) = serde_json::from_str::<Value>(params_raw.get())
                            {
                                if let Some(outcome) =
                                    self.docs.handle_did_open(client_id, role, &params_val)
                                {
                                    if let Some(msg) = outcome.to_server {
                                        self.send_to_server(
                                            msg,
                                            Some(client_id),
                                            outcome.action_tag,
                                            Some(m),
                                        );
                                    }
                                    if let Some(replay) = outcome.replay_diagnostics {
                                        self.send_to_client(
                                            client_id,
                                            replay,
                                            "replay_diagnostics",
                                            Some("textDocument/publishDiagnostics"),
                                        );
                                    }
                                }
                            }
                        }
                    }
                    "textDocument/didChange" => {
                        let role = self
                            .clients
                            .get(&client_id)
                            .map(|c| c.role)
                            .unwrap_or(ClientRole::Agent);
                        if let Some(params_raw) = env.params {
                            if let Ok(mut params_val) =
                                serde_json::from_str::<Value>(params_raw.get())
                            {
                                if let Some(outcome) =
                                    self.docs
                                        .handle_did_change(client_id, role, &mut params_val)
                                {
                                    if let Some(msg) = outcome.to_server {
                                        self.send_to_server(
                                            msg,
                                            Some(client_id),
                                            outcome.action_tag,
                                            Some(m),
                                        );
                                    } else {
                                        self.cfg.trace.log(
                                            &self.cfg.id,
                                            Some(client_id),
                                            "C->D",
                                            outcome.action_tag,
                                            Some(m),
                                            payload.len(),
                                        );
                                    }
                                }
                            }
                        }
                    }
                    "textDocument/didSave" => {
                        if let Some(params_raw) = env.params {
                            if let Ok(mut params_val) =
                                serde_json::from_str::<Value>(params_raw.get())
                            {
                                for msg in self.docs.handle_did_save(client_id, &mut params_val) {
                                    self.send_to_server(msg, Some(client_id), "forwarded", Some(m));
                                }
                            }
                        }
                    }
                    "textDocument/didClose" => {
                        if let Some(params_raw) = env.params {
                            if let Ok(params_val) = serde_json::from_str::<Value>(params_raw.get())
                            {
                                if let Some(uri) = params_val
                                    .get("textDocument")
                                    .and_then(|t| t.get("uri"))
                                    .and_then(|u| u.as_str())
                                {
                                    if let Some(msg) = self.docs.handle_did_close(client_id, uri) {
                                        self.send_to_server(
                                            msg,
                                            Some(client_id),
                                            "forwarded",
                                            Some(m),
                                        );
                                    }
                                }
                            }
                        }
                    }
                    "workspace/didChangeWatchedFiles" => {
                        if let Some(params_raw) = env.params {
                            if let Ok(params_val) = serde_json::from_str::<Value>(params_raw.get())
                            {
                                if let Some(msg) = self.docs.dedup_watched_files(&params_val) {
                                    self.send_to_server(msg, Some(client_id), "forwarded", Some(m));
                                }
                            }
                        }
                    }
                    "workspace/didChangeConfiguration" => {
                        self.config_cache.clear();
                        self.send_to_server(payload, Some(client_id), "forwarded", Some(m));
                    }
                    _ => {
                        self.send_to_server(payload, Some(client_id), "forwarded", Some(m));
                    }
                }
            }
            MessageKind::Response => {
                let Some(id_raw) = env.id else { return };
                let id_key = jsonrpc::normalize_id_key(id_raw);
                if id_key.starts_with("\"__lspshare_ignore_") {
                    return;
                }
                if let Some(pending) = self.pending_s2c.remove(&id_key) {
                    if let S2CKind::WorkspaceConfiguration { items } = pending.kind {
                        if let Some(res_raw) = env.result {
                            if let Ok(Value::Array(vals)) =
                                serde_json::from_str::<Value>(res_raw.get())
                            {
                                for (k, v) in items.into_iter().zip(vals) {
                                    self.config_cache.insert(k, v);
                                }
                            }
                        }
                    }
                    let rewritten = jsonrpc::rewrite_response_id(&env, &pending.server_id_json);
                    self.send_to_server(rewritten, Some(client_id), "forwarded_s2c_resp", None);
                }
            }
            MessageKind::Invalid => {}
        }
    }

    fn handle_server_message(&mut self, payload: Vec<u8>) {
        let Ok(env) = Envelope::parse(&payload) else {
            return;
        };
        let method_owned = env.method.as_deref().map(ToString::to_string);
        let method = method_owned.as_deref();
        self.cfg
            .trace
            .log(&self.cfg.id, None, "S->D", "recv", method, payload.len());

        match env.kind() {
            MessageKind::Response => {
                let Some(id_raw) = env.id else { return };
                let sid_num = id_raw.get().trim().parse::<u64>().ok();

                if let LifecycleState::Initializing {
                    primary_init_orig_id,
                    queued_joins,
                } = &mut self.lifecycle
                {
                    if sid_num == Some(1) {
                        let orig_id = primary_init_orig_id.clone();
                        let joins = std::mem::take(queued_joins);
                        self.lifecycle = LifecycleState::Running;
                        if let Some(res_raw) = env.result {
                            self.cached_init_result_raw = Some(res_raw.to_owned());
                        }
                        let primary_cid = *self.client_order.first().unwrap_or(&0);
                        let reply = jsonrpc::rewrite_response_id(&env, &orig_id);
                        self.send_to_client(primary_cid, reply, "forwarded", Some("initialize"));

                        for j in joins {
                            self.handle_join_when_running(j);
                        }
                        return;
                    }
                } else if let LifecycleState::Restarting { waiters } = &mut self.lifecycle {
                    if sid_num == Some(1) {
                        if let Some(res_raw) = env.result {
                            self.cached_init_result_raw = Some(res_raw.to_owned());
                        }
                        let waiters_taken = std::mem::take(waiters);
                        self.lifecycle = LifecycleState::Running;

                        let initialized = jsonrpc::build_notification("initialized", &json!({}));
                        self.send_to_server(
                            initialized,
                            None,
                            "restart_initialized",
                            Some("initialized"),
                        );
                        self.sent_initialized_to_server = true;

                        for open_notif in self.docs.snapshot_open_docs_for_restart() {
                            self.send_to_server(
                                open_notif,
                                None,
                                "restart_reopen",
                                Some("textDocument/didOpen"),
                            );
                        }
                        for w in waiters_taken {
                            let _ = w.send(Ok(()));
                        }
                        return;
                    }
                }

                if let Some(sid) = sid_num {
                    if let Some(pending) = self.pending_c2s.remove(&sid) {
                        if self.clients.contains_key(&pending.client_id) {
                            let rewritten =
                                jsonrpc::rewrite_response_id(&env, &pending.orig_id_json);
                            self.send_to_client(
                                pending.client_id,
                                rewritten,
                                "forwarded",
                                Some(&pending.method),
                            );
                        }
                    }
                }
            }
            MessageKind::Notification => {
                let m = method.unwrap_or("");
                match m {
                    "textDocument/publishDiagnostics" => {
                        if let Some(params_raw) = env.params {
                            if let Ok(val) = serde_json::from_str::<Value>(params_raw.get()) {
                                self.docs.cache_publish_diagnostics(val);
                            }
                        }
                        self.broadcast_clients(&payload, "broadcast", Some(m), |_| true);
                    }
                    "$/progress" => {
                        if let Some(params_raw) = env.params {
                            if let Ok(val) = serde_json::from_str::<Value>(params_raw.get()) {
                                let token_key = val
                                    .get("token")
                                    .map(|t| serde_json::to_string(t).unwrap_or_default())
                                    .unwrap_or_default();
                                let is_end = val
                                    .get("value")
                                    .and_then(|v| v.get("kind"))
                                    .and_then(|k| k.as_str())
                                    == Some("end");

                                let eligible_cids: Vec<u64> = self
                                    .client_order
                                    .iter()
                                    .copied()
                                    .filter(|cid| {
                                        self.clients.get(cid).is_some_and(|c| {
                                            !c.shutting_down && c.supports_work_done_progress
                                        })
                                    })
                                    .collect();

                                for cid in eligible_cids {
                                    let needs_create = self.clients.get(&cid).is_some_and(|c| {
                                        !c.created_progress_tokens.contains(&token_key)
                                    });
                                    if needs_create {
                                        if let Some(token_val) = val.get("token") {
                                            let create_id = self.alloc_s2c_id(true);
                                            let create_req = jsonrpc::build_request(
                                                &create_id,
                                                "window/workDoneProgress/create",
                                                &json!({ "token": token_val }),
                                            );
                                            self.send_to_client(
                                                cid,
                                                create_req,
                                                "synthetic_progress_create",
                                                Some("window/workDoneProgress/create"),
                                            );
                                            if let Some(c) = self.clients.get_mut(&cid) {
                                                c.created_progress_tokens.insert(token_key.clone());
                                            }
                                        }
                                    }
                                    self.send_to_client(cid, payload.clone(), "broadcast", Some(m));
                                }

                                if is_end {
                                    self.active_progress_tokens.remove(&token_key);
                                    for c in self.clients.values_mut() {
                                        c.created_progress_tokens.remove(&token_key);
                                    }
                                }
                            }
                        }
                    }
                    _ => {
                        self.broadcast_clients(&payload, "broadcast", Some(m), |_| true);
                    }
                }
            }
            MessageKind::Request => {
                let Some(id_raw) = env.id else { return };
                let m = method.unwrap_or("");
                match m {
                    "client/registerCapability" => {
                        let ack = jsonrpc::build_ok_null_response(id_raw);
                        self.send_to_server(ack, None, "synthetic_ack", Some(m));
                        if let Some(params_raw) = env.params {
                            if let Ok(val) = serde_json::from_str::<Value>(params_raw.get()) {
                                if let Some(regs) =
                                    val.get("registrations").and_then(|r| r.as_array())
                                {
                                    for r in regs {
                                        if let Some(rid) = r.get("id").and_then(|v| v.as_str()) {
                                            self.dynamic_registrations
                                                .insert(rid.to_string(), r.clone());
                                        }
                                    }
                                }
                                let req_id = self.alloc_s2c_id(true);
                                let fwd = jsonrpc::build_request(&req_id, m, &val);
                                self.broadcast_clients(&fwd, "broadcast", Some(m), |c| {
                                    c.supports_dynamic_registration
                                });
                            }
                        }
                    }
                    "client/unregisterCapability" => {
                        let ack = jsonrpc::build_ok_null_response(id_raw);
                        self.send_to_server(ack, None, "synthetic_ack", Some(m));
                        if let Some(params_raw) = env.params {
                            if let Ok(val) = serde_json::from_str::<Value>(params_raw.get()) {
                                if let Some(unregs) =
                                    val.get("unregisterations").and_then(|r| r.as_array())
                                {
                                    for u in unregs {
                                        if let Some(uid) = u.get("id").and_then(|v| v.as_str()) {
                                            self.dynamic_registrations.remove(uid);
                                        }
                                    }
                                }
                                let req_id = self.alloc_s2c_id(true);
                                let fwd = jsonrpc::build_request(&req_id, m, &val);
                                self.broadcast_clients(&fwd, "broadcast", Some(m), |c| {
                                    c.supports_dynamic_registration
                                });
                            }
                        }
                    }
                    "window/workDoneProgress/create" => {
                        let ack = jsonrpc::build_ok_null_response(id_raw);
                        self.send_to_server(ack, None, "synthetic_ack", Some(m));
                        if let Some(params_raw) = env.params {
                            if let Ok(val) = serde_json::from_str::<Value>(params_raw.get()) {
                                if let Some(tok) = val.get("token") {
                                    let tok_key = serde_json::to_string(tok).unwrap_or_default();
                                    self.active_progress_tokens.insert(tok_key, tok.clone());
                                }
                            }
                        }
                    }
                    "workspace/semanticTokens/refresh"
                    | "workspace/inlayHint/refresh"
                    | "workspace/diagnostic/refresh"
                    | "workspace/codeLens/refresh"
                    | "workspace/inlineValue/refresh" => {
                        let ack = jsonrpc::build_ok_null_response(id_raw);
                        self.send_to_server(ack, None, "synthetic_ack", Some(m));
                        let req_id = self.alloc_s2c_id(true);
                        let fwd = jsonrpc::build_request(&req_id, m, &Value::Null);
                        self.broadcast_clients(&fwd, "broadcast", Some(m), |_| true);
                    }
                    "workspace/workspaceFolders" => {
                        let mut folders = Vec::new();
                        let mut seen = HashSet::new();
                        for &cid in &self.client_order {
                            if let Some(c) = self.clients.get(&cid) {
                                for wf in &c.workspace_folders {
                                    if let Some(uri) = wf.get("uri").and_then(|u| u.as_str()) {
                                        if seen.insert(uri.to_string()) {
                                            folders.push(wf.clone());
                                        }
                                    }
                                }
                            }
                        }
                        if folders.is_empty() {
                            let fallback_uri = path_to_file_uri(&self.cfg.root);
                            folders
                                .push(json!({ "uri": fallback_uri, "name": self.cfg.server_name }));
                        }
                        let resp = jsonrpc::build_ok_response(id_raw, &Value::Array(folders));
                        self.send_to_server(resp, None, "synthetic_reply", Some(m));
                    }
                    "workspace/configuration" => {
                        let items: Vec<(Option<String>, Option<String>)> = env
                            .params
                            .and_then(|p| serde_json::from_str::<Value>(p.get()).ok())
                            .and_then(|v| v.get("items").and_then(|i| i.as_array()).cloned())
                            .unwrap_or_default()
                            .into_iter()
                            .map(|item| {
                                (
                                    item.get("scopeUri")
                                        .and_then(|s| s.as_str())
                                        .map(ToString::to_string),
                                    item.get("section")
                                        .and_then(|s| s.as_str())
                                        .map(ToString::to_string),
                                )
                            })
                            .collect();

                        if let Some(primary_cid) = self.primary_config_client_id() {
                            let s2c_id = self.alloc_s2c_id(false);
                            self.pending_s2c.insert(
                                s2c_id.clone(),
                                PendingS2C {
                                    server_id_json: id_raw.get().to_string(),
                                    routed_client_id: primary_cid,
                                    kind: S2CKind::WorkspaceConfiguration { items },
                                },
                            );
                            let fwd = jsonrpc::rewrite_request_id(&env, &s2c_id);
                            self.send_to_client(primary_cid, fwd, "routed_s2c", Some(m));
                        } else {
                            let cached_arr: Vec<Value> = items
                                .iter()
                                .map(|k| self.config_cache.get(k).cloned().unwrap_or(Value::Null))
                                .collect();
                            let resp =
                                jsonrpc::build_ok_response(id_raw, &Value::Array(cached_arr));
                            self.send_to_server(resp, None, "cached_config_reply", Some(m));
                        }
                    }
                    "workspace/applyEdit" => {
                        let target = self
                            .recent_exec_commands
                            .back()
                            .map(|(_, cid)| *cid)
                            .filter(|cid| self.clients.get(cid).is_some_and(|c| !c.shutting_down))
                            .or_else(|| self.primary_client_id());

                        if let Some(cid) = target {
                            let s2c_id = self.alloc_s2c_id(false);
                            self.pending_s2c.insert(
                                s2c_id.clone(),
                                PendingS2C {
                                    server_id_json: id_raw.get().to_string(),
                                    routed_client_id: cid,
                                    kind: S2CKind::ApplyEdit,
                                },
                            );
                            let fwd = jsonrpc::rewrite_request_id(&env, &s2c_id);
                            self.send_to_client(cid, fwd, "routed_s2c", Some(m));
                        } else {
                            let resp = jsonrpc::build_ok_response(
                                id_raw,
                                &json!({ "applied": false, "failureReason": "no active client" }),
                            );
                            self.send_to_server(resp, None, "synthetic_reply", Some(m));
                        }
                    }
                    _ => {
                        if let Some(cid) = self.primary_client_id() {
                            let s2c_id = self.alloc_s2c_id(false);
                            self.pending_s2c.insert(
                                s2c_id.clone(),
                                PendingS2C {
                                    server_id_json: id_raw.get().to_string(),
                                    routed_client_id: cid,
                                    kind: S2CKind::ApplyEdit,
                                },
                            );
                            let fwd = jsonrpc::rewrite_request_id(&env, &s2c_id);
                            self.send_to_client(cid, fwd, "routed_s2c", Some(m));
                        } else {
                            let resp = jsonrpc::build_ok_null_response(id_raw);
                            self.send_to_server(resp, None, "synthetic_reply", Some(m));
                        }
                    }
                }
            }
            MessageKind::Invalid => {}
        }
    }

    fn handle_client_disconnect(&mut self, client_id: u64) {
        if self.clients.remove(&client_id).is_none() {
            return;
        }
        self.client_order.retain(|&c| c != client_id);

        // Clean up any open docs whose refcount reaches 0.
        for close_msg in self.docs.handle_client_disconnect(client_id) {
            self.send_to_server(
                close_msg,
                Some(client_id),
                "cleanup_close",
                Some("textDocument/didClose"),
            );
        }

        // Cancel in-flight requests owned by this client.
        let orphan_sids: Vec<u64> = self
            .pending_c2s
            .iter()
            .filter_map(|(&sid, p)| {
                if p.client_id == client_id {
                    Some(sid)
                } else {
                    None
                }
            })
            .collect();
        for sid in orphan_sids {
            self.pending_c2s.remove(&sid);
            let cancel = jsonrpc::build_cancel_notification(sid);
            self.send_to_server(
                cancel,
                Some(client_id),
                "cleanup_cancel",
                Some("$/cancelRequest"),
            );
        }

        // Resolve any pending server->client requests that were routed to this client.
        let orphan_s2c: Vec<String> = self
            .pending_s2c
            .iter()
            .filter_map(|(k, p)| {
                if p.routed_client_id == client_id {
                    Some(k.clone())
                } else {
                    None
                }
            })
            .collect();
        for k in orphan_s2c {
            if let Some(p) = self.pending_s2c.remove(&k) {
                let id_rv = RawValue::from_string(p.server_id_json)
                    .unwrap_or_else(|_| RawValue::from_string("null".to_string()).unwrap());
                let fallback = match p.kind {
                    S2CKind::WorkspaceConfiguration { items } => {
                        let arr: Vec<Value> = items
                            .iter()
                            .map(|key| self.config_cache.get(key).cloned().unwrap_or(Value::Null))
                            .collect();
                        jsonrpc::build_ok_response(&id_rv, &Value::Array(arr))
                    }
                    S2CKind::ApplyEdit => {
                        jsonrpc::build_ok_response(&id_rv, &json!({ "applied": false }))
                    }
                };
                self.send_to_server(fallback, Some(client_id), "cleanup_s2c_fallback", None);
            }
        }

        if self.clients.is_empty() {
            self.idle_since = Some(Instant::now());
        }
    }

    fn status_info(&self) -> InstanceStatusInfo {
        let state = match &self.lifecycle {
            LifecycleState::Initializing { .. } => "initializing",
            LifecycleState::Running => {
                if self.clients.is_empty() {
                    "idle"
                } else {
                    "active"
                }
            }
            LifecycleState::Restarting { .. } => "restarting",
            LifecycleState::ShuttingDown => "shutting_down",
        };
        let clients = self
            .client_order
            .iter()
            .filter_map(|cid| self.clients.get(cid))
            .map(|c| ClientStatusInfo {
                client_id: c.client_id,
                pid: c.pid,
                role: match c.role {
                    ClientRole::Editor => "editor".to_string(),
                    ClientRole::Agent => "agent".to_string(),
                    ClientRole::Auto => "auto".to_string(),
                },
                client_name: c.client_name.clone(),
                requested_command: c.requested_command.clone(),
                matched_by_name: c.matched_by_name,
            })
            .collect();

        InstanceStatusInfo {
            id: self.cfg.id.clone(),
            server_name: self.cfg.server_name.clone(),
            root: self.cfg.root.clone(),
            binary_path: self.cfg.binary_path.display().to_string(),
            lsp_pid: self.child.as_ref().and_then(|c| c.pid),
            state: state.to_string(),
            open_docs: self.docs.open_doc_count(),
            idle_secs: self.idle_since.map(|t| t.elapsed().as_secs()),
            uptime_secs: self.started_at.elapsed().as_secs(),
            clients,
        }
    }
}

async fn run_actor(
    cfg: InstanceActorConfig,
    first_client: ClientJoin,
    mut cmd_rx: mpsc::Receiver<ActorCmd>,
) {
    let mut state = ActorState {
        docs: DocumentManager::new(cfg.open_conflict_policy, cfg.event_dedup_window_ms),
        cfg,
        started_at: Instant::now(),
        idle_since: None,
        lifecycle: LifecycleState::Initializing {
            primary_init_orig_id: "1".to_string(),
            queued_joins: Vec::new(),
        },
        child: None,
        clients: HashMap::new(),
        client_order: Vec::new(),
        next_server_id: 2,
        next_s2c_id: 1,
        pending_c2s: HashMap::new(),
        pending_s2c: HashMap::new(),
        cached_init_params: None,
        cached_init_result_raw: None,
        dynamic_registrations: HashMap::new(),
        active_progress_tokens: HashMap::new(),
        config_cache: HashMap::new(),
        recent_exec_commands: VecDeque::new(),
        sent_initialized_to_server: false,
    };

    let first_cid = first_client.client_id;
    let first_init_bytes = first_client.init_bytes.clone();
    let (client_struct, orig_id_json) = state.register_client(first_client);
    state.clients.insert(first_cid, client_struct);
    state.client_order.push(first_cid);
    state.lifecycle = LifecycleState::Initializing {
        primary_init_orig_id: orig_id_json.clone(),
        queued_joins: Vec::new(),
    };

    let init_params = prepare_init_params(&first_init_bytes, state.cfg.force_utf16);
    state.cached_init_params = Some(init_params.clone());

    match spawn_lsp_child(&state.cfg).await {
        Ok(handle) => {
            state.child = Some(handle);
            let init_req = jsonrpc::build_request("1", "initialize", &init_params);
            state.send_to_server(init_req, Some(first_cid), "forwarded", Some("initialize"));
        }
        Err(e) => {
            tracing::error!(instance = %state.cfg.id, "failed to spawn LSP child: {e:#}");
            let id_rv = RawValue::from_string(orig_id_json)
                .unwrap_or_else(|_| RawValue::from_string("1".to_string()).unwrap());
            let err_msg = jsonrpc::build_error_response(&id_rv, -32603, &format!("{e:#}"));
            state.send_to_client(first_cid, err_msg, "spawn_error", Some("initialize"));
            let _ = state.cfg.on_exit_notify.send(state.cfg.id.clone());
            return;
        }
    }

    let mut idle_ticker = tokio::time::interval(Duration::from_secs(1));
    idle_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = idle_ticker.tick() => {
                if state.cfg.idle_timeout_secs > 0 {
                    if let Some(since) = state.idle_since {
                        if since.elapsed() >= Duration::from_secs(state.cfg.idle_timeout_secs)
                            && state.clients.is_empty()
                        {
                            tracing::info!(instance = %state.cfg.id, "idle timeout reached, shutting down LSP");
                            state.lifecycle = LifecycleState::ShuttingDown;
                            graceful_stop_child(&mut state.child).await;
                            break;
                        }
                    }
                }
            }
            cmd_opt = cmd_rx.recv() => {
                let Some(cmd) = cmd_opt else {
                    graceful_stop_child(&mut state.child).await;
                    break;
                };
                match cmd {
                    ActorCmd::ClientJoin(join) => {
                        match &mut state.lifecycle {
                            LifecycleState::Initializing { queued_joins, .. } => {
                                queued_joins.push(join);
                            }
                            LifecycleState::Running => {
                                state.handle_join_when_running(join);
                            }
                            LifecycleState::Restarting { .. } => {
                                // Attach after restart completes by letting join happen on Running state;
                                // queue is drained via Running path below if needed, or send error if shutting down.
                                state.handle_join_when_running(join);
                            }
                            LifecycleState::ShuttingDown => {
                                let id_rv = Envelope::parse(&join.init_bytes)
                                    .ok()
                                    .and_then(|e| e.id.map(|r| r.to_owned()))
                                    .unwrap_or_else(|| RawValue::from_string("1".to_string()).unwrap());
                                let err = jsonrpc::build_error_response(&id_rv, -32603, "LSP instance is shutting down");
                                let _ = join.tx.try_send(err);
                            }
                        }
                    }
                    ActorCmd::ClientMessage { client_id, payload } => {
                        state.handle_client_message(client_id, payload);
                    }
                    ActorCmd::ClientDisconnect { client_id } => {
                        state.handle_client_disconnect(client_id);
                    }
                    ActorCmd::GetStatus { reply } => {
                        let _ = reply.send(state.status_info());
                    }
                    ActorCmd::Shutdown { reply } => {
                        state.lifecycle = LifecycleState::ShuttingDown;
                        fail_all_pending(&mut state);
                        state.clients.clear();
                        state.client_order.clear();
                        graceful_stop_child(&mut state.child).await;
                        let _ = reply.send(());
                        break;
                    }
                    ActorCmd::Restart { reply } => {
                        fail_all_pending(&mut state);
                        state.pending_s2c.clear();
                        graceful_stop_child(&mut state.child).await;
                        state.sent_initialized_to_server = false;

                        match spawn_lsp_child(&state.cfg).await {
                            Ok(new_handle) => {
                                state.child = Some(new_handle);
                                state.lifecycle = LifecycleState::Restarting {
                                    waiters: vec![reply],
                                };
                                let params = state.cached_init_params.clone().unwrap_or_else(|| json!({}));
                                let init_req = jsonrpc::build_request("1", "initialize", &params);
                                state.send_to_server(init_req, None, "restart_initialize", Some("initialize"));
                            }
                            Err(e) => {
                                let _ = reply.send(Err(e));
                                break;
                            }
                        }
                    }
                }
            }
            stdout_msg = async {
                match &mut state.child {
                    Some(c) => c.stdout_rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match stdout_msg {
                    Some(Some(payload)) => {
                        state.handle_server_message(payload);
                    }
                    Some(None) | None => {
                        let detail =
                            collect_child_exit_detail(&mut state.child, &state.cfg.binary_path)
                                .await;
                        tracing::error!(instance = %state.cfg.id, "{detail}");
                        let prev_lifecycle =
                            std::mem::replace(&mut state.lifecycle, LifecycleState::ShuttingDown);
                        match prev_lifecycle {
                            LifecycleState::Initializing {
                                primary_init_orig_id,
                                queued_joins,
                            } => {
                                let id_rv = RawValue::from_string(primary_init_orig_id)
                                    .unwrap_or_else(|_| RawValue::from_string("1".to_string()).unwrap());
                                let err = jsonrpc::build_error_response(&id_rv, -32603, &detail);
                                if let Some(&cid) = state.client_order.first() {
                                    state.send_to_client(cid, err, "init_exit_error", Some("initialize"));
                                }
                                for j in queued_joins {
                                    let jid = Envelope::parse(&j.init_bytes)
                                        .ok()
                                        .and_then(|e| e.id.map(|r| r.to_owned()))
                                        .unwrap_or_else(|| RawValue::from_string("1".to_string()).unwrap());
                                    let jerr = jsonrpc::build_error_response(&jid, -32603, &detail);
                                    let _ = j.tx.try_send(jerr);
                                }
                            }
                            LifecycleState::Restarting { waiters } => {
                                for w in waiters {
                                    let _ = w.send(Err(anyhow::anyhow!("{detail}")));
                                }
                            }
                            LifecycleState::Running | LifecycleState::ShuttingDown => {}
                        }
                        fail_all_pending(&mut state);
                        state.clients.clear();
                        state.client_order.clear();
                        break;
                    }
                }
            }
        }
    }

    let _ = state.cfg.on_exit_notify.send(state.cfg.id.clone());
}

fn fail_all_pending(state: &mut ActorState) {
    let entries: Vec<(u64, PendingC2S)> = state.pending_c2s.drain().collect();
    for (_, p) in entries {
        let id_rv = RawValue::from_string(p.orig_id_json)
            .unwrap_or_else(|_| RawValue::from_string("null".to_string()).unwrap());
        let err =
            jsonrpc::build_error_response(&id_rv, -32603, "Underlying language server exited");
        state.send_to_client(p.client_id, err, "server_exit_error", Some(&p.method));
    }
}

fn infer_role(client_name: Option<&str>) -> ClientRole {
    let Some(name) = client_name else {
        return ClientRole::Agent;
    };
    let lower = name.to_ascii_lowercase();
    for marker in [
        "visual studio code",
        "vscode",
        "code",
        "neovim",
        "nvim",
        "zed",
        "sublime",
        "helix",
        "emacs",
        "intellij",
        "cursor",
        "windsurf",
        "antigravity",
    ] {
        if lower.contains(marker) {
            return ClientRole::Editor;
        }
    }
    ClientRole::Agent
}

fn prepare_init_params(init_bytes: &[u8], force_utf16: bool) -> Value {
    let mut params = serde_json::from_slice::<Value>(init_bytes)
        .ok()
        .and_then(|v| v.get("params").cloned())
        .unwrap_or_else(|| json!({}));

    let rich_caps = json!({
        "workspace": {
            "applyEdit": true,
            "configuration": true,
            "workspaceFolders": true,
            "didChangeConfiguration": { "dynamicRegistration": true },
            "didChangeWatchedFiles": { "dynamicRegistration": true },
            "semanticTokens": { "refreshSupport": true },
            "inlayHint": { "refreshSupport": true },
            "diagnostic": { "refreshSupport": true }
        },
        "textDocument": {
            "synchronization": { "dynamicRegistration": true, "willSave": true, "didSave": true },
            "completion": {
                "completionItem": {
                    "snippetSupport": true,
                    "documentationFormat": ["markdown", "plaintext"],
                    "resolveSupport": { "properties": ["documentation", "detail", "additionalTextEdits"] }
                }
            },
            "hover": { "contentFormat": ["markdown", "plaintext"] },
            "signatureHelp": { "signatureInformation": { "documentationFormat": ["markdown", "plaintext"] } },
            "definition": { "linkSupport": true },
            "typeDefinition": { "linkSupport": true },
            "implementation": { "linkSupport": true },
            "references": {},
            "documentHighlight": {},
            "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
            "codeAction": {
                "codeActionLiteralSupport": {
                    "codeActionKind": {
                        "valueSet": ["quickfix", "refactor", "source", "source.organizeImports"]
                    }
                }
            },
            "formatting": {},
            "rangeFormatting": {},
            "rename": { "prepareSupport": true },
            "inlayHint": {},
            "diagnostic": {},
            "publishDiagnostics": {
                "relatedInformation": true,
                "versionSupport": true,
                "tagSupport": { "valueSet": [1, 2] }
            }
        },
        "window": {
            "workDoneProgress": true
        }
    });

    if let Some(obj) = params.as_object_mut() {
        // Rewrite processId to the daemon's PID so the shared LSP doesn't exit
        // if the first connecting client terminates while other clients remain active.
        obj.insert("processId".to_string(), json!(std::process::id()));
        let caps = obj.entry("capabilities").or_insert_with(|| json!({}));
        merge_missing_json(caps, &rich_caps);
        if force_utf16 {
            if let Some(caps_obj) = caps.as_object_mut() {
                let general = caps_obj.entry("general").or_insert_with(|| json!({}));
                if let Some(gen_obj) = general.as_object_mut() {
                    gen_obj.insert("positionEncodings".to_string(), json!(["utf-16"]));
                }
                caps_obj.insert("offsetEncoding".to_string(), json!(["utf-16"]));
            }
        }
    }

    params
}

/// Recursively fills missing object fields in `target` from `defaults` without overwriting
/// values explicitly supplied by the connecting client.
fn merge_missing_json(target: &mut Value, defaults: &Value) {
    if let (Some(t_map), Some(d_map)) = (target.as_object_mut(), defaults.as_object()) {
        for (k, dv) in d_map {
            match t_map.get_mut(k) {
                Some(tv) if tv.is_object() && dv.is_object() => {
                    merge_missing_json(tv, dv);
                }
                None => {
                    t_map.insert(k.clone(), dv.clone());
                }
                _ => {}
            }
        }
    }
}

fn path_to_file_uri(path: &str) -> String {
    let norm = path.replace('\\', "/");
    if norm.starts_with('/') {
        format!("file://{norm}")
    } else {
        format!("file:///{norm}")
    }
}
