use crate::config::{ArgMatching, BinaryMatching, Config, EffectiveServerSettings};
use crate::daemon::instance::{spawn_instance_actor, ActorCmd, ClientJoin, InstanceActorConfig};
use crate::ipc::ChildTracker;
use crate::paths;
use crate::protocol::frame::{DaemonStatusPayload, HelloPayload, InstanceStatusInfo};
use crate::trace::TraceLogger;
use anyhow::{anyhow, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, oneshot, Mutex};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct InstanceKey {
    pub server_key: String,
    pub root: String,
    pub semantic_args_key: String,
    pub env_key: String,
}

struct InstanceEntry {
    id: String,
    binary_path: PathBuf,
    tx: mpsc::Sender<ActorCmd>,
}

pub struct Pool {
    config: Config,
    started_at: Instant,
    trace: TraceLogger,
    child_tracker: ChildTracker,
    instances: Mutex<HashMap<InstanceKey, InstanceEntry>>,
    id_to_key: Mutex<HashMap<String, InstanceKey>>,
    exit_tx: mpsc::UnboundedSender<String>,
}

impl Pool {
    pub fn new(
        config: Config,
        trace: TraceLogger,
        child_tracker: ChildTracker,
    ) -> (Arc<Self>, mpsc::UnboundedReceiver<String>) {
        let (exit_tx, exit_rx) = mpsc::unbounded_channel();
        let pool = Arc::new(Self {
            config,
            started_at: Instant::now(),
            trace,
            child_tracker,
            instances: Mutex::new(HashMap::new()),
            id_to_key: Mutex::new(HashMap::new()),
            exit_tx,
        });
        (pool, exit_rx)
    }

    pub async fn remove_exited(&self, instance_id: &str) {
        let mut id_map = self.id_to_key.lock().await;
        if let Some(key) = id_map.remove(instance_id) {
            let mut inst_map = self.instances.lock().await;
            if inst_map.get(&key).is_some_and(|e| e.id == instance_id) {
                inst_map.remove(&key);
            }
        }
    }

    pub async fn attach_on_initialize(
        &self,
        client_id: u64,
        hello: HelloPayload,
        init_bytes: Vec<u8>,
        client_tx: mpsc::Sender<Vec<u8>>,
    ) -> Result<(String, mpsc::Sender<ActorCmd>)> {
        let init_val: Option<Value> = serde_json::from_slice(&init_bytes).ok();
        let init_params = init_val.as_ref().and_then(|v| v.get("params"));

        let server_name = paths::extract_server_name(&hello.command);
        let settings = self.config.effective_server(&server_name);
        let root = paths::normalize_workspace_root(
            hello.root_override.as_deref(),
            init_params,
            &hello.cwd,
        );

        let path_env = hello.env.get("PATH").map(|s| s.as_str());
        let shim_dir = paths::shim_bin_dir();
        let requested_resolved =
            paths::resolve_executable(&hello.command, path_env, Some(&shim_dir))
                .unwrap_or_else(|_| PathBuf::from(&hello.command));

        let binary_matching = hello.match_by.unwrap_or(settings.binary_matching);
        let server_key = compute_server_key(
            &server_name,
            &requested_resolved,
            binary_matching,
            hello.key_override.as_deref(),
        );
        let semantic_args_key = compute_args_key(&hello.args, &settings);
        let env_key = compute_env_key(&hello.env, &settings);

        let key = InstanceKey {
            server_key,
            root: root.clone(),
            semantic_args_key,
            env_key,
        };

        let client_name = init_params
            .and_then(|p| p.get("clientInfo"))
            .and_then(|ci| ci.get("name"))
            .and_then(|n| n.as_str())
            .map(ToString::to_string);

        let mut instances = self.instances.lock().await;
        if let Some(existing) = instances.get(&key) {
            if !existing.tx.is_closed() {
                let matched_by_name =
                    paths::normalize_uri_or_path(&requested_resolved.to_string_lossy())
                        != paths::normalize_uri_or_path(&existing.binary_path.to_string_lossy());
                if matched_by_name {
                    tracing::info!(
                        instance = %existing.id,
                        requested = %requested_resolved.display(),
                        active = %existing.binary_path.display(),
                        "client matched existing instance by name despite distinct binary path"
                    );
                }
                let join = ClientJoin {
                    client_id,
                    pid: hello.client_pid,
                    role: hello.role,
                    client_name: client_name.clone(),
                    requested_command: hello.command.clone(),
                    matched_by_name,
                    init_bytes: init_bytes.clone(),
                    tx: client_tx.clone(),
                };
                if existing.tx.send(ActorCmd::ClientJoin(join)).await.is_ok() {
                    return Ok((existing.id.clone(), existing.tx.clone()));
                }
            }
            // Existing actor died; remove stale entry before spawning fresh one.
            let stale_id = existing.id.clone();
            instances.remove(&key);
            self.id_to_key.lock().await.remove(&stale_id);
        }

        let launch_binary = match &settings.preferred_binary {
            Some(pref) => {
                let pref_str = pref.to_string_lossy();
                paths::resolve_executable(&pref_str, path_env, Some(&shim_dir))
                    .unwrap_or_else(|_| pref.clone())
            }
            None => requested_resolved.clone(),
        };

        let launch_args = compute_launch_args(&hello.args, &settings);
        let inst_id = format!(
            "{}#{:08x}",
            server_name,
            paths::fnv1a_64(
                format!(
                    "{}:{}:{}:{}",
                    key.server_key, key.root, key.semantic_args_key, key.env_key
                )
                .as_bytes()
            ) as u32
        );

        let matched_by_name = paths::normalize_uri_or_path(&requested_resolved.to_string_lossy())
            != paths::normalize_uri_or_path(&launch_binary.to_string_lossy());

        let actor_cfg = InstanceActorConfig {
            id: inst_id.clone(),
            server_name: server_name.clone(),
            root,
            binary_path: launch_binary.clone(),
            launch_args,
            env: hello.env,
            force_utf16: settings.force_utf16,
            idle_timeout_secs: settings.idle_timeout_secs,
            open_conflict_policy: self.config.documents.open_conflict_policy,
            event_dedup_window_ms: self.config.documents.event_dedup_window_ms,
            trace: self.trace.clone(),
            child_tracker: self.child_tracker.clone(),
            on_exit_notify: self.exit_tx.clone(),
        };

        let first_join = ClientJoin {
            client_id,
            pid: hello.client_pid,
            role: hello.role,
            client_name,
            requested_command: hello.command,
            matched_by_name,
            init_bytes,
            tx: client_tx,
        };

        let tx = spawn_instance_actor(actor_cfg, first_join);
        instances.insert(
            key.clone(),
            InstanceEntry {
                id: inst_id.clone(),
                binary_path: launch_binary,
                tx: tx.clone(),
            },
        );
        self.id_to_key.lock().await.insert(inst_id.clone(), key);
        Ok((inst_id, tx))
    }

