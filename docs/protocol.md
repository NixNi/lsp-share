# Wire Protocol & LSP Multiplexing

## 1. Internal Binary Wire Protocol (`proxy` ↔ `daemon`)

To eliminate redundant HTTP-style `Content-Length` header parsing inside the daemon and allow control traffic on the same socket, the internal IPC channel uses a 5-byte binary frame header:

```
┌────────────┬──────────────────┬─────────────────────────────┐
│ type (1 B) │ length (4 B, BE) │ payload (`length` bytes)    │
└────────────┴──────────────────┴─────────────────────────────┘
```

Maximum frame size is capped at a generous safety limit (e.g., `64 MiB`) to protect against malformed frames while easily supporting large `completion` or `semanticTokens` payloads.

### Frame Types

| Byte | Name | Direction | Payload |
|---|---|---|---|
| `0x01` | `HELLO` | Client → Daemon | UTF-8 JSON session handshake metadata |
| `0x02` | `HELLO_ACK` | Daemon → Client | UTF-8 JSON acceptance/rejection & assigned `client_id` |
| `0x03` | `LSP_MESSAGE` | Bidirectional | Raw JSON-RPC 2.0 body bytes (no `Content-Length` header) |
| `0x04` | `CONTROL_REQ` | CLI → Daemon | UTF-8 JSON control command (`status`, `stop`, `restart`, `shutdown_daemon`) |
| `0x05` | `CONTROL_RESP` | Daemon → CLI | UTF-8 JSON control response |

### `HELLO` Payload (`mode: "lsp"`)

```json
{
  "protocol_version": 1,
  "mode": "lsp",
  "client_pid": 48210,
  "role": "auto",
  "command": "/opt/homebrew/bin/clangd",
  "args": ["--background-index"],
  "cwd": "/Users/dev/project",
  "env": {
    "PATH": "...",
    "VIRTUAL_ENV": "/Users/dev/project/.venv",
    "CONDA_PREFIX": null
  },
  "match_by": null,
  "key_override": null,
  "root_override": null
}
```

### `HELLO_ACK` Payload

```json
{
  "protocol_version": 1,
  "ok": true,
  "client_id": 7,
  "error": null
}
```

---

## 2. Instance Key & Late Binding

### Why Late Binding on `initialize`?

Different clients (e.g., VS Code vs. an agent extension) often spawn the proxy process with different working directories (`cwd`), even when targeting the exact same repository. Therefore, the daemon **does not bind a client to an LSP instance during `HELLO`**. Instead:

1. The client completes `HELLO` / `HELLO_ACK`.
2. The daemon waits for the client's first `LSP_MESSAGE`, which per LSP spec is `initialize`.
3. The daemon extracts the canonical workspace root in priority order:
   1. `HELLO.root_override` (if explicitly passed via `--root`)
   2. `initialize.params.workspaceFolders[0].uri`
   3. `initialize.params.rootUri`
   4. `initialize.params.rootPath`
   5. `HELLO.cwd`
4. The root path is normalized:
   - Decode percent-encoded URI segments (`file:///c%3A/...` → `C:/...`).
   - Resolve symlinks via `std::fs::canonicalize` when the path exists.
   - Normalize Windows drive letters to uppercase and strip trailing slashes.

### Instance Key Formula & Aggressive Matching by Name (Default)

```
InstanceKey = (server_key, normalized_root_uri, semantic_args_key, env_key)
```

- **`server_key` (`binary_matching` policy):**
  - **`name` (Default — Aggressive Matching):** Uses the normalized executable basename, stripping platform extensions (`.exe`, `.cmd`, `.bat`) and lowercasing on Windows.
    - Example: `/Users/dev/.vscode/clangd/18/bin/clangd` and `/usr/bin/clangd` both produce `server_key = "clangd"` and **share the same instance**.
    - If a second client connects with a different binary path than the running instance, it still attaches to the existing instance, and the daemon logs a notice (also visible in `lsp-share status`).
    - Users can configure `binary = "/preferred/path/to/clangd"` in `config.toml` so the daemon always spawns the preferred executable regardless of which client connects first.
  - **`canonical_path` (Optional strict mode):** Resolves `command` via `which` + `fs::canonicalize()`. Symlinks to the same file share an instance; distinct binaries stay isolated.
  - **`--key <id>` (Explicit override):** When passed by a custom client, replaces `server_key` with the exact user-provided logical ID (e.g., `--key cpp-main`).

