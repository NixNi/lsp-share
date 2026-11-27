use crate::config::{ClientRole, Config, ShimConfig};
use crate::paths;
use crate::proxy::{self, ProxyOptions};
use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};

fn shim_filename(name: &str) -> String {
    let clean = paths::extract_server_name(name);
    #[cfg(windows)]
    {
        format!("{clean}.exe")
    }
    #[cfg(not(windows))]
    {
        clean
    }
}

fn ensure_windows_exe_ext(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        if path.extension().is_none() {
            return path.with_extension("exe");
        }
        path.to_path_buf()
    }
    #[cfg(not(windows))]
    {
        path.to_path_buf()
    }
}

fn resolve_shim_destination(
    name_or_path: &str,
    output: Option<&Path>,
) -> Result<(String, PathBuf, bool)> {
    let lookup_name = paths::extract_server_name(name_or_path);
    if lookup_name == "lsp-share" || lookup_name.is_empty() {
        return Err(anyhow!("invalid shim name: '{name_or_path}'"));
    }

    if let Some(out) = output {
        let out_str = out.to_string_lossy();
        let is_dir = out.is_dir() || out_str.ends_with('/') || out_str.ends_with('\\');
        let raw_path = if is_dir {
            out.join(shim_filename(&lookup_name))
        } else {
            ensure_windows_exe_ext(out)
        };
        return Ok((lookup_name, raw_path, false));
    }

    if name_or_path.contains('/') || name_or_path.contains('\\') {
        let raw_path = ensure_windows_exe_ext(Path::new(name_or_path));
        return Ok((lookup_name, raw_path, false));
    }

    let default_path = paths::shim_bin_dir().join(shim_filename(&lookup_name));
    Ok((lookup_name, default_path, true))
}

fn absolutize_shim_path(path: &Path) -> Result<PathBuf> {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("failed to get current directory")?
            .join(path)
    };
    if let (Some(parent), Some(file_name)) = (abs.parent(), abs.file_name()) {
        if parent.exists() {
            return Ok(paths::clean_canonicalize(parent).join(file_name));
        }
    }
    Ok(abs)
}

fn create_shim_link(src_exe: &Path, dst_shim: &Path) -> Result<()> {
    if dst_shim.exists() || dst_shim.symlink_metadata().is_ok() {
        std::fs::remove_file(dst_shim)
            .with_context(|| format!("failed to remove existing shim {}", dst_shim.display()))?;
    }

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(src_exe, dst_shim).with_context(|| {
            format!(
                "failed to symlink {} -> {}",
                dst_shim.display(),
                src_exe.display()
            )
        })?;
        Ok(())
    }

    #[cfg(windows)]
    {
        if std::os::windows::fs::symlink_file(src_exe, dst_shim).is_ok() {
            return Ok(());
        }
        if std::fs::hard_link(src_exe, dst_shim).is_ok() {
            return Ok(());
        }
        std::fs::copy(src_exe, dst_shim).with_context(|| {
            format!(
                "failed to copy {} to {}",
                src_exe.display(),
                dst_shim.display()
            )
        })?;
        Ok(())
    }
}

pub fn add_shim(
    name: &str,
    output: Option<PathBuf>,
    target: Option<PathBuf>,
    role: Option<ClientRole>,
    key: Option<String>,
    config_override: Option<&Path>,
) -> Result<()> {
    let (lookup_name, raw_shim_path, is_default_dir) =
        resolve_shim_destination(name, output.as_deref())?;

    if let Some(parent) = raw_shim_path.parent() {
        if !parent.as_os_str().is_empty() {
            if is_default_dir {
                paths::ensure_secure_dir(parent)?;
            } else {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("failed to create shim directory {}", parent.display())
                })?;
            }
        }
    }

    let shim_path = absolutize_shim_path(&raw_shim_path)?;
    let shim_key_name = paths::extract_server_name(&shim_path.to_string_lossy());
    if shim_key_name == "lsp-share" || shim_key_name.is_empty() {
        return Err(anyhow!("invalid shim filename: {}", shim_path.display()));
    }

    let default_shim_dir = paths::shim_bin_dir();
    let resolved_target = match target {
        Some(t) => paths::resolve_executable(&t.to_string_lossy(), None, Some(&default_shim_dir))
            .unwrap_or_else(|_| paths::clean_canonicalize(&t)),
        None => paths::resolve_executable(&lookup_name, None, Some(&default_shim_dir))
            .with_context(|| {
                format!("could not find real '{lookup_name}' in PATH; pass --target <PATH>")
            })?,
    };

    if paths::normalize_uri_or_path(&shim_path.to_string_lossy())
        == paths::normalize_uri_or_path(&resolved_target.to_string_lossy())
    {
        return Err(anyhow!(
            "refusing to overwrite real target binary '{}' with a shim; choose a different output path or pass --target <REAL_BINARY>",
            resolved_target.display()
        ));
    }

    let exe_raw = std::env::current_exe().context("failed to locate lsp-share binary")?;
    let current_exe = paths::clean_canonicalize(&exe_raw);
    create_shim_link(&current_exe, &shim_path)?;

    let mut cfg = Config::load(config_override)?;
    cfg.shims.insert(
        shim_key_name,
        ShimConfig {
            path: Some(shim_path.clone()),
            target: resolved_target.clone(),
            role,
            key,
        },
    );
    let saved_cfg = cfg.save(config_override)?;

    println!(
        "Added shim: {} -> {} (config: {})",
        shim_path.display(),
        resolved_target.display(),
        saved_cfg.display()
    );
    Ok(())
}