    pub async fn status(&self) -> DaemonStatusPayload {
        let entries: Vec<mpsc::Sender<ActorCmd>> = {
            let guard = self.instances.lock().await;
            guard.values().map(|e| e.tx.clone()).collect()
        };

        let mut infos: Vec<InstanceStatusInfo> = Vec::with_capacity(entries.len());
        for tx in entries {
            let (reply_tx, reply_rx) = oneshot::channel();
            if tx
                .send(ActorCmd::GetStatus { reply: reply_tx })
                .await
                .is_ok()
            {
                if let Ok(info) = reply_rx.await {
                    infos.push(info);
                }
            }
        }
        infos.sort_by(|a, b| a.id.cmp(&b.id));

        DaemonStatusPayload {
            daemon_pid: std::process::id(),
            uptime_secs: self.started_at.elapsed().as_secs(),
            endpoint: paths::ipc_endpoint_display(),
            instances: infos,
        }
    }

    pub async fn stop_instance(&self, instance_id: &str) -> Result<String> {
        let tx = {
            let id_map = self.id_to_key.lock().await;
            let key = id_map
                .get(instance_id)
                .ok_or_else(|| anyhow!("instance '{instance_id}' not found"))?
                .clone();
            let inst_map = self.instances.lock().await;
            inst_map
                .get(&key)
                .map(|e| e.tx.clone())
                .ok_or_else(|| anyhow!("instance '{instance_id}' not found"))?
        };

        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = tx.send(ActorCmd::Shutdown { reply: reply_tx }).await;
        let _ = reply_rx.await;
        self.remove_exited(instance_id).await;
        Ok(format!("stopped instance {instance_id}"))
    }

    pub async fn stop_all(&self) -> usize {
        let all: Vec<(String, mpsc::Sender<ActorCmd>)> = {
            let inst_map = self.instances.lock().await;
            inst_map
                .values()
                .map(|e| (e.id.clone(), e.tx.clone()))
                .collect()
        };
        let count = all.len();
        for (id, tx) in all {
            let (reply_tx, reply_rx) = oneshot::channel();
            let _ = tx.send(ActorCmd::Shutdown { reply: reply_tx }).await;
            let _ = reply_rx.await;
            self.remove_exited(&id).await;
        }
        count
    }