- **`semantic_args_key` (`arg_matching` policy):**
  - **`ignore` (Default):** Ignores cosmetic CLI differences (e.g., `--log=verbose`, `--pretty`, `--header-insertion=...`) so clients with slightly different launch flags still share the same instance.
  - **`semantic`:** Only includes configured semantic flags (e.g., `--compile-commands-dir`, `--query-driver` for `clangd`) in the key.
  - **`exact`:** Includes all normalized arguments in the key.

- **`env_key`:**
  - Only hashes explicitly configured environment variables for that server (e.g., `["VIRTUAL_ENV", "CONDA_PREFIX"]` for `pyright`; empty for `clangd`).

---

## 3. Zero-Copy Partial JSON-RPC Routing

Large LSP responses (`textDocument/completion`, `textDocument/semanticTokens/full`, `workspace/symbol`) frequently exceed several megabytes. Fully deserializing and re-serializing them on every hop wastes CPU and adds milliseconds of latency.

Instead, the daemon parses only a minimal JSON-RPC envelope using `serde_json::value::RawValue`:

```rust
#[derive(Deserialize)]
struct Envelope<'a> {
    #[serde(borrow)]
    id: Option<&'a RawValue>,
    method: Option<&'a str>,
    #[serde(borrow)]
    params: Option<&'a RawValue>,
    #[serde(borrow)]
    result: Option<&'a RawValue>,
    #[serde(borrow)]
    error: Option<&'a RawValue>,
}
```

- **Fast path (~95% of traffic):** Requests (`hover`, `definition`, `references`, `completion`, `diagnostic`, custom `clangd/*` extensions, etc.) and their responses **never parse `params` or `result`**. The actor only rewrites the top-level `"id"` field and writes the raw slices directly into the outbound buffer.
- **Control path:** Deep JSON parsing is performed only for stateful lifecycle/sync messages (`initialize`, `didOpen`, `didChange`, `didSave`, `didClose`, `$/cancelRequest`, `publishDiagnostics`, `registerCapability`, `workspace/configuration`).

---

## 4. Client Roles & Primary Election

Each connected client has a role (`editor` or `agent`):

- Explicit via `--role editor|agent` (or `HELLO.role`).
- Automatic (`role = "auto"`): inferred from `initialize.params.clientInfo.name` (known editors like `"Visual Studio Code"`, `"Neovim"`, `"Zed"`, etc. → `editor`; others → `agent`).

An `InstanceActor` maintains a **primary client** for server-initiated queries (`workspace/configuration`, `workspace/applyEdit`, `window/showMessageRequest`):
- Prefer the oldest connected `editor` client.
- If no `editor` is connected, fall back to the oldest connected `agent` client.
- Re-elect immediately whenever the current primary disconnects.

---

## 5. Message-by-Message Multiplexing Matrix

### Client → Server

| Message | Multiplexing Behavior |
|---|---|
| `initialize` (1st client) | Merge client capabilities with the daemon's rich base editor capability profile (preventing degraded features if an agent connects before VS Code). Apply server rules (e.g., force `utf-16` for `clangd`). Forward to real LSP, wait for `InitializeResult`, cache it, and reply to the client. |
| `initialize` (2nd+ client) | Immediately synthesize and return the cached `InitializeResult` (~0.1 ms) without contacting the real LSP. Record this client's individual capabilities (e.g., whether it supports `workDoneProgress` or dynamic registration). |
| `initialized` | Forwarded only for the 1st client. Swallowed for 2nd+ clients; immediately push cached dynamic registrations (`client/registerCapability`) if the joining client supports them. |
| `shutdown` | Do **not** forward to the real LSP. Reply immediately with `{"jsonrpc":"2.0","id":<id>,"result":null}` and mark the client as shutting down. |
| `exit` | Close the client's session, run disconnect cleanup (refcounted `didClose` + cancel pending requests), and start the `idle_timeout` timer if 0 clients remain. |
| Standard & custom requests (`textDocument/*`, `workspace/*`, `clangd/*`, etc.) | Allocate a monotonic `server_id: u64`, store `pending[server_id] = (client_id, original_id)`, rewrite `id` via zero-copy envelope, and forward to the real LSP. |
| `$/cancelRequest` | Look up `server_id` for `(client_id, params.id)`, rewrite `params.id` to `server_id`, and forward to the real LSP. |
| `textDocument/didOpen`, `didChange`, `didSave`, `didClose` | Handled by the [Document Synchronization Engine](./document-sync.md). |
| `workspace/didChangeWatchedFiles` | Forward to the real LSP, deduplicating identical URI+change-type bursts within a short window (~50 ms) when both VS Code and an agent report the same disk change. |
| `workspace/didChangeConfiguration` | Forward to the real LSP and invalidate the daemon's `workspace/configuration` cache. |

