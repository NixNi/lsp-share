use crate::paths;
use anyhow::{Context, Result};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum, Default)]
#[serde(rename_all = "snake_case")]
pub enum ClientRole {
    #[default]
    Auto,
    Editor,
    Agent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum, Default)]
#[serde(rename_all = "snake_case")]
pub enum BinaryMatching {
    #[default]
    Name,
    CanonicalPath,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum, Default)]
#[serde(rename_all = "snake_case")]
pub enum ArgMatching {
    #[default]
    Ignore,
    Semantic,
    Exact,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum, Default)]
#[serde(rename_all = "snake_case")]
pub enum OpenConflictPolicy {
    #[default]
    EditorWins,
    LastWriterWins,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DaemonConfig {
    pub idle_timeout_secs: u64,
    pub log_level: String,
    pub trace_file: Option<PathBuf>,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            idle_timeout_secs: 30,
            log_level: "info".to_string(),
            trace_file: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct MatchingConfig {
    pub binary_matching: BinaryMatching,
    pub arg_matching: ArgMatching,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DocumentsConfig {
    pub open_conflict_policy: OpenConflictPolicy,
    pub event_dedup_window_ms: u64,
}

impl Default for DocumentsConfig {
    fn default() -> Self {
        Self {
            open_conflict_policy: OpenConflictPolicy::EditorWins,
            event_dedup_window_ms: 50,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ServerConfig {
    pub binary: Option<PathBuf>,
    pub binary_matching: Option<BinaryMatching>,
    pub arg_matching: Option<ArgMatching>,
    pub semantic_args: Vec<String>,
    pub default_args: Vec<String>,
    pub env_key_vars: Vec<String>,
    pub force_utf16: Option<bool>,
    pub idle_timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShimConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    pub target: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<ClientRole>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub daemon: DaemonConfig,
    pub matching: MatchingConfig,
    pub documents: DocumentsConfig,
    pub servers: BTreeMap<String, ServerConfig>,
    pub shims: BTreeMap<String, ShimConfig>,
}

#[derive(Debug, Clone)]
pub struct EffectiveServerSettings {
    pub preferred_binary: Option<PathBuf>,
    pub binary_matching: BinaryMatching,
    pub arg_matching: ArgMatching,
    pub semantic_args: Vec<String>,
    pub default_args: Vec<String>,
    pub env_key_vars: Vec<String>,
    pub force_utf16: bool,
    pub idle_timeout_secs: u64,
}

impl Config {
    pub fn load(override_path: Option<&Path>) -> Result<Self> {
        let path = paths::config_file_path(override_path);
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read config file {}", path.display()))?;
        let cfg: Config = toml::from_str(&raw)
            .with_context(|| format!("failed to parse TOML config {}", path.display()))?;
        Ok(cfg)
    }

    pub fn save(&self, override_path: Option<&Path>) -> Result<PathBuf> {
        let path = paths::config_file_path(override_path);
        if let Some(parent) = path.parent() {
            paths::ensure_secure_dir(parent)?;
        }
        let rendered = toml::to_string_pretty(self).context("failed to serialize config")?;
        std::fs::write(&path, rendered)
            .with_context(|| format!("failed to write config file {}", path.display()))?;
        Ok(path)
    }

    pub fn effective_server(&self, server_name: &str) -> EffectiveServerSettings {
        let builtin = builtin_server_defaults(server_name);
        let user = self.servers.get(server_name);

        let binary_matching = user
            .and_then(|u| u.binary_matching)
            .unwrap_or(self.matching.binary_matching);
        let arg_matching = user
            .and_then(|u| u.arg_matching)
            .unwrap_or(self.matching.arg_matching);

        let semantic_args = match user {
            Some(u) if !u.semantic_args.is_empty() => u.semantic_args.clone(),
            _ => builtin.semantic_args,
        };
        let default_args = match user {
            Some(u) if !u.default_args.is_empty() => u.default_args.clone(),
            _ => builtin.default_args,
        };
        let env_key_vars = match user {
            Some(u) if !u.env_key_vars.is_empty() => u.env_key_vars.clone(),
            _ => builtin.env_key_vars,
        };
        let force_utf16 = user
            .and_then(|u| u.force_utf16)
            .or(builtin.force_utf16)
            .unwrap_or(false);
        let idle_timeout_secs = user
            .and_then(|u| u.idle_timeout_secs)
            .unwrap_or(self.daemon.idle_timeout_secs);

        EffectiveServerSettings {
            preferred_binary: user.and_then(|u| u.binary.clone()),
            binary_matching,
            arg_matching,
            semantic_args,
            default_args,
            env_key_vars,
            force_utf16,
            idle_timeout_secs,
        }
    }
}

fn builtin_server_defaults(server_name: &str) -> ServerConfig {
    match server_name {
        "clangd" => ServerConfig {
            semantic_args: vec![
                "--compile-commands-dir".to_string(),
                "--query-driver".to_string(),
            ],
            force_utf16: Some(true),
            ..Default::default()
        },
        "pyright-langserver" | "pyright" => ServerConfig {
            default_args: vec!["--stdio".to_string()],
            env_key_vars: vec!["VIRTUAL_ENV".to_string(), "CONDA_PREFIX".to_string()],
            ..Default::default()
        },
        _ => ServerConfig::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_use_aggressive_name_matching_and_editor_wins() {
        let cfg = Config::default();
        assert_eq!(cfg.daemon.idle_timeout_secs, 30);
        assert_eq!(cfg.matching.binary_matching, BinaryMatching::Name);
        assert_eq!(cfg.matching.arg_matching, ArgMatching::Ignore);
        assert_eq!(
            cfg.documents.open_conflict_policy,
            OpenConflictPolicy::EditorWins
        );

        let clangd = cfg.effective_server("clangd");
        assert!(clangd.force_utf16);
        assert_eq!(
            clangd.semantic_args,
            vec!["--compile-commands-dir", "--query-driver"]
        );

        let pyright = cfg.effective_server("pyright-langserver");
        assert_eq!(pyright.default_args, vec!["--stdio"]);
        assert_eq!(pyright.env_key_vars, vec!["VIRTUAL_ENV", "CONDA_PREFIX"]);
    }
}
