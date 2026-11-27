use lsp_share::protocol::frame::{read_lsp_frame, write_lsp_frame};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::BufReader;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

fn bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_lsp-share"))
}

struct TestEnv {
    _root_dir: TempDirGuard,
    runtime_dir: PathBuf,
    config_path: PathBuf,
    shim_dir: PathBuf,
    log_path: PathBuf,
    workspace_dir: PathBuf,
}

struct TempDirGuard {
    path: PathBuf,
}

impl TempDirGuard {
    fn new(name: &str) -> Self {
        let uniq = format!(
            "lsp-share-test-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        // Keep path short so Unix socket path stays well below macOS 104-byte limit.
        let path = PathBuf::from("/tmp").join(uniq);
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

impl TestEnv {
    fn new(name: &str) -> Self {
        let guard = TempDirGuard::new(name);
        let runtime_dir = guard.path.join("rt");
        let config_path = guard.path.join("config.toml");
        let shim_dir = guard.path.join("shims");
        let log_path = guard.path.join("daemon.log");
        let workspace_dir = guard.path.join("ws");
        std::fs::create_dir_all(&runtime_dir).unwrap();
        std::fs::create_dir_all(&shim_dir).unwrap();
        std::fs::create_dir_all(&workspace_dir).unwrap();
        Self {
            _root_dir: guard,
            runtime_dir,
            config_path,
            shim_dir,
            log_path,
            workspace_dir,
        }
    }

    fn apply_env(&self, cmd: &mut Command) {
        cmd.env("LSP_SHARE_RUNTIME_DIR", &self.runtime_dir)
            .env("LSP_SHARE_CONFIG", &self.config_path)
            .env("LSP_SHARE_SHIM_DIR", &self.shim_dir)
            .env("LSP_SHARE_LOG_FILE", &self.log_path);
    }

    fn create_mock_wrapper(&self, dir_name: &str, exe_name: &str, mock_log: &Path) -> PathBuf {
        let dir = self._root_dir.path.join(dir_name);
        std::fs::create_dir_all(&dir).unwrap();
        let wrapper = dir.join(exe_name);
        let lsp_share = bin_path();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let script = format!(
                "#!/bin/sh\nexec \"{}\" __mock-lsp --log \"{}\" --label \"{}\" \"$@\"\n",
                lsp_share.display(),
                mock_log.display(),
                dir_name
            );
            std::fs::write(&wrapper, script).unwrap();
            std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        wrapper
    }

    async fn run_cli(&self, args: &[&str]) -> std::process::Output {
        let mut cmd = Command::new(bin_path());
        cmd.args(args);
        self.apply_env(&mut cmd);
        cmd.output().await.unwrap()
    }

    async fn stop_daemon(&self) {
        let _ = self.run_cli(&["stop", "--daemon"]).await;
    }
}

struct LspTestClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl LspTestClient {
    async fn spawn_proxy(env: &TestEnv, role: &str, target: &Path, extra_args: &[&str]) -> Self {
        let mut cmd = Command::new(bin_path());
        cmd.arg("proxy")
            .arg("--role")
            .arg(role)
            .arg("--")
            .arg(target)
            .args(extra_args);
        env.apply_env(&mut cmd);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = cmd.spawn().unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            stdin,
            stdout,
        }
    }

    async fn spawn_direct(env: &TestEnv, exe: &Path) -> Self {
        let mut cmd = Command::new(exe);
        env.apply_env(&mut cmd);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = cmd.spawn().unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            stdin,
            stdout,
        }
    }

    async fn send(&mut self, msg: &Value) {
        let bytes = serde_json::to_vec(msg).unwrap();
        write_lsp_frame(&mut self.stdin, &bytes).await.unwrap();
    }

    async fn recv_timeout(&mut self, dur: Duration) -> Option<Value> {
        let mut buf = Vec::new();
        let res = tokio::time::timeout(dur, read_lsp_frame(&mut self.stdout, &mut buf)).await;
        match res {
            Ok(Ok(true)) => Some(serde_json::from_slice(&buf).unwrap()),
            _ => None,
        }
    }

    async fn recv(&mut self) -> Value {
        self.recv_timeout(Duration::from_secs(10))
            .await
            .expect("timed out waiting for LSP message")
    }

    /// Reads messages until a response with matching `id` arrives, auto-answering `workspace/configuration`
    /// if a handler closure is provided.
    async fn wait_response_with_cfg(
        &mut self,
        expected_id: i64,
        cfg_answer: Option<Value>,
    ) -> (Value, Vec<Value>) {
        let mut notifications = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let msg = self.recv().await;
            if msg.get("method").and_then(|m| m.as_str()) == Some("workspace/configuration") {
                if let Some(id) = msg.get("id") {
                    let ans = cfg_answer.clone().unwrap_or_else(|| json!([null]));
                    self.send(&json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": ans
                    }))
                    .await;
                }
                continue;
            }
            if msg.get("id").and_then(|v| v.as_i64()) == Some(expected_id)
                && msg.get("method").is_none()
            {
                return (msg, notifications);
            }
            notifications.push(msg);
        }
        panic!("timed out waiting for response id={expected_id}");
    }

    async fn wait_notification(&mut self, method: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            let msg = self.recv().await;
            if msg.get("method").and_then(|m| m.as_str()) == Some(method) {
                return msg;
            }
        }
        panic!("timed out waiting for notification {method}");
    }

    async fn shutdown_and_exit(mut self) {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": 9999,
            "method": "shutdown",
            "params": null
        }))
        .await;
        let (resp, _) = self.wait_response_with_cfg(9999, None).await;
        assert!(resp.get("result").is_some());
        self.send(&json!({
            "jsonrpc": "2.0",
            "method": "exit",
            "params": null
        }))
        .await;
        drop(self.stdin);
        let _ = tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await;
    }
}