### Server → Client

| Message | Multiplexing Behavior |
|---|---|
| Response (`id` present, no `method`) | Pop `(client_id, original_id)` from `pending[server_id]`, rewrite `id` back to `original_id` (zero-copy `result`/`error`), and send exclusively to `client_id`. If `client_id` already disconnected, discard silently. |
| `textDocument/publishDiagnostics` | Store the latest raw diagnostics notification in `diagnostics_cache[normalized_uri]` and broadcast to all connected clients. (On `didOpen` by a newly joining client, replay the cached notification immediately.) |
| `client/registerCapability` / `unregisterCapability` | Daemon replies immediately to the server with `result: null`, updates its active registration registry, and forwards the registration to connected clients that support dynamic registration for that capability. |
| `window/workDoneProgress/create` | Daemon replies immediately to the server with `result: null` and tracks active progress tokens. |
| `$/progress` | Broadcast to connected clients that advertised `window.workDoneProgress: true` (lazily synthesizing `workDoneProgress/create` first if the client joined mid-progress). |
| `workspace/configuration` | If a primary client is connected, forward to primary (with remapped server-request ID), cache the returned settings per `(scopeUri, section)`, and relay the response to the LSP. If no primary is connected (or primary lacks capability), answer directly from cache (or `null` array matching item count). |
| `workspace/workspaceFolders` | Answered directly by the daemon using the union of active workspace folders across connected clients. |
| `workspace/semanticTokens/refresh`, `inlayHint/refresh`, `diagnostic/refresh`, `codeLens/refresh` | Daemon replies immediately to the server with `result: null` (preventing a slow client from blocking the server) and broadcasts the refresh request asynchronously to capable clients. |
| `workspace/applyEdit` | Route to the client that initiated the associated `workspace/executeCommand` if active, otherwise to the primary `editor` client. If no client can apply edits, return `{"applied": false}`. |
| `window/showMessage`, `window/logMessage`, `telemetry/event` | Broadcast to all connected clients (with optional per-role filtering). |

---

## 6. Target Server Specifics (`clangd` & `pyright`)

### `clangd`
- **Position Encoding Normalization (`force_utf16 = true`):** `clangd` switches to UTF-8 column offsets if the first client advertises `general.positionEncodings: ["utf-8"]` or legacy `offsetEncoding: ["utf-8"]`. If a second client expects standard UTF-16, all positions break on non-ASCII lines. The daemon normalizes position encodings to `["utf-16"]` during the first `initialize` so all clients agree on coordinates.
- **Custom Extensions:** `textDocument/switchSourceHeader`, `textDocument/symbolInfo`, `textDocument/ast`, `textDocument/memoryUsage`, and `$clangd/inactiveRegions` work automatically via zero-copy passthrough.
- **Compile Database Flags:** `arg_matching = "semantic"` can isolate instances only when `--compile-commands-dir` or `--query-driver` actually differ, while ignoring cosmetic flags.

### `pyright` (`pyright-langserver`)
- **Stdio Flag:** `default_args = ["--stdio"]` ensures `pyright-langserver` always starts in stdio mode.
- **Configuration & Virtualenvs:** `pyright` relies heavily on `workspace/configuration` (`python`, `python.analysis`, `pyright`) for interpreter resolution (`python.pythonPath`) and `extraPaths`, as well as `VIRTUAL_ENV` / `CONDA_PREFIX` environment variables. Primary routing + config caching keeps interpreter resolution consistent even when only agent clients are connected.
- **Pull Diagnostics:** Newer `pyright` builds support LSP 3.17 pull diagnostics (`textDocument/diagnostic`, `workspace/diagnostic`), which are request/response based and multiplex cleanly through standard ID remapping.
