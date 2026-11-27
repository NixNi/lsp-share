use anyhow::{Context, Result};
use serde::Serialize;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Default)]
pub struct TraceLogger {
    inner: Option<Arc<Mutex<BufWriter<File>>>>,
}

#[derive(Serialize)]
struct TraceRecord<'a> {
    ts_ms: u128,
    instance_id: &'a str,
    client_id: Option<u64>,
    dir: &'a str,
    action: &'a str,
    method: Option<&'a str>,
    bytes: usize,
}

impl TraceLogger {
    pub fn open(path: Option<&Path>) -> Result<Self> {
        let Some(p) = path else {
            return Ok(Self { inner: None });
        };
        if let Some(parent) = p.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).ok();
            }
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
            .with_context(|| format!("failed to open trace file {}", p.display()))?;
        Ok(Self {
            inner: Some(Arc::new(Mutex::new(BufWriter::new(file)))),
        })
    }

    pub fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    pub fn log(
        &self,
        instance_id: &str,
        client_id: Option<u64>,
        dir: &str,
        action: &str,
        method: Option<&str>,
        bytes: usize,
    ) {
        let Some(writer) = &self.inner else {
            return;
        };
        let ts_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let rec = TraceRecord {
            ts_ms,
            instance_id,
            client_id,
            dir,
            action,
            method,
            bytes,
        };
        if let Ok(mut guard) = writer.lock() {
            if serde_json::to_writer(&mut *guard, &rec).is_ok() {
                let _ = guard.write_all(b"\n");
                let _ = guard.flush();
            }
        }
    }
}
