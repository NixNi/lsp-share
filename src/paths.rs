use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IpcEndpoint {
    #[cfg(unix)]
    UnixSocket(PathBuf),
    #[cfg(windows)]
    NamedPipe(String),
}

pub fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(unix)]
pub fn current_uid() -> u32 {
    unsafe { libc::geteuid() }
}

pub fn home_dir() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
    }
}

pub fn runtime_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("LSP_SHARE_RUNTIME_DIR") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir);
        }
    }

    #[cfg(target_os = "macos")]
    {
        PathBuf::from(format!("/tmp/lsp-share-{}", current_uid()))
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
            if !xdg.trim().is_empty() {
                return PathBuf::from(xdg).join("lsp-share");
            }
        }
        PathBuf::from(format!("/tmp/lsp-share-{}", current_uid()))
    }

    #[cfg(windows)]
    {
        let base = std::env::var_os("LOCALAPPDATA")
            .or_else(|| std::env::var_os("TEMP"))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Temp"));
        base.join("lsp-share")
    }
}

pub fn ipc_endpoint() -> IpcEndpoint {
    if let Ok(sock) = std::env::var("LSP_SHARE_SOCKET") {
        let trimmed = sock.trim();
        if !trimmed.is_empty() {
            #[cfg(unix)]
            {
                return IpcEndpoint::UnixSocket(PathBuf::from(trimmed));
            }
            #[cfg(windows)]
            {
                return IpcEndpoint::NamedPipe(trimmed.to_string());
            }
        }
    }

    #[cfg(unix)]
    {
        IpcEndpoint::UnixSocket(runtime_dir().join("daemon.sock"))
    }

    #[cfg(windows)]
    {
        if let Ok(dir) = std::env::var("LSP_SHARE_RUNTIME_DIR") {
            let trimmed = dir.trim();
            if !trimmed.is_empty() {
                let hash = fnv1a_64(trimmed.as_bytes());
                return IpcEndpoint::NamedPipe(format!(r"\\.\pipe\lsp-share-{hash:016x}"));
            }
        }
        let user = std::env::var("USERNAME")
            .or_else(|_| std::env::var("USER"))
            .unwrap_or_else(|_| "user".to_string());
        let sanitized: String = user
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let sanitized = if sanitized.is_empty() {
            "user".to_string()
        } else {
            sanitized
        };
        IpcEndpoint::NamedPipe(format!(r"\\.\pipe\lsp-share-{sanitized}"))
    }
}

pub fn ipc_endpoint_display() -> String {
    match ipc_endpoint() {
        #[cfg(unix)]
        IpcEndpoint::UnixSocket(p) => p.display().to_string(),
        #[cfg(windows)]
        IpcEndpoint::NamedPipe(p) => p,
    }
}

pub fn lock_file_path() -> PathBuf {
    if let Ok(dir) = std::env::var("LSP_SHARE_RUNTIME_DIR") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir).join("daemon.lock");
        }
    }

    #[cfg(unix)]
    {
        match ipc_endpoint() {
            IpcEndpoint::UnixSocket(sock) => sock
                .parent()
                .map(|p| p.join("daemon.lock"))
                .unwrap_or_else(|| runtime_dir().join("daemon.lock")),
        }
    }

    #[cfg(windows)]
    {
        if let Ok(sock) = std::env::var("LSP_SHARE_SOCKET") {
            if !sock.trim().is_empty() {
                let hash = fnv1a_64(sock.trim().as_bytes());
                return runtime_dir().join(format!("daemon-{hash:016x}.lock"));
            }
        }
        runtime_dir().join("daemon.lock")
    }
}

pub fn ensure_secure_dir(dir: &Path) -> Result<()> {
    if !dir.exists() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create directory {}", dir.display()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o700);
        let _ = std::fs::set_permissions(dir, perms);
    }
    Ok(())
}

pub fn config_file_path(override_path: Option<&Path>) -> PathBuf {
    if let Some(p) = override_path {
        return p.to_path_buf();
    }
    if let Ok(env_path) = std::env::var("LSP_SHARE_CONFIG") {
        if !env_path.trim().is_empty() {
            return PathBuf::from(env_path);
        }
    }

    #[cfg(windows)]
    {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            return PathBuf::from(appdata).join("lsp-share").join("config.toml");
        }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
            if !xdg.trim().is_empty() {
                return PathBuf::from(xdg).join("lsp-share").join("config.toml");
            }
        }
    }

    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("lsp-share")
        .join("config.toml")
}

