# CLI, Shims & Configuration

Everything is packaged into a single standalone binary: `lsp-share` (`lsp-share.exe` on Windows).

---

## 1. CLI Subcommands

### `lsp-share proxy` (Primary Integration Mode)
Runs as a transparent stdio LSP proxy, auto-starting the daemon if needed.

```bash
lsp-share proxy [OPTIONS] -- <COMMAND> [ARGS...]
```

Options:
- `--role <auto|editor|agent>` — Client role hint (default: `auto`, detected via `initialize.params.clientInfo.name`).
- `--match-by <name|canonical_path>` — Override server matching policy for this connection (default from `config.toml`, which defaults to `name`).
- `--key <ID>` — Explicit logical instance key (overrides `binary_matching`, e.g., `--key cpp-clangd`). Ideal for custom-built editor and agent clients.
- `--root <PATH>` — Explicit workspace root override (skips `initialize` root extraction).
- `--trace <FILE>` — Append multiplexed JSON-RPC frames for this session to a JSONL trace file for debugging.

### `lsp-share daemon`
Runs the background multiplexer daemon (normally auto-spawned by `proxy`).

```bash
lsp-share daemon [--foreground] [--config <PATH>] [--idle-timeout <SECS>] [--log-level <LEVEL>] [--trace <FILE>]
```

### `lsp-share status`
Queries the running daemon over IPC and displays active LSP instances and attached clients.

```bash
lsp-share status [--json]
```

Example output:
```
Daemon: PID 18421 | uptime 1h 12m | socket /tmp/lsp-share-501/daemon.sock

ID              SERVER   ROOT                      PID    CLIENTS  OPEN DOCS  IDLE
clangd#a1b2     clangd   /Users/dev/my-repo        18425  2 (1e,1a) 14        active
  └─ #1 [editor] Visual Studio Code (pid 18001)  cmd=/opt/llvm/bin/clangd
  └─ #2 [agent]  custom-agent-lsp   (pid 19104)  cmd=/usr/bin/clangd (matched by name)
pyright#c3d4    pyright  /Users/dev/my-repo        18502  1 (0e,1a) 6         active
```

### `lsp-share stop` / `lsp-share restart`
```bash
lsp-share stop <INSTANCE_ID>     # Gracefully shut down one LSP instance
lsp-share stop --all             # Shut down all LSP instances (keep daemon running)
lsp-share stop --daemon          # Gracefully shut down all LSPs and exit daemon
lsp-share restart <INSTANCE_ID>  # Restart an LSP instance (clients reconnect cleanly)
```

### `lsp-share shim`
Manages transparent `argv[0]` shims for editors or tools whose settings only accept a single executable path (e.g., `"clangd.path"`).

```bash
lsp-share shim add <NAME> [--target <REAL_BINARY_PATH>] [--role <editor|agent>]
lsp-share shim list
lsp-share shim remove <NAME>
```

---

## 2. Transparent `argv[0]` Shim Dispatch (BusyBox Mode)

When `lsp-share` starts, it inspects its own invoked filename (`argv[0]`):

- If the executable name (without `.exe`) is **`lsp-share`**: parse normal CLI subcommands (`proxy`, `daemon`, `status`, etc.).
- Otherwise (e.g., invoked as `~/.lsp-share/bin/clangd` or `%USERPROFILE%\.lsp-share\bin\clangd.exe`):
  1. Look up `<NAME>` in `[shims]` inside `config.toml` for an explicit `target` binary path and optional default `role` / `key`.
  2. If not explicitly mapped, search `PATH` for `<NAME>` while **excluding** `~/.lsp-share/bin` (preventing infinite recursion).
  3. Execute the exact same code path as `lsp-share proxy -- <resolved_real_binary> <passed_args...>`.