fn file_uri(p: &Path) -> String {
    format!("file://{}", p.to_string_lossy())
}

#[tokio::test]
async fn e2e_aggressive_name_sharing_and_concurrent_requests() {
    let env = TestEnv::new("share");
    let mock_log = env._root_dir.path.join("mock_events.jsonl");
    let bin_a = env.create_mock_wrapper("bin_a", "mock-server", &mock_log);
    let bin_b = env.create_mock_wrapper("bin_b", "mock-server", &mock_log);

    let ws_uri = file_uri(&env.workspace_dir);

    let mut editor = LspTestClient::spawn_proxy(&env, "editor", &bin_a, &["--flag-a"]).await;
    editor
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "rootUri": ws_uri,
                "clientInfo": { "name": "Visual Studio Code" },
                "capabilities": {
                    "workspace": { "configuration": true }
                }
            }
        }))
        .await;
    let (init1, _) = editor.wait_response_with_cfg(1, None).await;
    editor
        .send(&json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }))
        .await;

    // Agent uses a completely different binary path (bin_b/mock-server) and different flags,
    // yet shares the exact same instance by normalized binary name!
    let mut agent = LspTestClient::spawn_proxy(&env, "agent", &bin_b, &["--flag-b"]).await;
    agent
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "workspaceFolders": [{ "uri": ws_uri, "name": "ws" }],
                "clientInfo": { "name": "custom-agent" },
                "capabilities": {}
            }
        }))
        .await;
    let (init2, _) = agent.wait_response_with_cfg(1, None).await;
    agent
        .send(&json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }))
        .await;

    let pid1 = init1["result"]["serverInfo"]["pid"].as_u64().unwrap();
    let pid2 = init2["result"]["serverInfo"]["pid"].as_u64().unwrap();
    assert_eq!(pid1, pid2, "editor and agent must share one LSP PID");

    // Colliding request IDs (id = 10 from both clients simultaneously)
    editor
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "mock/slowEcho",
            "params": { "who": "editor" }
        }))
        .await;
    agent
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "mock/slowEcho",
            "params": { "who": "agent" }
        }))
        .await;

    let (e_echo, _) = editor.wait_response_with_cfg(10, None).await;
    let (a_echo, _) = agent.wait_response_with_cfg(10, None).await;
    assert_eq!(e_echo["result"]["echo"]["who"], "editor");
    assert_eq!(a_echo["result"]["echo"]["who"], "agent");

    // workspace/configuration routed to primary (editor) and cached for agent
    agent
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 20,
            "method": "mock/getConfig",
            "params": {}
        }))
        .await;

    let cfg_req = editor.recv().await;
    assert_eq!(cfg_req["method"], "workspace/configuration");
    editor
        .send(&json!({
            "jsonrpc": "2.0",
            "id": cfg_req["id"],
            "result": ["/custom/venv/bin/python"]
        }))
        .await;

    let (cfg_resp, _) = agent.wait_response_with_cfg(20, None).await;
    assert_eq!(cfg_resp["result"]["config"][0], "/custom/venv/bin/python");

    // Shut down editor; agent must still receive cached config from daemon!
    editor.shutdown_and_exit().await;

    agent
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 21,
            "method": "mock/getConfig",
            "params": {}
        }))
        .await;
    let (cached_cfg_resp, _) = agent.wait_response_with_cfg(21, None).await;
    assert_eq!(
        cached_cfg_resp["result"]["config"][0],
        "/custom/venv/bin/python"
    );

    agent.shutdown_and_exit().await;

    // Verify mock-lsp only started once and received initialize once
    let log_str = std::fs::read_to_string(&mock_log).unwrap();
    let started_count = log_str
        .lines()
        .filter(|l| l.contains(r#""event":"started""#))
        .count();
    let init_count = log_str
        .lines()
        .filter(|l| l.contains(r#""method":"initialize""#))
        .count();
    assert_eq!(started_count, 1);
    assert_eq!(init_count, 1);

    env.stop_daemon().await;
}

#[tokio::test]
async fn e2e_concurrent_editing_lru10_and_diagnostics_replay() {
    let env = TestEnv::new("docsync");
    let mock_log = env._root_dir.path.join("mock_docs.jsonl");
    let bin = env.create_mock_wrapper("bin", "mock-server", &mock_log);
    let ws_uri = file_uri(&env.workspace_dir);
    let foo_uri = file_uri(&env.workspace_dir.join("foo.cpp"));

    let mut editor = LspTestClient::spawn_proxy(&env, "editor", &bin, &[]).await;
    editor
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "rootUri": ws_uri, "capabilities": {} }
        }))
        .await;
    let _ = editor.wait_response_with_cfg(1, None).await;
    editor
        .send(&json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }))
        .await;

    editor
        .send(&json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": { "uri": foo_uri, "languageId": "cpp", "version": 1, "text": "AAA" }
            }
        }))
        .await;
    let diag1 = editor
        .wait_notification("textDocument/publishDiagnostics")
        .await;
    assert_eq!(diag1["params"]["uri"], foo_uri);

    // Agent joins and opens foo.cpp -> immediately receives replayed diagnostics
    let mut agent = LspTestClient::spawn_proxy(&env, "agent", &bin, &[]).await;
    agent
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "rootUri": ws_uri, "capabilities": {} }
        }))
        .await;
    let _ = agent.wait_response_with_cfg(1, None).await;
    agent
        .send(&json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }))
        .await;

    agent
        .send(&json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": { "uri": foo_uri, "languageId": "cpp", "version": 1, "text": "AAA" }
            }
        }))
        .await;
    let diag_replay = agent
        .wait_notification("textDocument/publishDiagnostics")
        .await;
    assert_eq!(diag_replay["params"]["uri"], foo_uri);

    // Agent opens 10 new files and evicts foo.cpp via didClose
    for i in 0..10 {
        let u = file_uri(&env.workspace_dir.join(format!("lru_{i}.cpp")));
        agent
            .send(&json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didOpen",
                "params": {
                    "textDocument": { "uri": u, "languageId": "cpp", "version": 1, "text": "int x;" }
                }
            }))
            .await;
    }
    agent
        .send(&json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didClose",
            "params": { "textDocument": { "uri": foo_uri } }
        }))
        .await;

    // Agent re-opens foo.cpp and edits it -> "BBBBBB", then editor sends disk-reload diff 0..3 -> "BBBBBB"
    agent
        .send(&json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": { "uri": foo_uri, "languageId": "cpp", "version": 1, "text": "AAA" }
            }
        }))
        .await;
    let _ = agent
        .wait_notification("textDocument/publishDiagnostics")
        .await;

    agent
        .send(&json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didChange",
            "params": {
                "textDocument": { "uri": foo_uri, "version": 2 },
                "contentChanges": [{
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": 0, "character": 3 }
                    },
                    "text": "BBBBBB"
                }]
            }
        }))
        .await;

    editor
        .send(&json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didChange",
            "params": {
                "textDocument": { "uri": foo_uri, "version": 2 },
                "contentChanges": [{
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": 0, "character": 3 }
                    },
                    "text": "BBBBBB"
                }]
            }
        }))
        .await;

    editor
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 50,
            "method": "textDocument/hover",
            "params": {
                "textDocument": { "uri": foo_uri },
                "position": { "line": 0, "character": 0 }
            }
        }))
        .await;
    let (hover, _) = editor.wait_response_with_cfg(50, None).await;
    assert!(
        hover["result"]["contents"]
            .as_str()
            .unwrap()
            .contains("text=BBBBBB"),
        "got hover: {hover}"
    );
    assert!(
        !hover["result"]["contents"]
            .as_str()
            .unwrap()
            .contains("BBBBBBBBB"),
        "disk-reload diff should have been deduplicated"
    );

    // Agent exits; editor must remain functional
    agent.shutdown_and_exit().await;

    editor
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 51,
            "method": "textDocument/hover",
            "params": {
                "textDocument": { "uri": foo_uri },
                "position": { "line": 0, "character": 0 }
            }
        }))
        .await;
    let (hover2, _) = editor.wait_response_with_cfg(51, None).await;
    assert!(hover2["result"]["contents"]
        .as_str()
        .unwrap()
        .contains("text=BBBBBB"));

    editor.shutdown_and_exit().await;

    // Verify mock-lsp never received didClose for foo.cpp while editor had it open
    let log_str = std::fs::read_to_string(&mock_log).unwrap();
    let foo_closes = log_str
        .lines()
        .filter(|l| l.contains(r#""method":"textDocument/didClose""#) && l.contains("foo.cpp"))
        .count();
    assert_eq!(
        foo_closes, 1,
        "only final editor didClose should reach server"
    );

    env.stop_daemon().await;
}

#[tokio::test]
async fn e2e_argv0_shim_status_restart_and_stop() {
    let env = TestEnv::new("shim");
    let mock_log = env._root_dir.path.join("mock_shim.jsonl");
    let real_bin = env.create_mock_wrapper("real", "mymock", &mock_log);
    let ws_uri = file_uri(&env.workspace_dir);
    let keep_uri = file_uri(&env.workspace_dir.join("keep.cpp"));

    let out = env
        .run_cli(&[
            "shim",
            "add",
            "mymock",
            "--target",
            real_bin.to_str().unwrap(),
        ])
        .await;
    assert!(
        out.status.success(),
        "shim add failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let shim_exe = env.shim_dir.join("mymock");
    let mut client = LspTestClient::spawn_direct(&env, &shim_exe).await;
    client
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "rootUri": ws_uri, "capabilities": {} }
        }))
        .await;
    let (init, _) = client.wait_response_with_cfg(1, None).await;
    let old_pid = init["result"]["serverInfo"]["pid"].as_u64().unwrap();
    client
        .send(&json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }))
        .await;

    client
        .send(&json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": keep_uri,
                    "languageId": "cpp",
                    "version": 1,
                    "text": "hello_restart"
                }
            }
        }))
        .await;
    let _ = client
        .wait_notification("textDocument/publishDiagnostics")
        .await;

    // Query status --json
    let st_out = env.run_cli(&["status", "--json"]).await;
    assert!(st_out.status.success());
    let st_json: Value = serde_json::from_slice(&st_out.stdout).unwrap();
    assert_eq!(st_json["instances"].as_array().unwrap().len(), 1);
    let inst = &st_json["instances"][0];
    let inst_id = inst["id"].as_str().unwrap().to_string();
    assert_eq!(inst["open_docs"], 1);
    assert_eq!(inst["clients"].as_array().unwrap().len(), 1);

    // Restart instance in-place
    let restart_out = env.run_cli(&["restart", &inst_id]).await;
    assert!(
        restart_out.status.success(),
        "restart failed: {}",
        String::from_utf8_lossy(&restart_out.stderr)
    );

    // Verify hover on same client returns NEW pid AND preserved document text!
    client
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 77,
            "method": "textDocument/hover",
            "params": {
                "textDocument": { "uri": keep_uri },
                "position": { "line": 0, "character": 0 }
            }
        }))
        .await;
    let (hover, _) = client.wait_response_with_cfg(77, None).await;
    let contents = hover["result"]["contents"].as_str().unwrap();
    assert!(contents.contains("text=hello_restart"), "got: {contents}");
    assert!(
        !contents.contains(&format!("pid={old_pid};")),
        "expected new LSP pid after restart, got: {contents}"
    );

    client.shutdown_and_exit().await;

    let stop_out = env.run_cli(&["stop", "--daemon"]).await;
    assert!(stop_out.status.success());
    tokio::time::sleep(Duration::from_millis(100)).await;

    let st_after = env.run_cli(&["status", "--json"]).await;
    let st_after_json: Value = serde_json::from_slice(&st_after.stdout).unwrap();
    assert_eq!(st_after_json["running"], false);
}