pub fn daemon_log_path() -> PathBuf {
    if let Ok(env_path) = std::env::var("LSP_SHARE_LOG_FILE") {
        if !env_path.trim().is_empty() {
            return PathBuf::from(env_path);
        }
    }

    #[cfg(target_os = "macos")]
    {
        home_dir()
            .unwrap_or_else(runtime_dir)
            .join("Library")
            .join("Logs")
            .join("lsp-share")
            .join("daemon.log")
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Ok(xdg) = std::env::var("XDG_STATE_HOME") {
            if !xdg.trim().is_empty() {
                return PathBuf::from(xdg).join("lsp-share").join("daemon.log");
            }
        }
        home_dir()
            .unwrap_or_else(runtime_dir)
            .join(".local")
            .join("state")
            .join("lsp-share")
            .join("daemon.log")
    }

    #[cfg(windows)]
    {
        runtime_dir().join("daemon.log")
    }
}

pub fn shim_bin_dir() -> PathBuf {
    if let Ok(env_path) = std::env::var("LSP_SHARE_SHIM_DIR") {
        if !env_path.trim().is_empty() {
            return PathBuf::from(env_path);
        }
    }
    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".lsp-share")
        .join("bin")
}

pub fn extract_server_name(cmd: &str) -> String {
    let file_name = cmd.rsplit(['/', '\\']).next().unwrap_or(cmd);
    let lower = file_name.to_ascii_lowercase();
    for ext in [".exe", ".cmd", ".bat", ".com"] {
        if let Some(stripped) = lower.strip_suffix(ext) {
            return stripped.to_string();
        }
    }
    lower
}

fn is_self_executable(candidate: &Path, self_canon: Option<&Path>) -> bool {
    let Some(self_path) = self_canon else {
        return false;
    };
    match std::fs::canonicalize(candidate) {
        Ok(cand_canon) => cand_canon == self_path,
        Err(_) => false,
    }
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(windows)]
    {
        true
    }
}

fn candidate_variants(base: &Path) -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        if base.extension().is_some() {
            return vec![base.to_path_buf()];
        }
        let pathext =
            std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
        let mut out = Vec::new();
        for ext in pathext.split(';').filter(|s| !s.is_empty()) {
            let ext_clean = ext.trim().trim_start_matches('.');
            out.push(base.with_extension(ext_clean));
        }
        out.push(base.to_path_buf());
        out
    }
    #[cfg(not(windows))]
    {
        vec![base.to_path_buf()]
    }
}

/// Canonicalizes `path` while stripping Windows `\\?\` verbatim prefixes that break
/// legacy Win32 tools when passed via argv[0] or Command::current_dir.
pub fn clean_canonicalize(path: &Path) -> PathBuf {
    let Ok(canon) = std::fs::canonicalize(path) else {
        return path.to_path_buf();
    };
    #[cfg(windows)]
    {
        let s = canon.to_string_lossy();
        if let Some(unc) = s.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{unc}"));
        }
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
        canon
    }
    #[cfg(not(windows))]
    {
        canon
    }
}

pub fn to_native_os_path(norm_path: &str) -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(norm_path.replace('/', "\\"))
    }
    #[cfg(not(windows))]
    {
        PathBuf::from(norm_path)
    }
}

/// Resolves an executable path without invoking any external crate, skipping `exclude_dir`
/// and any symlink/hardlink pointing to the current `lsp-share` binary to prevent recursion.
pub fn resolve_executable(
    cmd: &str,
    path_env: Option<&str>,
    exclude_dir: Option<&Path>,
) -> Result<PathBuf> {
    let self_canon = std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::canonicalize(p).ok());
    let exclude_canon = exclude_dir.and_then(|d| std::fs::canonicalize(d).ok());

    let cmd_path = Path::new(cmd);
    let has_sep = cmd.contains('/') || cmd.contains('\\');
    if has_sep || cmd_path.is_absolute() {
        for cand in candidate_variants(cmd_path) {
            if is_executable_file(&cand) && !is_self_executable(&cand, self_canon.as_deref()) {
                return Ok(clean_canonicalize(&cand));
            }
        }
        return Err(anyhow!(
            "executable not found or points to lsp-share: {cmd}"
        ));
    }

    let search_path = path_env
        .map(std::ffi::OsString::from)
        .or_else(|| std::env::var_os("PATH"))
        .unwrap_or_default();

    for dir in std::env::split_paths(&search_path) {
        if let Some(excl) = exclude_dir {
            if dir == excl {
                continue;
            }
        }
        if let Some(excl_c) = &exclude_canon {
            if let Ok(dir_c) = std::fs::canonicalize(&dir) {
                if &dir_c == excl_c {
                    continue;
                }
            }
        }

        let base = dir.join(cmd);
        for cand in candidate_variants(&base) {
            if is_executable_file(&cand) && !is_self_executable(&cand, self_canon.as_deref()) {
                return Ok(clean_canonicalize(&cand));
            }
        }
    }

    Err(anyhow!("executable '{cmd}' not found in PATH"))
}

