# Architecture & Cross-Platform IPC

## High-Level Topology

`lsp-share` ships as a **single static binary** that operates in three modes:

1. **Daemon (`lsp-share daemon`)** — One long-lived background process per OS user. Listens on a deterministic local IPC endpoint and manages a pool of real LSP child processes.
2. **Thin Stdio Proxy (`lsp-share proxy -- <lsp> [args...]`)** — Lightweight bridge launched by VS Code or agent clients. Auto-starts the daemon if not running, forwards stdio LSP frames over the local IPC channel, and exits cleanly when its client disconnects.
3. **Control / Shim CLI (`status`, `stop`, `restart`, `shim`)** — Queries or manages the running daemon over the same IPC endpoint, or dispatches transparently via `argv[0]` when invoked as a shim (e.g., `~/.lsp-share/bin/clangd`).

```
VS Code Client  ──stdio──▶ lsp-share proxy ──┐
                                             ├──(UDS / Named Pipe)──▶ [ lsp-share daemon ]
Agent Client 1  ──stdio──▶ lsp-share proxy ──┤                              │
                                             │                Pool (InstanceKey -> Actor)
Agent Client 2  ──stdio──▶ lsp-share proxy ──┘                              │
                                                                 ┌──────────┴──────────┐
                                                                 ▼                     ▼
                                                          InstanceActor         InstanceActor
                                                         (clangd, repoA)       (pyright, repoA)
                                                                 │ stdio               │ stdio
                                                            [ clangd ]       [ pyright-langserver ]
```