pub fn list_shims(config_override: Option<&Path>) -> Result<()> {
    let cfg = Config::load(config_override)?;
    let shim_dir = paths::shim_bin_dir();
    println!("Default shim directory: {}", shim_dir.display());
    if cfg.shims.is_empty() {
        println!("No shims registered.");
        return Ok(());
    }
    for (name, sc) in &cfg.shims {
        let shim_path = sc
            .path
            .clone()
            .unwrap_or_else(|| shim_dir.join(shim_filename(name)));
        let status = if shim_path.exists() {
            "present"
        } else {
            "missing-link"
        };
        println!(
            "{} ({}) -> {} [{}]{}{}",
            name,
            shim_path.display(),
            sc.target.display(),
            status,
            sc.role.map(|r| format!(" role={r:?}")).unwrap_or_default(),
            sc.key
                .as_deref()
                .map(|k| format!(" key={k}"))
                .unwrap_or_default()
        );
    }
    Ok(())
}

pub fn remove_shim(name_or_path: &str, config_override: Option<&Path>) -> Result<()> {
    let mut cfg = Config::load(config_override)?;
    let input_norm = paths::normalize_uri_or_path(name_or_path);
    let candidate_key = paths::extract_server_name(name_or_path);

    let matched_key = if cfg.shims.contains_key(&candidate_key) {
        Some(candidate_key.clone())
    } else {
        cfg.shims
            .iter()
            .find(|(_, sc)| {
                sc.path.as_ref().is_some_and(|p| {
                    paths::normalize_uri_or_path(&p.to_string_lossy()) == input_norm
                })
            })
            .map(|(k, _)| k.clone())
    };

    let key = matched_key.unwrap_or(candidate_key);
    let shim_path = cfg
        .shims
        .get(&key)
        .and_then(|sc| sc.path.clone())
        .unwrap_or_else(|| {
            if name_or_path.contains('/') || name_or_path.contains('\\') {
                ensure_windows_exe_ext(Path::new(name_or_path))
            } else {
                paths::shim_bin_dir().join(shim_filename(&key))
            }
        });

    if shim_path.exists() || shim_path.symlink_metadata().is_ok() {
        std::fs::remove_file(&shim_path)
            .with_context(|| format!("failed to remove {}", shim_path.display()))?;
    }
    cfg.shims.remove(&key);
    cfg.save(config_override)?;
    println!("Removed shim: {} ({})", key, shim_path.display());
    Ok(())
}

pub async fn run_as_shim(invoked_name: &str, args: Vec<String>) -> Result<()> {
    let clean_name = paths::extract_server_name(invoked_name);
    let cfg = Config::load(None).unwrap_or_default();
    let shim_dir = paths::shim_bin_dir();

    let (command, role, key) = if let Some(sc) = cfg.shims.get(&clean_name) {
        (
            sc.target.to_string_lossy().into_owned(),
            sc.role.unwrap_or(ClientRole::Auto),
            sc.key.clone(),
        )
    } else {
        let resolved = paths::resolve_executable(&clean_name, None, Some(&shim_dir))?;
        (
            resolved.to_string_lossy().into_owned(),
            ClientRole::Auto,
            None,
        )
    };

    proxy::run_proxy(ProxyOptions {
        role,
        match_by: None,
        key,
        root: None,
        trace: None,
        config: None,
        command,
        args,
    })
    .await
}
