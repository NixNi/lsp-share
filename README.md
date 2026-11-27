# lsp-share

A single-binary, cross-platform (**Windows**, **macOS**, **Linux**) Language Server Protocol (LSP) multiplexer written in Rust. `lsp-share` lets editors (such as VS Code) and AI coding agent LSP clients share a single warm language server instance per workspace (`clangd`, `pyright`, etc.) with sub-millisecond proxying overhead.

## Key Features

- **Single Static Binary:** One executable (`lsp-share`) acting as daemon, stdio proxy, control CLI, and `argv[0]` shim.
- **Cross-Platform IPC:** Unix Domain Sockets on macOS/Linux and Named Pipes on Windows (`\\.\pipe\lsp-share-<user>`), with race-free lockfile daemon auto-start and Windows Job Object orphan protection.
- **Aggressive Name Matching (Default):** Clients pointing to different binary paths or cosmetic CLI flags (e.g. `/opt/llvm/bin/clangd` vs `/usr/bin/clangd`) share the same server instance automatically, with optional `canonical_path` or explicit `--key` overrides.
- **Safe Concurrent Editing:** Per-client Copy-on-Write `Rope` (`ropey`) tracking prevents disk-reload incremental diffs from corrupting server buffers, suppresses premature `didClose` from agent LRU caches, and replays cached `publishDiagnostics` immediately to newly joining clients.
- **Fast Zero-Copy Routing:** Partial JSON-RPC envelope parsing (`serde_json::value::RawValue`) forwards requests and megabyte-scale completions/tokens without deserializing bodies.

## Building

### Standard Build
```bash
cargo build --release
# Output: target/release/lsp-share (or lsp-share.exe on Windows)
```

### Offline / Closed-Contour Build
```bash
cargo vendor
cargo build --release --offline
```

## Quickstart

### Option 1: Direct Stdio Proxy (`lsp-share proxy`)
Point both your editor LSP client and your coding agent extension to `lsp-share proxy`:

```bash
# Editor client
lsp-share proxy --role editor -- clangd --background-index

# Agent client (automatically shares the same clangd instance for the same workspace)
lsp-share proxy --role agent -- clangd
```

For `pyright`:
```bash
lsp-share proxy -- pyright-langserver --stdio
```

### Option 2: Transparent `argv[0]` Shim
When an editor setting only accepts a single binary path (no extra arguments):

```bash
# Default location (~/.lsp-share/bin/clangd or %USERPROFILE%\.lsp-share\bin\clangd.exe)
lsp-share shim add clangd --target /opt/homebrew/bin/clangd

# Or place the shim anywhere you want (custom file path or directory via -o/--output):
lsp-share shim add D:\shims\clangd.exe --target C:\LLVM\bin\clangd.exe
lsp-share shim add clangd -o D:\shims\ --target C:\LLVM\bin\clangd.exe
```

Then configure `"clangd.path"` to point to your shim path (e.g. `~/.lsp-share/bin/clangd` or `D:\shims\clangd.exe`).

## CLI Commands

- `lsp-share proxy [--role auto|editor|agent] [--match-by name|canonical_path] [--key <id>] [--root <dir>] -- <cmd> [args...]`
- `lsp-share daemon [--foreground] [--idle-timeout <secs>] [--trace <file.jsonl>]`
- `lsp-share status [--json]`
- `lsp-share stop [<instance_id> | --all | --daemon]`
- `lsp-share restart <instance_id>`
- `lsp-share shim add|list|remove ...`

## Documentation

Detailed design and protocol specifications live in [`docs/`](./docs/README.md):

- [Architecture & Cross-Platform IPC](./docs/architecture.md)
- [Wire Protocol & LSP Multiplexing](./docs/protocol.md)
- [Document Synchronization Engine](./docs/document-sync.md)
- [CLI, Shims & Configuration](./docs/cli-and-config.md)
