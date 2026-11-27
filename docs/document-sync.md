# Document Synchronization Engine

Document synchronization (`didOpen`, `didChange`, `didSave`, `didClose`) is the hardest problem in multi-client LSP multiplexing—especially when **both** an editor (VS Code) and coding agents open, edit, and close files concurrently.

## Real-World Failure Modes Solved

1. **LRU `didClose` eviction by agents:** An agent client opens files to inspect or edit them, keeping an LRU cache of ~10 files and sending `didClose` for evicted files. Forwarding that `didClose` directly would close files still open in VS Code and erase editor diagnostics.
2. **Duplicate `didOpen`:** Sending a second `didOpen` for an already-open URI causes servers like `clangd` and `pyright` to error or reset internal document state.
3. **Version regression:** LSP requires `textDocument.version` to increase monotonically. Forwarding VS Code's `v14` followed by an agent's `v1` breaks incremental updates.
4. **Disk-reload incremental corruption (critical race):**
   - Suppose `foo.cpp` on disk is `"AAA"`, open in VS Code (`v1`).
   - The agent opens `foo.cpp`, writes `"BBBBBB"` to disk, and sends `didChange("BBBBBB")` to the LSP. The real LSP now holds `"BBBBBB"`.
   - 100 ms later, VS Code detects the disk change and computes an **incremental diff** against its old buffer `"AAA"`: `replace(0:0..0:3, "BBBBBB")`.
   - Naively forwarding that incremental range onto the LSP's `"BBBBBB"` buffer produces `"BBBBBBBBB"`—silently corrupting the server's in-memory AST!
5. **Missing diagnostics on re-open:** When an agent re-opens a file it previously evicted (or opens a file VS Code opened 10 minutes ago), the LSP does not re-emit `textDocument/publishDiagnostics` because the file did not change.

---

## Core Data Model: Per-Client CoW `Rope` + `server_rope`

For each currently open normalized URI, the `InstanceActor` maintains a `DocState`:

```rust
struct DocState {
    uri: String,
    language_id: String,
    server_version: i32,                          // Strictly monotonic version sent to real LSP
    server_rope: ropey::Rope,                     // Exact document text currently inside real LSP
    last_writer: Option<ClientId>,                // Client whose edit last updated server_rope
    clients: HashMap<ClientId, ClientDocState>,   // Active clients holding this URI open
}

struct ClientDocState {
    role: ClientRole,                             // Editor | Agent
    client_version: i32,                          // Last version reported by this client
    rope: ropey::Rope,                            // Exact document text as viewed by this client
    dirty: bool,                                  // True if modified since didOpen/didSave
}
```

### Why `ropey::Rope`?
- `ropey::Rope` is a persistent Copy-on-Write B-tree backed by `Arc` nodes.
- Cloning a `Rope` (`doc.server_rope = client.rope.clone()`) is **$O(1)$** (an atomic reference-count bump, zero text copying).
- Incremental LSP edits apply in **$O(\log N)$** microseconds with built-in UTF-16 code-unit indexing (`line_to_char` + `utf16_cu_to_char`), matching LSP's `Position { line, character }` natively.

---

## Exact State Transitions

### 1. `textDocument/didOpen` (from Client `C`)

1. Normalize `params.textDocument.uri`.
2. **Case A: URI is not in `docs` (refcount `0 → 1`):**
   - Create `rope = Rope::from_str(text)`.
   - Insert `DocState { server_version: 1, server_rope: rope.clone(), last_writer: Some(C), clients: { C -> ClientDocState { rope, dirty: false, ... } } }`.
   - Forward `didOpen` to the real LSP with `version: 1`.
3. **Case B: URI is already in `docs` (refcount `N → N + 1`):**
   - Insert `C` into `doc.clients` with `rope = Rope::from_str(incoming_text)`.
   - **Suppress `didOpen`** to the real LSP.
   - Compare `incoming_text` against `doc.server_rope`:
     - **Equal:** Send nothing to the real LSP.
     - **Different:** Apply configured `open_conflict_policy`:
       - `editor_wins` *(default)*: If an `editor` client currently has `dirty == true` on this URI and `C` is an `agent`, keep `doc.server_rope` untouched so an agent reading the disk copy does not clobber unsaved VS Code edits. Otherwise, update `doc.server_rope = client.rope.clone()`, increment `server_version`, set `last_writer = Some(C)`, and send a full-text `didChange` to the real LSP.
       - `last_writer_wins`: Always update `doc.server_rope` and emit a full-text `didChange`.
   - **Diagnostics Replay:** If `diagnostics_cache` contains an entry for this URI, immediately send that cached `textDocument/publishDiagnostics` notification to Client `C`.

