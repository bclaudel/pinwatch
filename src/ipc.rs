use crate::model::*;
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{Semaphore, watch},
};

pub const MAX_FRAME_BYTES: usize = 1024 * 1024;
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub schema_version: u32,
    pub op: String,
}
#[derive(Clone)]
pub struct Cached {
    pub frame: Frame,
    pub at: Instant,
}
impl Cached {
    pub fn fresh_frame(&self) -> Frame {
        let mut frame = self.frame.clone();
        frame.refresh(self.at.elapsed().as_millis() as u64);
        frame
    }
}
/// Bounded line reader: neither a missing newline nor a hostile peer can grow memory indefinitely.
pub async fn line<R: AsyncBufRead + Unpin>(reader: &mut R, max: usize) -> Result<Option<Vec<u8>>> {
    let mut result = vec![];
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            ensure!(result.is_empty(), "truncated JSON line");
            return Ok(None);
        }
        let count = available
            .iter()
            .position(|b| *b == b'\n')
            .map_or(available.len(), |p| p + 1);
        ensure!(result.len() + count <= max, "JSON line exceeds {max} bytes");
        let done = available[count - 1] == b'\n';
        result.extend_from_slice(&available[..count]);
        reader.consume(count);
        if done {
            return Ok(Some(result));
        }
    }
}
pub fn check_directory(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path).with_context(|| {
        format!(
            "runtime directory {} must exist (see setup)",
            path.display()
        )
    })?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "runtime directory must be a real directory"
    );
    ensure!(
        metadata.mode() & 0o022 == 0,
        "runtime directory must not be group/world writable"
    );
    Ok(())
}
pub struct Ownership {
    _file: File,
}
impl Ownership {
    pub fn acquire(directory: &Path, name: &str) -> Result<Self> {
        check_directory(directory)?;
        ensure!(
            name.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "invalid lock identity"
        );
        let path = directory.join(format!("{name}.lock"));
        let file = match OpenOptions::new()
            .write(true)
            .read(true)
            .create_new(true)
            .mode(0o644)
            .open(&path)
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let m = std::fs::symlink_metadata(&path)?;
                ensure!(
                    m.is_file() && !m.file_type().is_symlink() && m.mode() & 0o022 == 0,
                    "unsafe ownership lock"
                );
                File::open(&path)?
            }
            Err(e) => return Err(e.into()),
        };
        file.try_lock_exclusive()
            .context("another Pinwatch instance owns this device/socket")?;
        // Never unlink locks: doing so could split ownership across two inodes.
        Ok(Self { _file: file })
    }
}
pub struct SocketGuard {
    path: PathBuf,
    inode: u64,
    _lock: Ownership,
}
impl SocketGuard {
    pub fn bind(path: &Path) -> Result<(UnixListener, Self)> {
        let parent = path.parent().context("socket has no parent")?;
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .context("invalid socket name")?
            .replace('.', "_");
        let lock = Ownership::acquire(parent, &format!("socket-{name}"))?;
        if let Ok(m) = std::fs::symlink_metadata(path) {
            ensure!(
                m.file_type().is_socket(),
                "refusing to replace non-socket path"
            );
            ensure!(
                std::os::unix::net::UnixStream::connect(path).is_err(),
                "socket already accepting connections"
            );
            std::fs::remove_file(path)?;
        }
        let listener = UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))?;
        let inode = std::fs::symlink_metadata(path)?.ino();
        Ok((
            listener,
            Self {
                path: path.into(),
                inode,
                _lock: lock,
            },
        ))
    }
}
impl Drop for SocketGuard {
    fn drop(&mut self) {
        if std::fs::symlink_metadata(&self.path)
            .is_ok_and(|m| m.file_type().is_socket() && m.ino() == self.inode)
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}
async fn send(stream: &mut UnixStream, frame: &Frame) -> Result<()> {
    let mut data = serde_json::to_vec(frame)?;
    data.push(b'\n');
    ensure!(
        data.len() <= MAX_FRAME_BYTES,
        "frame exceeds protocol size limit"
    );
    tokio::time::timeout(Duration::from_secs(2), stream.write_all(&data)).await??;
    Ok(())
}
async fn serve_one(stream: UnixStream, mut cache: watch::Receiver<Option<Cached>>) -> Result<()> {
    let mut reader = BufReader::new(stream);
    let bytes = tokio::time::timeout(Duration::from_secs(3), line(&mut reader, 4096))
        .await??
        .context("no request")?;
    let request: Request = serde_json::from_slice(&bytes)?;
    ensure!(
        request.schema_version == SCHEMA_VERSION,
        "unsupported schema version"
    );
    ensure!(
        request.op == "snapshot" || request.op == "subscribe",
        "unknown operation; read-only protocol"
    );
    let mut stream = reader.into_inner();
    let mut heartbeat = tokio::time::interval(Duration::from_secs(1));
    loop {
        let cached = cache.borrow_and_update().clone();
        if let Some(cached) = cached {
            send(&mut stream, &cached.fresh_frame()).await?;
            if request.op == "snapshot" {
                return Ok(());
            }
        }
        tokio::select! {
            changed=cache.changed() => { if changed.is_err() { return Ok(()); } }
            _=heartbeat.tick() => {}
        }
    }
}
pub async fn serve(
    listener: UnixListener,
    cache: watch::Receiver<Option<Cached>>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let clients = Arc::new(Semaphore::new(32));
    let mut tasks = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            _=shutdown.changed() => break,
            accepted=listener.accept() => {
                let (stream,_)=accepted?;
                if let Ok(permit)=clients.clone().try_acquire_owned() {
                    let cache=cache.clone(); tasks.spawn(async move { let _permit=permit; if let Err(e)=serve_one(stream,cache).await { eprintln!("IPC client disconnected: {e}"); } });
                }
            }
            Some(_)=tasks.join_next(), if !tasks.is_empty() => {}
        }
    }
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    Ok(())
}
pub async fn connect(path: &Path, subscribe: bool) -> Result<BufReader<UnixStream>> {
    let mut stream = tokio::time::timeout(Duration::from_secs(3), UnixStream::connect(path))
        .await
        .context("daemon connection timed out")?
        .with_context(|| format!("connect to {}", path.display()))?;
    let request = Request {
        schema_version: SCHEMA_VERSION,
        op: if subscribe { "subscribe" } else { "snapshot" }.into(),
    };
    let mut bytes = serde_json::to_vec(&request)?;
    bytes.push(b'\n');
    stream.write_all(&bytes).await?;
    Ok(BufReader::new(stream))
}
pub async fn next_frame(reader: &mut BufReader<UnixStream>) -> Result<Frame> {
    let bytes = tokio::time::timeout(Duration::from_secs(8), line(reader, MAX_FRAME_BYTES))
        .await??
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "daemon disconnected")
        })?;
    let frame: Frame = serde_json::from_slice(&bytes)?;
    if frame.schema_version != SCHEMA_VERSION {
        bail!("unsupported daemon schema version");
    }
    Ok(frame)
}

/// Only an absent/refused endpoint permits automatic hardware fallback. An
/// inaccessible, malformed, incompatible, or unresponsive daemon remains an error.
pub async fn connect_if_running(
    path: &Path,
    subscribe: bool,
) -> Result<Option<BufReader<UnixStream>>> {
    match connect(path, subscribe).await {
        Ok(reader) => Ok(Some(reader)),
        Err(error)
            if error.downcast_ref::<std::io::Error>().is_some_and(|e| {
                matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                )
            }) =>
        {
            match std::fs::symlink_metadata(path) {
                Ok(meta) => ensure!(
                    meta.file_type().is_socket(),
                    "daemon endpoint exists but is not a socket: {}",
                    path.display()
                ),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

pub fn disconnected(error: &anyhow::Error) -> bool {
    error.downcast_ref::<std::io::Error>().is_some_and(|e| {
        matches!(
            e.kind(),
            std::io::ErrorKind::UnexpectedEof
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::BrokenPipe
        )
    })
}