pub fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn strip_windows_verbatim(s: &str) -> &str {
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return rest;
    }
    s.strip_prefix(r"\\?\").unwrap_or(s)
}

fn normalize_drive_and_slashes(raw: &str) -> String {
    let mut s = raw.replace('\\', "/");
    // Handle file:///c:/... -> /c:/... -> C:/...
    if s.len() >= 3
        && s.as_bytes()[0] == b'/'
        && s.as_bytes()[1].is_ascii_alphabetic()
        && s.as_bytes()[2] == b':'
    {
        s.remove(0);
    }
    if s.len() >= 2 && s.as_bytes()[0].is_ascii_alphabetic() && s.as_bytes()[1] == b':' {
        let upper = (s.as_bytes()[0] as char).to_ascii_uppercase();
        s.replace_range(0..1, &upper.to_string());
    }
    while s.len() > 1 && s.ends_with('/') {
        // Keep "C:/" intact
        if s.len() == 3 && s.as_bytes()[1] == b':' && s.as_bytes()[2] == b'/' {
            break;
        }
        s.pop();
    }
    s
}

/// Normalizes a `file://` URI or filesystem path into a canonical comparison string.
pub fn normalize_uri_or_path(raw: &str) -> String {
    let decoded = if let Some(rest) = raw.strip_prefix("file://") {
        // Strip authority if localhost, keep UNC host if present
        let path_part = if let Some(after_localhost) = rest.strip_prefix("localhost/") {
            format!("/{after_localhost}")
        } else {
            rest.to_string()
        };
        percent_decode(&path_part)
    } else {
        raw.to_string()
    };

    let pre_norm = normalize_drive_and_slashes(&decoded);
    let candidate = Path::new(&pre_norm);
    if candidate.exists() {
        if let Ok(canon) = std::fs::canonicalize(candidate) {
            let canon_str = canon.to_string_lossy();
            let stripped = strip_windows_verbatim(&canon_str);
            return normalize_drive_and_slashes(stripped);
        }
    }
    pre_norm
}

pub fn normalize_workspace_root(
    root_override: Option<&str>,
    init_params: Option<&Value>,
    cwd: &str,
) -> String {
    if let Some(ov) = root_override.filter(|s| !s.trim().is_empty()) {
        return normalize_uri_or_path(ov);
    }

    if let Some(params) = init_params {
        if let Some(uri) = params
            .get("workspaceFolders")
            .and_then(|wf| wf.as_array())
            .and_then(|arr| arr.first())
            .and_then(|item| item.get("uri"))
            .and_then(|v| v.as_str())
        {
            return normalize_uri_or_path(uri);
        }
        if let Some(uri) = params.get("rootUri").and_then(|v| v.as_str()) {
            return normalize_uri_or_path(uri);
        }
        if let Some(path) = params.get("rootPath").and_then(|v| v.as_str()) {
            return normalize_uri_or_path(path);
        }
    }

    normalize_uri_or_path(cwd)
}

pub fn capture_relevant_env() -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for (key, val) in std::env::vars() {
        if !key.starts_with("LSP_SHARE_") {
            map.insert(key, val);
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extract_server_name_strips_extensions() {
        assert_eq!(extract_server_name("/opt/llvm-19/bin/clangd"), "clangd");
        assert_eq!(extract_server_name("clangd.exe"), "clangd");
        assert_eq!(
            extract_server_name(r"C:\Tools\Pyright\pyright-langserver.CMD"),
            "pyright-langserver"
        );
    }

    #[test]
    fn normalize_uri_handles_percent_encoding_and_drive_letter() {
        assert_eq!(
            normalize_uri_or_path("file:///c%3A/Users/Dev/My%20Repo/"),
            "C:/Users/Dev/My Repo"
        );
        assert_eq!(
            normalize_uri_or_path("file:///Users/dev/project/"),
            "/Users/dev/project"
        );
    }

    #[test]
    fn workspace_root_priority_order() {
        let params = json!({
            "workspaceFolders": [{ "uri": "file:///tmp/ws-folder/", "name": "ws" }],
            "rootUri": "file:///tmp/root-uri/",
            "rootPath": "/tmp/root-path"
        });
        assert_eq!(
            normalize_workspace_root(Some("/tmp/override/"), Some(&params), "/tmp/cwd"),
            "/tmp/override"
        );
        assert_eq!(
            normalize_workspace_root(None, Some(&params), "/tmp/cwd"),
            "/tmp/ws-folder"
        );
        let params_no_wf = json!({
            "rootUri": "file:///tmp/root-uri/",
            "rootPath": "/tmp/root-path"
        });
        assert_eq!(
            normalize_workspace_root(None, Some(&params_no_wf), "/tmp/cwd"),
            "/tmp/root-uri"
        );
    }
}