*(Future optimization: custom agent clients can optionally bypass the stdio proxy process and speak the internal binary protocol directly over the daemon's IPC endpoint.)*

## Concurrency Model: Actor per LSP Instance

Inside the daemon, each real LSP server process is owned exclusively by a dedicated `tokio` task (**`InstanceActor`**):

- **No shared mutexes on the hot path:** Clients communicate with an `InstanceActor` over bounded `tokio::sync::mpsc` channels.
- **Reader / Writer split per connection:**
  - Each client IPC connection spawns an independent reader task (pushes frames to the actor) and writer task (drains an `mpsc` channel to the client socket).
  - Each real LSP child process spawns an independent stdout reader task (parses `Content-Length` frames and pushes to the actor) and stdin writer task (drains an `mpsc` channel to the child's stdin).
  - A slow or stalled client cannot block the actor or other connected clients; bounded outbound queues apply backpressure or drop non-critical notifications (`$/progress`, `window/logMessage`) if a client stops reading.
- **Pool registry:** A lightweight `Pool` manager handles late binding on `initialize`, spawning new `InstanceActor`s on demand, routing `CONTROL_REQ` commands (`status`, `stop`, `restart`), and reaping idle actors.

## Deterministic Cross-Platform IPC

Clients locate the daemon **without scanning system processes** and **without allocating random TCP ports** (avoiding port collisions, Windows Defender Firewall prompts, and TCP loopback overhead). Every process for the same OS user computes the exact same local IPC address:

| Platform | IPC Mechanism | Endpoint Path | Notes |
|---|---|---|---|
| **Linux** | Unix Domain Socket | `$XDG_RUNTIME_DIR/lsp-share/daemon.sock` (fallback: `/tmp/lsp-share-<uid>/daemon.sock`) | Parent directory created with `0700` permissions for per-user isolation. |
| **macOS** | Unix Domain Socket | `/tmp/lsp-share-<uid>/daemon.sock` | Short path guarantees staying well below macOS's 104-byte `sun_path` limit; directory permissions `0700`. |
| **Windows** | Named Pipe | `\\.\pipe\lsp-share-<user-sid>` | Native `tokio::net::windows::named_pipe`. Kernel cleans up pipe automatically on crash; zero stale-file issues. |

An internal `ipc` module exposes a unified async `LocalListener` / `LocalStream` (`AsyncRead + AsyncWrite`) abstraction across all three platforms.

### Race-Free Daemon Auto-Start

When `lsp-share proxy` starts, it executes a double-checked locking sequence:

1. **Fast path (~50 µs):** Attempt `connect()` directly to the deterministic IPC endpoint. If the daemon is already running, proceed immediately to `HELLO` handshake.
2. **Cold start (on `ENOENT` / `ECONNREFUSED` / `ERROR_FILE_NOT_FOUND`):**
   1. Acquire an OS-level exclusive lock on `daemon.lock` (`flock` on Unix, `LockFileEx` on Windows).
   2. Retry `connect()` once inside the lock (if VS Code and an agent started at the same millisecond, only one spawns the daemon).
   3. On Unix, remove any stale `daemon.sock` left behind by a `SIGKILL`ed daemon.
   4. Spawn `lsp-share daemon` detached from the caller:
      - **Unix:** new session/process group (`setsid` / `process_group(0)`), `stdin`/`stdout`/`stderr` redirected to `/dev/null` (logs go to daemon log file).
      - **Windows:** `CREATE_NO_WINDOW | DETACHED_PROCESS` creation flags (`0x08000000 | 0x00000008`), null stdio.
   5. Poll `connect()` with short exponential backoff (`5 ms → 10 ms → 20 ms → 40 ms`, up to `3 s`).
   6. Release `daemon.lock`.
3. **Protocol version check:** During the `HELLO` handshake, the proxy sends `protocol_version`. If the running daemon reports an incompatible version (e.g., after upgrading the `lsp-share` binary), the daemon reports a clear mismatch error (or optionally triggers a graceful restart when no active sessions remain).

## Child Process Lifecycle & OS Safeguards

- **Idle Keep-Alive (`idle_timeout_secs`):** When the last client disconnects from an `InstanceActor`, the real LSP process stays warm for a configurable TTL (default: `30 s`, `0` = keep alive until explicit stop or daemon exit). If a client reconnects before expiration, the timer is cancelled. Once all instances and client connections are gone, the daemon itself also exits cleanly after a short grace period (`<= 5 s`).
- **Graceful LSP Shutdown:** On idle expiration or `lsp-share stop`:
  1. Daemon sends LSP `shutdown` request to the real server (timeout: `3 s`).
  2. Daemon sends LSP `exit` notification (timeout: `2 s`).
  3. Escalate to `SIGTERM` (Unix) / terminate (Windows), then `SIGKILL` after grace period.
- **Orphan Prevention:**
  - **Windows:** All spawned LSP child processes are assigned to a Windows **Job Object** configured with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. If the daemon crashes or is force-killed, the OS immediately terminates all child LSPs.
  - **Windows `.cmd`/`.bat` shims:** Resolve executables via `which` and invoke `.cmd`/`.bat` wrappers (common for Node-based servers like `pyright-langserver.cmd`) properly via `cmd.exe /C` or direct Node execution without spawning console windows (`CREATE_NO_WINDOW`).
  - **Unix:** Each child LSP runs in its own process group so termination cleans up helper subprocesses (e.g., compiler drivers spawned by `clangd`).
- **Unexpected LSP Crash:** If a real LSP process exits unexpectedly, the `InstanceActor` fails all pending client requests with JSON-RPC error `-32603` ("Underlying language server exited"), notifies/closes connected client sessions cleanly, and removes itself from the pool so the next reconnect starts fresh.

## Proposed Rust Crate Structure & Minimal Dependencies

To stay friendly to closed/air-gapped environments (`cargo vendor`), the dependency graph is intentionally small and pure-Rust:

- **Core crates:** `tokio` (rt-multi-thread, net, io-util, process, sync, time, signal), `serde`, `serde_json` (`raw_value` feature), `ropey`, `toml`, `clap`, `tracing`, `tracing-subscriber`, `which`, `fs4` (or `std::fs` locking), `windows-sys` (Windows target only, for Job Objects and user SID lookup), `libc` (Unix target only, for UID and `setsid`).

```
src/
├── main.rs              # Entry point: argv[0] shim dispatch + CLI subcommands
├── config.rs            # config.toml loading, defaults, per-server rules
├── ipc/
│   ├── mod.rs           # Cross-platform LocalListener / LocalStream + lockfile + auto-start
│   ├── unix.rs          # Unix Domain Socket transport & UID path resolution
│   └── windows.rs       # Windows Named Pipe transport, SID resolution & Job Object
├── protocol/
│   ├── mod.rs
│   ├── frame.rs         # Binary IPC framing (HELLO / LSP_MESSAGE / CONTROL) & stdio Content-Length framing
│   └── jsonrpc.rs       # Partial zero-copy JSON-RPC envelope (RawValue) & ID rewriting
├── daemon/
│   ├── mod.rs           # Daemon bootstrap, signal handling, log setup
│   ├── server.rs        # IPC accept loop, HELLO handshake, CONTROL_REQ router
│   ├── pool.rs          # InstanceKey calculation, late binding, idle reaping
│   ├── instance.rs      # InstanceActor: LSP lifecycle, ID map, capabilities, S->C requests, diagnostics cache
│   └── documents.rs     # Per-client CoW Rope, server_rope, refcounting, didChange reconciliation
├── proxy.rs             # Thin stdio <-> IPC bridge
├── control.rs           # CLI commands: status, stop, restart
└── shim.rs              # `lsp-share shim add/list/remove` & argv[0] real-binary resolution
```