### Cross-Platform Shim Creation (`lsp-share shim add`)
- **macOS / Linux:** Creates a symlink `~/.lsp-share/bin/<name> -> /path/to/lsp-share`.
- **Windows:** Attempts in order without requiring Administrator privileges:
  1. Symlink (succeeds if Developer Mode is enabled).
  2. NTFS Hard Link (`std::fs::hard_link` — works without admin on the same volume).
  3. File Copy fallback (copies the small `lsp-share.exe` binary to `%USERPROFILE%\.lsp-share\bin\<name>.exe`).

---

## 3. Configuration File (`config.toml`)

`lsp-share` works **zero-config** out of the box with sensible defaults. An optional configuration file can customize behavior:

- **Linux:** `$XDG_CONFIG_HOME/lsp-share/config.toml` (fallback: `~/.config/lsp-share/config.toml`)
- **macOS:** `~/.config/lsp-share/config.toml`
- **Windows:** `%APPDATA%\lsp-share\config.toml`

### Full Schema with Defaults

```toml
[daemon]
# Keep an LSP server warm after its last client disconnects (0 = never auto-stop)
idle_timeout_secs = 30
# Log level: error | warn | info | debug | trace
log_level = "info"
# Optional global JSONL wire-tap log for debugging custom LSP clients
# trace_file = "/tmp/lsp-share-trace.jsonl"

[matching]
# Default binary matching strategy:
#   "name"           - Aggressive matching by normalized binary name (default)
#   "canonical_path" - Strict matching by resolved canonical executable path
binary_matching = "name"

# Default CLI argument matching strategy:
#   "ignore"   - Ignore CLI args when matching an existing instance (default)
#   "semantic" - Match only on configured `semantic_args`
#   "exact"    - Require exact argument match
arg_matching = "ignore"

[documents]
# Conflict policy when a 2nd client sends didOpen with text differing from current buffer:
#   "editor_wins"      - Keep editor's dirty unsaved buffer if an agent opens disk copy (default)
#   "last_writer_wins" - Always adopt incoming text
open_conflict_policy = "editor_wins"

# Deduplication window (ms) for bursts of identical didSave / didChangeWatchedFiles
event_dedup_window_ms = 50

# --- Per-server overrides (keyed by normalized binary name) ---

[servers.clangd]
# Optional: always launch this preferred binary even if a client requested a different path
# binary = "/opt/llvm-19/bin/clangd"
binary_matching = "name"
arg_matching = "semantic"
semantic_args = ["--compile-commands-dir", "--query-driver"]
default_args = ["--background-index", "--clang-tidy"]
force_utf16 = true
idle_timeout_secs = 600

[servers.pyright-langserver]
# binary = "/opt/pyright/bin/pyright-langserver"
binary_matching = "name"
arg_matching = "ignore"
default_args = ["--stdio"]
# Include selected env vars in instance key if projects use distinct virtualenvs
env_key_vars = ["VIRTUAL_ENV", "CONDA_PREFIX"]
idle_timeout_secs = 300

# --- Managed by `lsp-share shim add` ---
[shims.clangd]
target = "/opt/homebrew/bin/clangd"
# role = "editor"
# key = "cpp-main"
```

---

## 4. Observability & Debugging (Closed-Contour Ready)

- **Daemon Logs:** Written to a rotating/persistent log file in the OS state directory (`$XDG_STATE_HOME/lsp-share/daemon.log`, `~/Library/Logs/lsp-share/daemon.log`, or `%LOCALAPPDATA%\lsp-share\daemon.log`), never polluting stdio.
- **Wire-Tap Trace (`--trace` / `trace_file`):** Optional newline-delimited JSON (`.jsonl`) recording timestamp, `instance_id`, `client_id`, `direction` (`C->D`, `D->S`, `S->D`, `D->C`), `action` (`forwarded`, `cached_reply`, `suppressed`, `deduplicated`, `handoff_full_sync`), `method`, and message size/payload. Makes diagnosing custom LSP client interactions straightforward.
