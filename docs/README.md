# lsp-share Design Documentation

`lsp-share` is a single-binary, cross-platform (Windows, macOS, Linux) Language Server Protocol (LSP) multiplexer written in Rust. It allows multiple persistent LSP clients—such as a VS Code client and one or more AI coding agent clients—to share a single warm language server instance per workspace with sub-millisecond proxying overhead.

## Problem Statement

In agentic development workflows, both an interactive editor (e.g., VS Code) and coding agent LSP clients work on the same repository simultaneously using heavy language servers such as `clangd` and `pyright`. Running independent LSP instances per client leads to:

- Multiplied RAM and CPU usage from redundant indexing and AST caches.
- Slow startup times and duplicated background indexing whenever a new agent session connects.
- Inconsistent diagnostics and wasted recomputation when files change.

Because LSP is a stateful 1:1 protocol over stdio, naive byte forwarding between multiple clients and one server fails immediately (duplicate `initialize`, colliding JSON-RPC IDs, conflicting `didOpen`/`didClose`/`didChange` state, and premature `shutdown`).

## Design Goals

- **Single zero-dependency binary** compiled from Rust, targeting **Windows, macOS, and Linux**, with a minimal dependency graph suitable for offline/air-gapped (`cargo vendor`) builds.
- **Minimal latency (< 0.2 ms proxy overhead):** Binary length-prefixed internal IPC over Unix Domain Sockets (macOS/Linux) and Named Pipes (Windows), actor-per-instance concurrency with zero global lock contention, and partial zero-copy JSON-RPC routing (`serde_json::value::RawValue`).
- **Transparent stdio compatibility:** Works as a drop-in proxy (`lsp-share proxy -- <cmd>`) or transparent `argv[0]` shim (`~/.lsp-share/bin/<server>`) for any stdio LSP client and any LSP server, including custom JSON-RPC extensions.
- **Aggressive instance sharing by default:** Matches language servers by normalized binary name (e.g., `clangd`, `pyright-langserver`) and normalized workspace root so clients with different binary paths or cosmetic CLI flags share the same instance out of the box, with overrides for strict path matching or explicit logical keys.
- **Safe concurrent document editing:** Supports multiple simultaneous readers and writers (including editors and agents sending `didOpen`, `didChange`, and LRU-based `didClose`) without corrupting incremental document state or prematurely closing files.

## Non-Goals

- Hijacking already-running LSP processes that were not launched through `lsp-share` (OS anonymous stdio pipes cannot be safely intercepted externally).
- Translating proprietary non-stdio transports such as VS Code's `--node-ipc` (clients are expected to speak standard LSP over stdio, or optionally connect directly to the `lsp-share` IPC socket).
- Acting as a multi-server aggregator (combining different LSPs like `ruff` + `pyright` into a single client connection).

## Document Index

1. [Architecture & Cross-Platform IPC](./architecture.md) — Daemon/shim topology, actor model, Unix sockets & Windows Named Pipes, race-free auto-start, process cleanup, and crate structure.
2. [Wire Protocol & LSP Multiplexing](./protocol.md) — Internal binary framing, instance key resolution and late binding, zero-copy routing, message-by-message multiplexing rules, and `clangd`/`pyright` specifics.
3. [Document Synchronization Engine](./document-sync.md) — Per-client Copy-on-Write `Rope` model, `didOpen`/`didClose` refcounting, writer handoff, disk-reload deduplication, and diagnostics replay.
4. [CLI, Shims & Configuration](./cli-and-config.md) — Subcommands, `argv[0]` transparent shims, `config.toml` schema, and wire-tap tracing for debugging.
