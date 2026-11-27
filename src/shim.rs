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
    target: Option<PathBuf>,
    role: Option<ClientRole>,
    key: Option<String>,
    config_override: Option<&Path>,
) -> Result<()> {
    if name.contains('/') || name.contains('\\') {
        return Err(anyhow!("shim name must not contain path separators"));
    }
    let clean_name = paths::extract_server_name(name);
    if clean_name == "lsp-share" || clean_name.is_empty() {
        return Err(anyhow!("invalid shim name: '{name}'"));
    }

    let shim_dir = paths::shim_bin_dir();
    paths::ensure_secure_dir(&shim_dir)?;

    let resolved_target = match target {
        Some(t) => {
            paths::resolve_executable(&t.to_string_lossy(), None, Some(&shim_dir)).unwrap_or(t)
        }
        None => paths::resolve_executable(name, None, Some(&shim_dir)).with_context(|| {
            format!("could not find real '{name}' in PATH; pass --target <PATH>")
        })?,
    };

    let exe_raw = std::env::current_exe().context("failed to locate lsp-share binary")?;
    let current_exe = std::fs::canonicalize(&exe_raw).unwrap_or(exe_raw);
    let shim_path = shim_dir.join(shim_filename(&clean_name));
    create_shim_link(&current_exe, &shim_path)?;

    let mut cfg = Config::load(config_override)?;
    cfg.shims.insert(
        clean_name.clone(),
        ShimConfig {
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
    println!("Shim directory: {}", shim_dir.display());
    if cfg.shims.is_empty() {
        println!("No shims registered.");
        return Ok(());
    }
    for (name, sc) in &cfg.shims {
        let shim_path = shim_dir.join(shim_filename(name));
        let status = if shim_path.exists() {
            "present"
        } else {
            "missing-link"
        };
        println!(
            "{} -> {} [{}]{}{}",
            name,
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

pub fn remove_shim(name: &str, config_override: Option<&Path>) -> Result<()> {
    let clean_name = paths::extract_server_name(name);
    let shim_path = paths::shim_bin_dir().join(shim_filename(&clean_name));
    if shim_path.exists() || shim_path.symlink_metadata().is_ok() {
        std::fs::remove_file(&shim_path)
            .with_context(|| format!("failed to remove {}", shim_path.display()))?;
    }
    let mut cfg = Config::load(config_override)?;
    cfg.shims.remove(&clean_name);
    cfg.save(config_override)?;
    println!("Removed shim: {clean_name}");
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