    pub async fn restart_instance(&self, instance_id: &str) -> Result<String> {
        let tx = {
            let id_map = self.id_to_key.lock().await;
            let key = id_map
                .get(instance_id)
                .ok_or_else(|| anyhow!("instance '{instance_id}' not found"))?
                .clone();
            let inst_map = self.instances.lock().await;
            inst_map
                .get(&key)
                .map(|e| e.tx.clone())
                .ok_or_else(|| anyhow!("instance '{instance_id}' not found"))?
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        tx.send(ActorCmd::Restart { reply: reply_tx })
            .await
            .map_err(|_| anyhow!("instance actor closed"))?;
        reply_rx.await.map_err(|_| anyhow!("restart aborted"))??;
        Ok(format!("restarted instance {instance_id}"))
    }
}

pub fn compute_server_key(
    server_name: &str,
    resolved_binary: &std::path::Path,
    matching: BinaryMatching,
    key_override: Option<&str>,
) -> String {
    if let Some(k) = key_override.filter(|s| !s.trim().is_empty()) {
        return format!("key:{k}");
    }
    match matching {
        BinaryMatching::Name => format!("name:{server_name}"),
        BinaryMatching::CanonicalPath => {
            let norm = paths::normalize_uri_or_path(&resolved_binary.to_string_lossy());
            format!("path:{norm}")
        }
    }
}

fn compute_args_key(args: &[String], settings: &EffectiveServerSettings) -> String {
    match settings.arg_matching {
        ArgMatching::Ignore => String::new(),
        ArgMatching::Exact => args.join("\0"),
        ArgMatching::Semantic => {
            let mut extracted = Vec::new();
            let mut i = 0;
            while i < args.len() {
                let arg = &args[i];
                let mut matched = false;
                for flag in &settings.semantic_args {
                    if arg == flag {
                        extracted.push(arg.clone());
                        if let Some(next) = args.get(i + 1) {
                            extracted.push(next.clone());
                            i += 1;
                        }
                        matched = true;
                        break;
                    } else if arg.starts_with(&format!("{flag}=")) {
                        extracted.push(arg.clone());
                        matched = true;
                        break;
                    }
                }
                let _ = matched;
                i += 1;
            }
            extracted.sort();
            extracted.join("\0")
        }
    }
}

fn compute_env_key(
    env: &std::collections::BTreeMap<String, String>,
    settings: &EffectiveServerSettings,
) -> String {
    if settings.env_key_vars.is_empty() {
        return String::new();
    }
    let mut parts = Vec::with_capacity(settings.env_key_vars.len());
    for var in &settings.env_key_vars {
        let val = env.get(var).map(|s| s.as_str()).unwrap_or("");
        parts.push(format!("{var}={val}"));
    }
    parts.join("\0")
}

fn compute_launch_args(client_args: &[String], settings: &EffectiveServerSettings) -> Vec<String> {
    let mut out = client_args.to_vec();
    for def_arg in &settings.default_args {
        let flag_prefix = def_arg.split('=').next().unwrap_or(def_arg);
        let already_present = out
            .iter()
            .any(|a| a == def_arg || a.starts_with(&format!("{flag_prefix}=")));
        if !already_present {
            out.push(def_arg.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggressive_name_matching_collapses_distinct_paths() {
        let k1 = compute_server_key(
            "clangd",
            std::path::Path::new("/opt/llvm-18/bin/clangd"),
            BinaryMatching::Name,
            None,
        );
        let k2 = compute_server_key(
            "clangd",
            std::path::Path::new("/usr/bin/clangd"),
            BinaryMatching::Name,
            None,
        );
        assert_eq!(k1, k2);
        assert_eq!(k1, "name:clangd");

        let c1 = compute_server_key(
            "clangd",
            std::path::Path::new("/opt/llvm-18/bin/clangd"),
            BinaryMatching::CanonicalPath,
            None,
        );
        let c2 = compute_server_key(
            "clangd",
            std::path::Path::new("/usr/bin/clangd"),
            BinaryMatching::CanonicalPath,
            None,
        );
        assert_ne!(c1, c2);

        let o1 = compute_server_key(
            "clangd",
            std::path::Path::new("/usr/bin/clangd"),
            BinaryMatching::Name,
            Some("cpp-main"),
        );
        assert_eq!(o1, "key:cpp-main");
    }
}