### 2. `textDocument/didChange` (from Client `C`)

1. If `C` is not in `doc.clients` (defensive fallback for non-compliant clients), synthesize a `didOpen` entry first.
2. Apply every `TextDocumentContentChangeEvent` in `params.contentChanges` sequentially to **`client_doc.rope`**:
   - If `range` is `None`: replace `client_doc.rope = Rope::from_str(event.text)`.
   - If `range` is `Some(r)`: convert UTF-16 `(line, character)` start/end to char indices in `client_doc.rope` and replace the slice. Because `client_doc.rope` tracks **only Client `C`'s own timeline**, `r` is always valid against `client_doc.rope`!
   - Mark `client_doc.dirty = true` and update `client_doc.client_version`.
3. Reconcile `client_doc.rope` with `doc.server_rope`:
   - **Exact Match (`client_doc.rope == doc.server_rope`):**
     - **Deduplicate and drop!** (This automatically catches the disk-reload scenario: VS Code's incremental diff transforms VS Code's `client_doc.rope` from `"AAA"` → `"BBBBBB"`, which matches `doc.server_rope`, so zero corrupted diffs and zero wasted re-parses are sent to the real LSP.)
   - **Different & Same Writer (`doc.last_writer == Some(C)` — Fast Path):**
     - `doc.server_rope = client_doc.rope.clone()` ($O(1)$).
     - `doc.server_version += 1`.
     - Rewrite `params.textDocument.version = doc.server_version` and forward Client `C`'s **original incremental `didChange`** straight to the real LSP.
   - **Different & Writer Handoff (`doc.last_writer != Some(C)` — Safe Handoff):**
     - `doc.server_rope = client_doc.rope.clone()` ($O(1)$).
     - `doc.server_version += 1`.
     - `doc.last_writer = Some(C)`.
     - Emit a **full-text `didChange`** (`contentChanges: [{ text: server_rope.to_string() }]`, `version: doc.server_version`) to the real LSP. Per LSP 3.17, servers advertising `TextDocumentSyncKind::Incremental` must still accept full-text change events (omitted `range`), which both `clangd` and `pyright` support natively.

### 3. `textDocument/didSave` (from Client `C`)

- Mark `client_doc.dirty = false` (and if `params.text` is included and differs from `doc.server_rope`, sync `server_rope` via full-text `didChange` first).
- Forward `didSave` to the real LSP (if the server's `InitializeResult` advertised save support), deduplicating identical saves for the same URI within a `50 ms` window.

### 4. `textDocument/didClose` (from Client `C`)

1. Remove `C` from `doc.clients`.
2. **If `doc.clients` is still non-empty** (e.g., agent evicted the file via its 10-file LRU while VS Code still has it open):
   - **Suppress `didClose`** to the real LSP.
   - If `doc.last_writer == Some(C)`, clear `doc.last_writer = None` so the next edit from a remaining client triggers a clean full-text handoff if needed.
3. **If `doc.clients` is now empty (refcount `1 → 0`):**
   - Remove `DocState` from `docs`.
   - Forward `didClose` to the real LSP.

### 5. Abrupt Client Disconnect / Crash Cleanup

Whenever a client's IPC connection closes (clean exit, `SIGKILL`, crashed terminal, or VS Code reload):
1. Iterate over all URIs where `client_id ∈ doc.clients` and execute the exact `didClose` transition above (emitting real `didClose` only for URIs whose refcount reaches `0`).
2. For every in-flight request in `pending` owned by `client_id`, send `$/cancelRequest` with its `server_id` to the real LSP and remove it from `pending`.
3. If `client_id` was the primary client, immediately re-elect a new primary.
4. If 0 clients remain attached to the `InstanceActor`, arm the `idle_timeout_secs` shutdown timer.
