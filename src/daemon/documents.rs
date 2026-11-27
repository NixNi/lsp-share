use crate::config::{ClientRole, OpenConflictPolicy};
use crate::paths;
use crate::protocol::jsonrpc;
use ropey::Rope;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct LspPosition {
    pub line: u32,
    pub character: u32,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct LspRange {
    pub start: LspPosition,
    pub end: LspPosition,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ContentChangeEvent {
    pub range: Option<LspRange>,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct ClientDocState {
    pub role: ClientRole,
    pub client_version: i64,
    pub rope: Rope,
    pub dirty: bool,
}

#[derive(Debug, Clone)]
pub struct DocState {
    pub norm_uri: String,
    pub server_uri: String,
    pub language_id: String,
    pub server_version: i64,
    pub server_rope: Rope,
    pub last_writer: Option<u64>,
    pub clients: HashMap<u64, ClientDocState>,
}

#[derive(Debug, Clone)]
pub struct CachedDiagnostics {
    pub raw_params: Value,
}

#[derive(Debug)]
pub struct DidOpenOutcome {
    pub to_server: Option<Vec<u8>>,
    pub replay_diagnostics: Option<Vec<u8>>,
    pub action_tag: &'static str,
}

#[derive(Debug)]
pub struct DidChangeOutcome {
    pub to_server: Option<Vec<u8>>,
    pub action_tag: &'static str,
}

pub struct DocumentManager {
    policy: OpenConflictPolicy,
    dedup_window: Duration,
    docs: HashMap<String, DocState>,
    diagnostics_cache: HashMap<String, CachedDiagnostics>,
    recent_saves: HashMap<String, Instant>,
    recent_watched: HashMap<(String, u64), Instant>,
}

impl DocumentManager {
    pub fn new(policy: OpenConflictPolicy, dedup_window_ms: u64) -> Self {
        Self {
            policy,
            dedup_window: Duration::from_millis(dedup_window_ms),
            docs: HashMap::new(),
            diagnostics_cache: HashMap::new(),
            recent_saves: HashMap::new(),
            recent_watched: HashMap::new(),
        }
    }

    pub fn open_doc_count(&self) -> usize {
        self.docs.len()
    }

    pub fn handle_did_open(
        &mut self,
        client_id: u64,
        role: ClientRole,
        params: &Value,
    ) -> Option<DidOpenOutcome> {
        let td = params.get("textDocument")?;
        let raw_uri = td.get("uri")?.as_str()?;
        let text = td.get("text")?.as_str()?;
        let language_id = td
            .get("languageId")
            .and_then(|v| v.as_str())
            .unwrap_or("plaintext")
            .to_string();
        let client_version = td.get("version").and_then(|v| v.as_i64()).unwrap_or(1);

        let norm_uri = paths::normalize_uri_or_path(raw_uri);
        let incoming_rope = Rope::from_str(text);

        if let Some(doc) = self.docs.get_mut(&norm_uri) {
            doc.clients.insert(
                client_id,
                ClientDocState {
                    role,
                    client_version,
                    rope: incoming_rope.clone(),
                    dirty: false,
                },
            );

            let to_server = if !ropes_equal(&incoming_rope, &doc.server_rope) {
                let editor_has_dirty = doc.clients.iter().any(|(&cid, st)| {
                    cid != client_id && st.role == ClientRole::Editor && st.dirty
                });

                let should_update_server = match self.policy {
                    OpenConflictPolicy::EditorWins => {
                        !(role == ClientRole::Agent && editor_has_dirty)
                    }
                    OpenConflictPolicy::LastWriterWins => true,
                };

                if should_update_server {
                    doc.server_rope = incoming_rope;
                    doc.server_version += 1;
                    doc.last_writer = Some(client_id);
                    Some(build_full_did_change(
                        &doc.server_uri,
                        doc.server_version,
                        text,
                    ))
                } else {
                    None
                }
            } else {
                None
            };

            let replay_diagnostics = self.diagnostics_cache.get(&norm_uri).map(|cached| {
                let mut replay_params = cached.raw_params.clone();
                if let Some(obj) = replay_params.as_object_mut() {
                    obj.insert("uri".to_string(), Value::String(raw_uri.to_string()));
                }
                jsonrpc::build_notification("textDocument/publishDiagnostics", &replay_params)
            });

            let action_tag = if to_server.is_some() {
                "suppressed_open_synced"
            } else {
                "suppressed"
            };

            Some(DidOpenOutcome {
                to_server,
                replay_diagnostics,
                action_tag,
            })
        } else {
            let mut clients = HashMap::new();
            clients.insert(
                client_id,
                ClientDocState {
                    role,
                    client_version,
                    rope: incoming_rope.clone(),
                    dirty: false,
                },
            );
            let doc = DocState {
                norm_uri: norm_uri.clone(),
                server_uri: raw_uri.to_string(),
                language_id: language_id.clone(),
                server_version: 1,
                server_rope: incoming_rope,
                last_writer: Some(client_id),
                clients,
            };
            self.docs.insert(norm_uri, doc);

            let mut out_params = params.clone();
            if let Some(td_obj) = out_params
                .get_mut("textDocument")
                .and_then(|v| v.as_object_mut())
            {
                td_obj.insert("version".to_string(), json!(1));
            }
            let to_server = Some(jsonrpc::build_notification(
                "textDocument/didOpen",
                &out_params,
            ));

            Some(DidOpenOutcome {
                to_server,
                replay_diagnostics: None,
                action_tag: "forwarded",
            })
        }
    }

    pub fn handle_did_change(
        &mut self,
        client_id: u64,
        role: ClientRole,
        params: &mut Value,
    ) -> Option<DidChangeOutcome> {
        let td = params.get("textDocument")?;
        let raw_uri = td.get("uri")?.as_str()?.to_string();
        let client_version = td.get("version").and_then(|v| v.as_i64()).unwrap_or(1);
        let changes_val = params.get("contentChanges")?.clone();
        let changes: Vec<ContentChangeEvent> = serde_json::from_value(changes_val).ok()?;
        let norm_uri = paths::normalize_uri_or_path(&raw_uri);

        // Defensive synthesis if a non-compliant client sends didChange before didOpen.
        if let std::collections::hash_map::Entry::Vacant(e) = self.docs.entry(norm_uri.clone()) {
            if let Some(first_full) = changes.first().filter(|c| c.range.is_none()) {
                let mut open_doc = DocState {
                    norm_uri: norm_uri.clone(),
                    server_uri: raw_uri.clone(),
                    language_id: "plaintext".to_string(),
                    server_version: 1,
                    server_rope: Rope::from_str(&first_full.text),
                    last_writer: Some(client_id),
                    clients: HashMap::new(),
                };
                for c in changes.iter().skip(1) {
                    apply_change_to_rope(&mut open_doc.server_rope, c);
                }
                open_doc.clients.insert(
                    client_id,
                    ClientDocState {
                        role,
                        client_version,
                        rope: open_doc.server_rope.clone(),
                        dirty: true,
                    },
                );
                let text = open_doc.server_rope.to_string();
                e.insert(open_doc);
                let synth_open = jsonrpc::build_notification(
                    "textDocument/didOpen",
                    &json!({
                        "textDocument": {
                            "uri": raw_uri,
                            "languageId": "plaintext",
                            "version": 1,
                            "text": text
                        }
                    }),
                );
                return Some(DidChangeOutcome {
                    to_server: Some(synth_open),
                    action_tag: "synthesized_open",
                });
            }
            return None;
        }

        let doc = self.docs.get_mut(&norm_uri)?;
        if !doc.clients.contains_key(&client_id) {
            doc.clients.insert(
                client_id,
                ClientDocState {
                    role,
                    client_version,
                    rope: doc.server_rope.clone(),
                    dirty: false,
                },
            );
        }

        let client_doc = doc.clients.get_mut(&client_id)?;
        client_doc.client_version = client_version;
        client_doc.dirty = true;
        for change in &changes {
            apply_change_to_rope(&mut client_doc.rope, change);
        }

        if ropes_equal(&client_doc.rope, &doc.server_rope) {
            return Some(DidChangeOutcome {
                to_server: None,
                action_tag: "deduplicated",
            });
        }

        let new_rope = client_doc.rope.clone();
        doc.server_rope = new_rope;
        doc.server_version += 1;

        if doc.last_writer == Some(client_id) {
            if let Some(td_obj) = params
                .get_mut("textDocument")
                .and_then(|v| v.as_object_mut())
            {
                td_obj.insert("uri".to_string(), Value::String(doc.server_uri.clone()));
                td_obj.insert("version".to_string(), json!(doc.server_version));
            }
            let msg = jsonrpc::build_notification("textDocument/didChange", params);
            Some(DidChangeOutcome {
                to_server: Some(msg),
                action_tag: "forwarded",
            })
        } else {
            doc.last_writer = Some(client_id);
            let full_text = doc.server_rope.to_string();
            let msg = build_full_did_change(&doc.server_uri, doc.server_version, &full_text);
            Some(DidChangeOutcome {
                to_server: Some(msg),
                action_tag: "handoff_full_sync",
            })
        }
    }

    pub fn handle_did_save(&mut self, client_id: u64, params: &mut Value) -> Vec<Vec<u8>> {
        let Some(raw_uri) = params
            .get("textDocument")
            .and_then(|td| td.get("uri"))
            .and_then(|v| v.as_str())
            .map(ToString::to_string)
        else {
            return Vec::new();
        };
        let norm_uri = paths::normalize_uri_or_path(&raw_uri);
        let mut out = Vec::new();

        if let Some(doc) = self.docs.get_mut(&norm_uri) {
            if let Some(c) = doc.clients.get_mut(&client_id) {
                c.dirty = false;
            }
            if let Some(saved_text) = params.get("text").and_then(|v| v.as_str()) {
                let saved_rope = Rope::from_str(saved_text);
                if let Some(c) = doc.clients.get_mut(&client_id) {
                    c.rope = saved_rope.clone();
                }
                if !ropes_equal(&saved_rope, &doc.server_rope) {
                    doc.server_rope = saved_rope;
                    doc.server_version += 1;
                    doc.last_writer = Some(client_id);
                    out.push(build_full_did_change(
                        &doc.server_uri,
                        doc.server_version,
                        saved_text,
                    ));
                }
            }
            if let Some(td_obj) = params
                .get_mut("textDocument")
                .and_then(|v| v.as_object_mut())
            {
                td_obj.insert("uri".to_string(), Value::String(doc.server_uri.clone()));
            }
        }

        let now = Instant::now();
        let is_dup = self
            .recent_saves
            .get(&norm_uri)
            .is_some_and(|prev| now.duration_since(*prev) < self.dedup_window);
        if !is_dup {
            self.recent_saves.insert(norm_uri, now);
            out.push(jsonrpc::build_notification("textDocument/didSave", params));
        }
        out
    }

    pub fn handle_did_close(&mut self, client_id: u64, raw_uri: &str) -> Option<Vec<u8>> {
        let norm_uri = paths::normalize_uri_or_path(raw_uri);
        self.close_norm_uri(client_id, &norm_uri)
    }

    pub fn handle_client_disconnect(&mut self, client_id: u64) -> Vec<Vec<u8>> {
        let uris: Vec<String> = self
            .docs
            .iter()
            .filter_map(|(uri, doc)| {
                if doc.clients.contains_key(&client_id) {
                    Some(uri.clone())
                } else {
                    None
                }
            })
            .collect();
        let mut notifications = Vec::new();
        for uri in uris {
            if let Some(msg) = self.close_norm_uri(client_id, &uri) {
                notifications.push(msg);
            }
        }
        notifications
    }

    fn close_norm_uri(&mut self, client_id: u64, norm_uri: &str) -> Option<Vec<u8>> {
        let doc = self.docs.get_mut(norm_uri)?;
        doc.clients.remove(&client_id);
        if doc.last_writer == Some(client_id) {
            doc.last_writer = None;
        }
        if doc.clients.is_empty() {
            let server_uri = doc.server_uri.clone();
            self.docs.remove(norm_uri);
            Some(jsonrpc::build_notification(
                "textDocument/didClose",
                &json!({ "textDocument": { "uri": server_uri } }),
            ))
        } else {
            None
        }
    }

    pub fn cache_publish_diagnostics(&mut self, params: Value) {
        if let Some(uri) = params.get("uri").and_then(|v| v.as_str()) {
            let norm = paths::normalize_uri_or_path(uri);
            self.diagnostics_cache
                .insert(norm, CachedDiagnostics { raw_params: params });
        }
    }

    pub fn dedup_watched_files(&mut self, params: &Value) -> Option<Vec<u8>> {
        let changes = params.get("changes").and_then(|c| c.as_array())?;
        let now = Instant::now();
        // Prune stale entries lazily if map grows.
        if self.recent_watched.len() > 4096 {
            self.recent_watched
                .retain(|_, ts| now.duration_since(*ts) < self.dedup_window * 4);
        }

        let mut filtered = Vec::with_capacity(changes.len());
        for item in changes {
            let Some(uri) = item.get("uri").and_then(|v| v.as_str()) else {
                continue;
            };
            let kind = item.get("type").and_then(|v| v.as_u64()).unwrap_or(0);
            let key = (paths::normalize_uri_or_path(uri), kind);
            let is_dup = self
                .recent_watched
                .get(&key)
                .is_some_and(|prev| now.duration_since(*prev) < self.dedup_window);
            if !is_dup {
                self.recent_watched.insert(key, now);
                filtered.push(item.clone());
            }
        }
        if filtered.is_empty() {
            None
        } else {
            Some(jsonrpc::build_notification(
                "workspace/didChangeWatchedFiles",
                &json!({ "changes": filtered }),
            ))
        }
    }

    /// Generates `textDocument/didOpen` notifications (version = 1) for all open docs
    /// so an in-place LSP restart preserves open buffers seamlessly.
    pub fn snapshot_open_docs_for_restart(&mut self) -> Vec<Vec<u8>> {
        let mut out = Vec::with_capacity(self.docs.len());
        for doc in self.docs.values_mut() {
            doc.server_version = 1;
            doc.last_writer = None;
            out.push(jsonrpc::build_notification(
                "textDocument/didOpen",
                &json!({
                    "textDocument": {
                        "uri": doc.server_uri,
                        "languageId": doc.language_id,
                        "version": 1,
                        "text": doc.server_rope.to_string(),
                    }
                }),
            ));
        }
        out
    }

    #[cfg(test)]
    pub fn server_text(&self, uri: &str) -> Option<String> {
        let norm = paths::normalize_uri_or_path(uri);
        self.docs.get(&norm).map(|d| d.server_rope.to_string())
    }
}

fn ropes_equal(a: &Rope, b: &Rope) -> bool {
    a.len_bytes() == b.len_bytes() && a == b
}

fn build_full_did_change(uri: &str, version: i64, text: &str) -> Vec<u8> {
    jsonrpc::build_notification(
        "textDocument/didChange",
        &json!({
            "textDocument": {
                "uri": uri,
                "version": version
            },
            "contentChanges": [{ "text": text }]
        }),
    )
}

pub fn lsp_pos_to_char_idx(rope: &Rope, pos: LspPosition) -> usize {
    let line_idx = pos.line as usize;
    if line_idx >= rope.len_lines() {
        return rope.len_chars();
    }
    let line_slice = rope.line(line_idx);
    let line_chars = line_slice.len_chars();
    let newline_chars = if line_chars >= 2
        && line_slice.char(line_chars - 2) == '\r'
        && line_slice.char(line_chars - 1) == '\n'
    {
        2
    } else if line_chars >= 1
        && (line_slice.char(line_chars - 1) == '\n' || line_slice.char(line_chars - 1) == '\r')
    {
        1
    } else {
        0
    };
    let content_slice = line_slice.slice(..line_chars - newline_chars);
    let clamped_u16 = (pos.character as usize).min(content_slice.len_utf16_cu());
    rope.line_to_char(line_idx) + content_slice.utf16_cu_to_char(clamped_u16)
}

pub fn apply_change_to_rope(rope: &mut Rope, change: &ContentChangeEvent) {
    match change.range {
        None => {
            *rope = Rope::from_str(&change.text);
        }
        Some(range) => {
            let start = lsp_pos_to_char_idx(rope, range.start);
            let end = lsp_pos_to_char_idx(rope, range.end);
            let (lo, hi) = if start <= end {
                (start, end)
            } else {
                (end, start)
            };
            rope.remove(lo..hi);
            rope.insert(lo, &change.text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_msg(uri: &str, version: i64, text: &str) -> Value {
        json!({
            "textDocument": {
                "uri": uri,
                "languageId": "cpp",
                "version": version,
                "text": text
            }
        })
    }

    fn inc_change(
        uri: &str,
        version: i64,
        sl: u32,
        sc: u32,
        el: u32,
        ec: u32,
        text: &str,
    ) -> Value {
        json!({
            "textDocument": { "uri": uri, "version": version },
            "contentChanges": [{
                "range": {
                    "start": { "line": sl, "character": sc },
                    "end": { "line": el, "character": ec }
                },
                "text": text
            }]
        })
    }

    #[test]
    fn refcounting_lru_eviction_and_diagnostics_replay() {
        let mut mgr = DocumentManager::new(OpenConflictPolicy::EditorWins, 50);
        let uri = "file:///repo/foo.cpp";

        let o1 = mgr
            .handle_did_open(1, ClientRole::Editor, &open_msg(uri, 1, "int x = 1;"))
            .unwrap();
        assert!(o1.to_server.is_some());
        assert_eq!(mgr.open_doc_count(), 1);

        mgr.cache_publish_diagnostics(json!({
            "uri": uri,
            "diagnostics": [{ "message": "unused variable" }]
        }));

        // Agent opens the same file -> didOpen suppressed, cached diagnostics replayed!
        let o2 = mgr
            .handle_did_open(2, ClientRole::Agent, &open_msg(uri, 1, "int x = 1;"))
            .unwrap();
        assert!(o2.to_server.is_none());
        let replay: Value = serde_json::from_slice(&o2.replay_diagnostics.unwrap()).unwrap();
        assert_eq!(replay["method"], "textDocument/publishDiagnostics");
        assert_eq!(
            replay["params"]["diagnostics"][0]["message"],
            "unused variable"
        );

        // Agent evicts file via LRU-10 didClose -> server must NOT receive didClose while editor holds it.
        assert!(mgr.handle_did_close(2, uri).is_none());
        assert_eq!(mgr.open_doc_count(), 1);

        // Editor closes -> refcount reaches 0 -> real didClose emitted.
        let close_msg = mgr.handle_did_close(1, uri);
        assert!(close_msg.is_some());
        assert_eq!(mgr.open_doc_count(), 0);
    }

    #[test]
    fn disk_reload_incremental_corruption_is_deduplicated() {
        let mut mgr = DocumentManager::new(OpenConflictPolicy::EditorWins, 50);
        let uri = "file:///repo/race.cpp";

        // Editor opens "AAA"
        mgr.handle_did_open(1, ClientRole::Editor, &open_msg(uri, 1, "AAA"))
            .unwrap();
        // Agent opens "AAA"
        mgr.handle_did_open(2, ClientRole::Agent, &open_msg(uri, 1, "AAA"))
            .unwrap();

        // Agent writes "BBBBBB" and sends didChange -> writer handoff emits full sync
        let mut agent_edit = inc_change(uri, 2, 0, 0, 0, 3, "BBBBBB");
        let out_agent = mgr
            .handle_did_change(2, ClientRole::Agent, &mut agent_edit)
            .unwrap();
        assert_eq!(out_agent.action_tag, "handoff_full_sync");
        assert_eq!(mgr.server_text(uri).as_deref(), Some("BBBBBB"));

        // 100ms later VS Code reloads disk and sends incremental diff (0:0..0:3 -> "BBBBBB")
        // against its old "AAA" buffer. Must be deduplicated so server does NOT become "BBBBBBBBB"!
        let mut editor_reload = inc_change(uri, 2, 0, 0, 0, 3, "BBBBBB");
        let out_editor = mgr
            .handle_did_change(1, ClientRole::Editor, &mut editor_reload)
            .unwrap();
        assert_eq!(out_editor.action_tag, "deduplicated");
        assert!(out_editor.to_server.is_none());
        assert_eq!(mgr.server_text(uri).as_deref(), Some("BBBBBB"));
    }

    #[test]
    fn writer_handoff_and_utf16_cyrillic_emoji_crlf() {
        let mut mgr = DocumentManager::new(OpenConflictPolicy::EditorWins, 50);
        let uri = "file:///repo/utf16.py";

        // "😀" is 2 UTF-16 code units; "Привет" is 6 UTF-16 code units; CRLF line ending.
        let initial = "x = '😀'\r\ny = 'Привет'\r\n";
        mgr.handle_did_open(1, ClientRole::Editor, &open_msg(uri, 1, initial))
            .unwrap();
        mgr.handle_did_open(2, ClientRole::Agent, &open_msg(uri, 1, initial))
            .unwrap();

        // Editor edits line 0 after emoji (col 5 + 2 + 1 = 8 -> end of line 0 before \r\n)
        let mut c1 = inc_change(uri, 2, 0, 7, 0, 7, "!");
        let o1 = mgr
            .handle_did_change(1, ClientRole::Editor, &mut c1)
            .unwrap();
        assert_eq!(o1.action_tag, "forwarded");
        assert_eq!(
            mgr.server_text(uri).as_deref(),
            Some("x = '😀!'\r\ny = 'Привет'\r\n")
        );

        // Agent edits line 1 ("Привет" -> "Мир"): col 5..11 in agent's own rope
        let mut c2 = inc_change(uri, 2, 1, 5, 1, 11, "Мир");
        let o2 = mgr
            .handle_did_change(2, ClientRole::Agent, &mut c2)
            .unwrap();
        assert_eq!(o2.action_tag, "handoff_full_sync");
        assert_eq!(
            mgr.server_text(uri).as_deref(),
            Some("x = '😀'\r\ny = 'Мир'\r\n")
        );

        // Editor makes a subsequent edit -> handoff back to editor via full sync, then fast path resumes
        let mut c3 = inc_change(uri, 3, 1, 5, 1, 11, "Test");
        let o3 = mgr
            .handle_did_change(1, ClientRole::Editor, &mut c3)
            .unwrap();
        assert_eq!(o3.action_tag, "handoff_full_sync");
        assert_eq!(
            mgr.server_text(uri).as_deref(),
            Some("x = '😀!'\r\ny = 'Test'\r\n")
        );

        let mut c4 = inc_change(uri, 4, 1, 9, 1, 9, "2");
        let o4 = mgr
            .handle_did_change(1, ClientRole::Editor, &mut c4)
            .unwrap();
        assert_eq!(o4.action_tag, "forwarded");
        assert_eq!(
            mgr.server_text(uri).as_deref(),
            Some("x = '😀!'\r\ny = 'Test2'\r\n")
        );
    }

    #[test]
    fn editor_wins_protects_dirty_editor_buffer_from_agent_open() {
        let mut mgr = DocumentManager::new(OpenConflictPolicy::EditorWins, 50);
        let uri = "file:///repo/dirty.cpp";
        mgr.handle_did_open(1, ClientRole::Editor, &open_msg(uri, 1, "old"))
            .unwrap();
        let mut edit = inc_change(uri, 2, 0, 0, 0, 3, "unsaved_editor");
        mgr.handle_did_change(1, ClientRole::Editor, &mut edit)
            .unwrap();

        // Agent opens stale disk content -> EditorWins keeps unsaved_editor in server_rope
        let o2 = mgr
            .handle_did_open(2, ClientRole::Agent, &open_msg(uri, 1, "old"))
            .unwrap();
        assert!(o2.to_server.is_none());
        assert_eq!(mgr.server_text(uri).as_deref(), Some("unsaved_editor"));
    }
}
