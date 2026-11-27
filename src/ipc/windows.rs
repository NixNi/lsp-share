use crate::paths::{self, IpcEndpoint};
use anyhow::{anyhow, Context, Result};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf, ReadHalf, WriteHalf};
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::Storage::FileSystem::{LockFileEx, LOCKFILE_EXCLUSIVE_LOCK};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, TerminateProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
};
use windows_sys::Win32::System::IO::OVERLAPPED;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
const ERROR_PIPE_BUSY: i32 = 231;

pub enum PipeStream {
    Server(NamedPipeServer),
    Client(NamedPipeClient),
}

impl AsyncRead for PipeStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match &mut *self {
            PipeStream::Server(s) => Pin::new(s).poll_read(cx, buf),
            PipeStream::Client(c) => Pin::new(c).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for PipeStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match &mut *self {
            PipeStream::Server(s) => Pin::new(s).poll_write(cx, buf),
            PipeStream::Client(c) => Pin::new(c).poll_write(cx, buf),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        match &mut *self {
            PipeStream::Server(s) => Pin::new(s).poll_flush(cx),
            PipeStream::Client(c) => Pin::new(c).poll_flush(cx),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        match &mut *self {
            PipeStream::Server(s) => Pin::new(s).poll_shutdown(cx),
            PipeStream::Client(c) => Pin::new(c).poll_shutdown(cx),
        }
    }
}

pub type IpcReadHalf = ReadHalf<PipeStream>;
pub type IpcWriteHalf = WriteHalf<PipeStream>;

pub struct LocalListener {
    pipe_name: String,
    next_server: tokio::sync::Mutex<NamedPipeServer>,
}

impl LocalListener {
    pub fn bind(endpoint: &IpcEndpoint) -> Result<Self> {
        let IpcEndpoint::NamedPipe(pipe_name) = endpoint;
        let first = ServerOptions::new()
            .first_pipe_instance(true)
            .create(pipe_name)
            .with_context(|| format!("failed to create named pipe {pipe_name}"))?;
        Ok(Self {
            pipe_name: pipe_name.clone(),
            next_server: tokio::sync::Mutex::new(first),
        })
    }

    pub async fn accept(&self) -> Result<(IpcReadHalf, IpcWriteHalf)> {
        let mut guard = self.next_server.lock().await;
        guard.connect().await?;
        let replacement = ServerOptions::new()
            .create(&self.pipe_name)
            .with_context(|| format!("failed to create replacement pipe {}", self.pipe_name))?;
        let connected = std::mem::replace(&mut *guard, replacement);
        drop(guard);
        Ok(tokio::io::split(PipeStream::Server(connected)))
    }
}

pub async fn try_connect(endpoint: &IpcEndpoint) -> Result<(IpcReadHalf, IpcWriteHalf)> {
    let IpcEndpoint::NamedPipe(pipe_name) = endpoint;
    for attempt in 0..10 {
        match ClientOptions::new().open(pipe_name) {
            Ok(client) => return Ok(tokio::io::split(PipeStream::Client(client))),
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && attempt < 9 => {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            Err(e) => {
                return Err(e).with_context(|| format!("failed to open named pipe {pipe_name}"));
            }
        }
    }
    Err(anyhow!("named pipe busy: {pipe_name}"))
}

pub struct FileLock {
    _file: File,
}

impl FileLock {
    pub async fn acquire(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            paths::ensure_secure_dir(parent)?;
        }
        let path_owned = path.to_path_buf();
        let file = tokio::task::spawn_blocking(move || -> Result<File> {
            let file = OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(&path_owned)
                .with_context(|| format!("failed to open lockfile {}", path_owned.display()))?;
            let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
            let ok = unsafe {
                LockFileEx(
                    file.as_raw_handle() as HANDLE,
                    LOCKFILE_EXCLUSIVE_LOCK,
                    0,
                    1,
                    0,
                    &mut overlapped,
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error()).context("LockFileEx failed");
            }
            Ok(file)
        })
        .await
        .context("lock task panicked")??;
        Ok(Self { _file: file })
    }
}

pub fn configure_detached_daemon_command(cmd: &mut std::process::Command) {
    cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
}

struct JobHandle(HANDLE);

unsafe impl Send for JobHandle {}
unsafe impl Sync for JobHandle {}

impl Drop for JobHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

#[derive(Clone)]
pub struct ChildTracker {
    job: Arc<JobHandle>,
}

impl ChildTracker {
    pub fn new() -> Result<Self> {
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            return Err(io::Error::last_os_error()).context("CreateJobObjectW failed");
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            unsafe { CloseHandle(job) };
            return Err(io::Error::last_os_error()).context("SetInformationJobObject failed");
        }
        Ok(Self {
            job: Arc::new(JobHandle(job)),
        })
    }

    pub fn track_pid(&self, pid: u32) -> Result<()> {
        let proc_handle = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid) };
        if proc_handle.is_null() {
            return Err(io::Error::last_os_error()).context("OpenProcess failed");
        }
        let ok = unsafe { AssignProcessToJobObject(self.job.0, proc_handle) };
        unsafe { CloseHandle(proc_handle) };
        if ok == 0 {
            return Err(io::Error::last_os_error()).context("AssignProcessToJobObject failed");
        }
        Ok(())
    }
}

pub fn build_lsp_command(program: &Path, args: &[String]) -> tokio::process::Command {
    let is_batch = program
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat"))
        .unwrap_or(false);

    let mut cmd = if is_batch {
        let mut c = tokio::process::Command::new("cmd.exe");
        c.arg("/d").arg("/s").arg("/c").arg(program);
        c.args(args);
        c
    } else {
        let mut c = tokio::process::Command::new(program);
        c.args(args);
        c
    };
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}

pub fn terminate_child_process(pid: u32, _force: bool) {
    let proc_handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
    if !proc_handle.is_null() {
        unsafe {
            TerminateProcess(proc_handle, 1);
            CloseHandle(proc_handle);
        }
    }
}