#[tokio::test]
async fn e2e_real_clangd_sharing_and_utf16() {
    let clangd_path = Path::new("/usr/bin/clangd");
    if !clangd_path.exists() {
        return;
    }

    let env = TestEnv::new("clangd");
    let cpp_file = env.workspace_dir.join("main.cpp");
    let cpp_src =
        "int add_nums(int a, int b) { return a + b; }\nint bad_fn() { return \"oops\"; }\n";
    std::fs::write(&cpp_file, cpp_src).unwrap();

    let ws_uri = file_uri(&env.workspace_dir);
    let doc_uri = file_uri(&cpp_file);

    // Client 1 tries to request utf-8, which lsp-share normalizes to utf-16 for clangd.
    let mut c1 = LspTestClient::spawn_proxy(&env, "editor", clangd_path, &[]).await;
    c1.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "rootUri": ws_uri,
            "clientInfo": { "name": "Visual Studio Code" },
            "capabilities": {
                "general": { "positionEncodings": ["utf-8"] },
                "offsetEncoding": ["utf-8"]
            }
        }
    }))
    .await;
    let (init1, _) = c1.wait_response_with_cfg(1, None).await;
    if let Some(enc) = init1["result"]["capabilities"]["positionEncoding"].as_str() {
        assert_eq!(enc, "utf-16");
    }
    c1.send(&json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }))
        .await;

    // Client 2 (agent) connects to the same clangd instance
    let mut c2 = LspTestClient::spawn_proxy(&env, "agent", clangd_path, &[]).await;
    c2.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "rootUri": ws_uri,
            "clientInfo": { "name": "agent-client" },
            "capabilities": {
                "general": { "positionEncodings": ["utf-16"] }
            }
        }
    }))
    .await;
    let (init2, _) = c2.wait_response_with_cfg(1, None).await;
    assert_eq!(init1["result"], init2["result"]);
    c2.send(&json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }))
        .await;

    // Verify status shows 1 clangd instance and 2 clients
    let st_out = env.run_cli(&["status", "--json"]).await;
    let st_json: Value = serde_json::from_slice(&st_out.stdout).unwrap();
    assert_eq!(st_json["instances"].as_array().unwrap().len(), 1);
    assert_eq!(
        st_json["instances"][0]["clients"].as_array().unwrap().len(),
        2
    );

    // Client 1 opens main.cpp -> real clangd emits diagnostics for `bad_fn`
    c1.send(&json!({
        "jsonrpc": "2.0",
        "method": "textDocument/didOpen",
        "params": {
            "textDocument": {
                "uri": doc_uri,
                "languageId": "cpp",
                "version": 1,
                "text": cpp_src
            }
        }
    }))
    .await;

    // Wait for non-empty diagnostics from clangd
    let mut got_diag = false;
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if let Some(msg) = c1.recv_timeout(Duration::from_secs(5)).await {
            if msg.get("method").and_then(|m| m.as_str()) == Some("textDocument/publishDiagnostics")
            {
                if let Some(arr) = msg["params"]["diagnostics"].as_array() {
                    if !arr.is_empty() {
                        got_diag = true;
                        break;
                    }
                }
            }
        }
    }
    assert!(got_diag, "expected real clangd diagnostics for type error");

    // Client 2 opens main.cpp -> gets replayed diagnostics + can query real clangd hover
    c2.send(&json!({
        "jsonrpc": "2.0",
        "method": "textDocument/didOpen",
        "params": {
            "textDocument": {
                "uri": doc_uri,
                "languageId": "cpp",
                "version": 1,
                "text": cpp_src
            }
        }
    }))
    .await;
    let replay_diag = c2
        .wait_notification("textDocument/publishDiagnostics")
        .await;
    assert!(!replay_diag["params"]["diagnostics"]
        .as_array()
        .unwrap()
        .is_empty());

    c2.send(&json!({
        "jsonrpc": "2.0",
        "id": 9,
        "method": "textDocument/hover",
        "params": {
            "textDocument": { "uri": doc_uri },
            "position": { "line": 0, "character": 5 }
        }
    }))
    .await;
    let (hover, _) = c2.wait_response_with_cfg(9, None).await;
    assert!(
        hover["result"].is_object(),
        "expected hover object from clangd, got: {hover}"
    );

    c2.shutdown_and_exit().await;
    c1.shutdown_and_exit().await;
    env.stop_daemon().await;
}
