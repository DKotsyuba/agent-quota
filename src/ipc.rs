//! Bounded, read-only local IPC authenticated by operating-system credentials.
//!
//! The socket directory and service UID are trusted administration boundaries.
//! Callers must signal shutdown and await `serve`; dropping its future is not a
//! substitute for joining synchronous handlers that have already started.

use std::ffi::CString;
use std::fs::{self, File, Metadata, OpenOptions};
use std::future::Future;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{oneshot, watch};
use tokio::task::JoinSet;
use tokio::time::{Instant, timeout, timeout_at};

const MAX_REQUEST: usize = 64 * 1024;
const MAX_OWNER_REQUEST: usize = 4 * 1024 * 1024;
const MAX_REPLY: usize = 4 * 1024 * 1024;
const MAX_CONNECTIONS: usize = 32;
const DEADLINE: Duration = Duration::from_secs(5);
static NEXT_STAGE: AtomicU64 = AtomicU64::new(0);

/// One protocol-v1 request; empty snapshot scope means all configured accounts.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    pub version: u8,
    pub method: String,
    #[serde(default)]
    pub accounts: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation: Option<Value>,
}

/// Exactly one result or fixed safe error; clients additionally validate version.
#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub(crate) enum Reply {
    Result { version: u8, result: Value },
    Error { version: u8, error: String },
}

impl Reply {
    /// Wraps an already redacted application result in protocol version one.
    pub(crate) fn ok(result: Value) -> Self {
        Self::Result { version: 1, result }
    }

    /// Rejects unrecognized messages so arbitrary diagnostics cannot reach peers.
    pub(crate) fn error(code: &str) -> Self {
        Self::Error {
            version: 1,
            error: if safe_error(code) {
                code
            } else {
                "internal_error"
            }
            .into(),
        }
    }

    /// Checks the envelope independently of application-specific result fields.
    fn valid(&self) -> bool {
        match self {
            Self::Result { version, .. } => *version == 1,
            Self::Error { version, error } => *version == 1 && safe_error(error),
        }
    }
}

/// Enumerates public protocol/domain failures without exposing their private cause.
fn safe_error(code: &str) -> bool {
    matches!(
        code,
        "invalid_request"
            | "unsupported_version"
            | "unsupported_method"
            | "unauthorized"
            | "busy"
            | "unavailable"
            | "internal_error"
            | "response_too_large"
            | "unknown_account"
            | "unknown_model"
            | "storage_error"
            | "invalid_scope"
            | "history_too_large"
            | "config_changed"
            | "invalid_observation"
            | "unsupported_source"
            | "outcome_unknown"
    )
}

/// Checks transport semantics; account identities remain the application's job.
fn validate_request(request: &Request) -> Result<(), &'static str> {
    if request.version != 1 {
        return Err("unsupported_version");
    }
    match request.method.as_str() {
        "status"
            if request.accounts.is_empty()
                && request.model.is_none()
                && request.observation.is_none() =>
        {
            Ok(())
        }
        "snapshot"
            if request.accounts.len() <= 256
                && request.observation.is_none()
                && request.model.as_ref().is_none_or(|model| {
                    !model.is_empty() && model.len() <= 256 && !model.chars().any(char::is_control)
                }) =>
        {
            Ok(())
        }
        "ingest"
            if request.accounts.is_empty()
                && request.model.is_none()
                && request.observation.is_some() =>
        {
            Ok(())
        }
        "status" | "snapshot" | "ingest" => Err("invalid_request"),
        _ => Err("unsupported_method"),
    }
}

/// Retrieves the service identity without reading environment or credentials.
pub(crate) fn owner_uid() -> u32 {
    // SAFETY: geteuid has no pointer arguments or preconditions.
    unsafe { libc::geteuid() }
}

/// Authorizes only the owning UID or an explicitly configured additional UID.
fn authorized(owner: u32, allowed: &[u32], peer: u32) -> bool {
    owner == peer || allowed.contains(&peer)
}

/// Serializes directly into a capped buffer, including the terminating newline.
fn encode<T: Serialize>(value: &T, limit: usize) -> Result<Vec<u8>, ()> {
    let mut output = LimitedBuffer {
        bytes: Vec::new(),
        limit: limit.saturating_sub(1),
    };
    serde_json::to_writer(&mut output, value).map_err(|_| ())?;
    output.bytes.push(b'\n');
    Ok(output.bytes)
}

/// Enforces the allocation budget while serde writes its output.
struct LimitedBuffer {
    bytes: Vec<u8>,
    limit: usize,
}

impl Write for LimitedBuffer {
    /// Refuses a write in full when it would exceed the frame budget.
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(io::Error::other("frame_limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    /// Memory-backed output has nothing to flush.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Reads one newline-terminated frame with a fixed allocation ceiling.
async fn read_frame(stream: &mut UnixStream, limit: usize) -> Result<Vec<u8>, ()> {
    let mut frame = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let remaining = limit.saturating_sub(frame.len());
        if remaining == 0 {
            return Err(());
        }
        let size = remaining.min(chunk.len());
        let count = stream.read(&mut chunk[..size]).await.map_err(|_| ())?;
        if count == 0 {
            return Err(());
        }
        if let Some(end) = chunk[..count].iter().position(|byte| *byte == b'\n') {
            // One request per connection; reject already-buffered pipelined data.
            if end + 1 != count {
                return Err(());
            }
            frame.extend_from_slice(&chunk[..end]);
            return Ok(frame);
        }
        frame.extend_from_slice(&chunk[..count]);
    }
}

/// Performs a bounded request and verifies both socket owner and peer against the expected UID.
/// A possible ingest write with a lost response returns outcome_unknown and is never retried.
pub(crate) async fn request(
    path: &Path,
    req: &Request,
    expected_uid: u32,
) -> Result<Reply, String> {
    validate_request(req).map_err(str::to_owned)?;
    let mutation = req.method == "ingest";
    let bytes = encode(
        req,
        if mutation {
            MAX_OWNER_REQUEST
        } else {
            MAX_REQUEST
        },
    )
    .map_err(|_| "invalid_request".to_owned())?;
    let mut may_have_effect = false;
    let result = timeout(DEADLINE, async {
        let before = fs::symlink_metadata(path).map_err(|_| "unavailable")?;
        if !before.file_type().is_socket()
            || before.mode() & 0o007 != 0
            || before.uid() != expected_uid
        {
            return Err("unavailable");
        }
        let mut stream = UnixStream::connect(path).await.map_err(|_| "unavailable")?;
        let peer = stream.peer_cred().map_err(|_| "unavailable")?;
        let after = fs::symlink_metadata(path).map_err(|_| "unavailable")?;
        if !same_inode(&before, &after) || peer.uid() != expected_uid {
            return Err("unavailable");
        }
        may_have_effect = mutation;
        let after_write = if mutation {
            "outcome_unknown"
        } else {
            "unavailable"
        };
        stream.write_all(&bytes).await.map_err(|_| after_write)?;
        let frame = read_frame(&mut stream, MAX_REPLY)
            .await
            .map_err(|_| after_write)?;
        let reply: Reply = serde_json::from_slice(&frame).map_err(|_| after_write)?;
        if !reply.valid() {
            return Err(after_write);
        }
        Ok(reply)
    })
    .await;
    match result {
        Ok(result) => result.map_err(str::to_owned),
        Err(_) if may_have_effect => Err("outcome_unknown".into()),
        Err(_) => Err("unavailable".into()),
    }
}

/// Owns a locked service endpoint; the lock file deliberately survives shutdown.
struct Endpoint {
    path: PathBuf,
    identity: Metadata,
    _lock: File,
}

impl Endpoint {
    /// Removes only this endpoint's inode, preserving replacements and other files.
    fn cleanup(&self) -> Result<(), String> {
        match fs::symlink_metadata(&self.path) {
            Ok(current)
                if same_inode(&self.identity, &current) && current.file_type().is_socket() =>
            {
                fs::remove_file(&self.path).map_err(|_| "socket_cleanup_failed".to_owned())
            }
            Ok(_) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err("socket_cleanup_failed".into()),
        }
    }
}

impl Drop for Endpoint {
    /// Provides best-effort inode-checked cleanup on early return or unwinding.
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

/// Matches both filesystem identity and owner, never only a pathname.
fn same_inode(left: &Metadata, right: &Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino() && left.uid() == right.uid()
}

/// Requires an existing real owner-controlled directory; never changes its mode.
fn directory(path: &Path, shared: bool) -> Result<Metadata, String> {
    let metadata = fs::symlink_metadata(path).map_err(|_| "unsafe_socket_directory")?;
    if !metadata.is_dir() || metadata.uid() != owner_uid() || metadata.mode() & 0o022 != 0 {
        return Err("unsafe_socket_directory".into());
    }
    if shared && metadata.mode() & 0o2010 != 0o2010 {
        return Err("shared_socket_directory_requires_setgid_and_group_execute".into());
    }
    Ok(metadata)
}

/// Opens a stable no-follow owner-private regular lock and refuses concurrent owners.
fn lock_endpoint(path: &Path) -> Result<File, String> {
    let mut name = path.as_os_str().to_os_string();
    name.push(".lock");
    let path = PathBuf::from(name);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)
        .map_err(|_| "unsafe_socket_lock")?;
    let opened = file.metadata().map_err(|_| "unsafe_socket_lock")?;
    let named = fs::symlink_metadata(&path).map_err(|_| "unsafe_socket_lock")?;
    if !opened.is_file()
        || opened.uid() != owner_uid()
        || opened.mode() & 0o077 != 0
        || opened.nlink() != 1
        || !same_inode(&opened, &named)
    {
        return Err("unsafe_socket_lock".into());
    }
    file.try_lock_exclusive()
        .map_err(|_| "socket_already_owned")?;
    Ok(file)
}

/// Removes a same-owner socket only after a positive connection-refused result.
async fn remove_stale(path: &Path) -> Result<(), String> {
    let old = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err("unsafe_socket_path".into()),
    };
    if !old.file_type().is_socket() || old.uid() != owner_uid() {
        return Err("unsafe_socket_path".into());
    }
    match timeout(DEADLINE, UnixStream::connect(path)).await {
        Ok(Err(error)) if error.raw_os_error() == Some(libc::ECONNREFUSED) => {}
        _ => return Err("socket_already_active".into()),
    }
    let current = fs::symlink_metadata(path).map_err(|_| "socket_changed")?;
    if !same_inode(&old, &current) || !current.file_type().is_socket() {
        return Err("socket_changed".into());
    }
    fs::remove_file(path).map_err(|_| "socket_cleanup_failed".into())
}

/// Temporary owner-only bind directory prevents publication with permissive mode.
struct Staging {
    path: PathBuf,
}

impl Staging {
    /// Allocates a private sibling directory without replacing existing entries.
    fn new(parent: &Path) -> Result<Self, String> {
        for _ in 0..32 {
            let name = format!(".q{:x}", NEXT_STAGE.fetch_add(1, Ordering::Relaxed));
            let path = parent.join(name);
            match fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(_) => return Err("socket_bind_failed".into()),
            }
        }
        Err("socket_bind_failed".into())
    }
}

impl Drop for Staging {
    /// Removes only the private staging entry and empty directory, never recursively.
    fn drop(&mut self) {
        let _ = fs::remove_file(self.path.join("s"));
        let _ = fs::remove_dir(&self.path);
    }
}

/// Publishes a fully permissioned socket without overwriting any competing entry.
pub(crate) fn publish(source: &Path, destination: &Path) -> Result<(), String> {
    let source = CString::new(source.as_os_str().as_bytes()).map_err(|_| "socket_bind_failed")?;
    let destination =
        CString::new(destination.as_os_str().as_bytes()).map_err(|_| "socket_bind_failed")?;
    #[cfg(target_os = "linux")]
    // SAFETY: both path pointers are valid NUL-terminated strings; AT_FDCWD
    // requires no descriptor ownership. NOREPLACE preserves any existing entry.
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(target_os = "macos")]
    // SAFETY: both path pointers are valid NUL-terminated strings and RENAME_EXCL
    // atomically refuses to replace another entry.
    let result =
        unsafe { libc::renamex_np(source.as_ptr(), destination.as_ptr(), libc::RENAME_EXCL) };
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let result = -1;
    if result == 0 {
        Ok(())
    } else {
        Err("socket_publish_failed".into())
    }
}

/// Acquires ownership, recovers a stale socket and publishes the protected endpoint.
async fn bind(path: &Path, shared: bool) -> Result<(UnixListener, Endpoint), String> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or("unsafe_socket_directory")?;
    let parent_identity = directory(parent, shared)?;
    let lock = lock_endpoint(path)?;
    remove_stale(path).await?;
    let stage = Staging::new(parent)?;
    let temporary = stage.path.join("s");
    let listener = UnixListener::bind(&temporary).map_err(|_| "socket_bind_failed")?;
    fs::set_permissions(
        &temporary,
        fs::Permissions::from_mode(if shared { 0o660 } else { 0o600 }),
    )
    .map_err(|_| "socket_permissions_failed")?;
    let identity = fs::symlink_metadata(&temporary).map_err(|_| "socket_bind_failed")?;
    if identity.uid() != owner_uid() || (shared && identity.gid() != parent_identity.gid()) {
        return Err("socket_permissions_failed".into());
    }
    let current_parent = directory(parent, shared)?;
    if !same_inode(&parent_identity, &current_parent) {
        return Err("socket_directory_changed".into());
    }
    publish(&temporary, path)?;
    Ok((
        listener,
        Endpoint {
            path: path.into(),
            identity,
            _lock: lock,
        },
    ))
}

/// Handles one authorized read; synchronous work remains joined even after timeout.
async fn connection<F>(
    mut stream: UnixStream,
    handler: Arc<F>,
    mut stop: watch::Receiver<bool>,
    is_owner: bool,
) -> Result<(), &'static str>
where
    F: Fn(Request) -> Reply + Send + Sync + 'static,
{
    let deadline = Instant::now() + DEADLINE;
    let frame = tokio::select! {
        biased;
        _ = stop.changed() => return Ok(()),
        result = timeout_at(deadline, read_frame(&mut stream, if is_owner { MAX_OWNER_REQUEST } else { MAX_REQUEST })) => result,
    };
    let reply = match frame {
        Ok(Ok(frame)) => match serde_json::from_slice::<Request>(&frame) {
            Ok(request)
                if request.method != "ingest" && frame.len().saturating_add(1) > MAX_REQUEST =>
            {
                Reply::error("invalid_request")
            }
            Ok(request) => match validate_request(&request) {
                Ok(()) if request.method == "ingest" && !is_owner => Reply::error("unauthorized"),
                Ok(()) => {
                    if Instant::now() >= deadline || *stop.borrow() {
                        return Ok(());
                    }
                    // Keep this handle until actual completion; a started blocking
                    // closure cannot be cancelled by a Tokio deadline or abort.
                    tokio::task::spawn_blocking(move || handler(request))
                        .await
                        .map_err(|_| "ipc_handler_failed")?
                }
                Err(code) => Reply::error(code),
            },
            Err(_) => Reply::error("invalid_request"),
        },
        Ok(Err(())) => Reply::error("invalid_request"),
        Err(_) => return Ok(()),
    };
    if *stop.borrow() {
        return Ok(());
    }
    let reply = if reply.valid() {
        reply
    } else {
        Reply::error("internal_error")
    };
    let bytes = encode(&reply, MAX_REPLY)
        .or_else(|_| encode(&Reply::error("response_too_large"), MAX_REPLY))
        .map_err(|_| "ipc_encode_failed")?;
    tokio::select! {
        biased;
        _ = stop.changed() => {},
        _ = timeout_at(deadline, stream.write_all(&bytes)) => {},
    }
    Ok(())
}

/// Serves owner/allowlisted peers, with at most 32 connections and five-second I/O.
/// The read-only handler must bound its own synchronous work. Shutdown stops
/// admission, closes idle connections, and joins every started handler before
/// removing this socket inode. Keep awaiting this future after signaling shutdown.
#[cfg(test)]
pub(crate) async fn serve<F, S>(
    path: &Path,
    allowed_uids: Vec<u32>,
    handler: F,
    shutdown: S,
) -> Result<(), String>
where
    F: Fn(Request) -> Reply + Send + Sync + 'static,
    S: Future<Output = ()> + Send,
{
    serve_inner(path, allowed_uids, handler, shutdown, None).await
}

/// Serves reads and reports readiness only after the owned socket is published.
pub(crate) async fn serve_ready<F, S>(
    path: &Path,
    allowed_uids: Vec<u32>,
    handler: F,
    shutdown: S,
    ready: oneshot::Sender<()>,
) -> Result<(), String>
where
    F: Fn(Request) -> Reply + Send + Sync + 'static,
    S: Future<Output = ()> + Send,
{
    serve_inner(path, allowed_uids, handler, shutdown, Some(ready)).await
}

/// Owns the listener and every accepted handler until cooperative shutdown.
async fn serve_inner<F, S>(
    path: &Path,
    allowed_uids: Vec<u32>,
    handler: F,
    shutdown: S,
    ready: Option<oneshot::Sender<()>>,
) -> Result<(), String>
where
    F: Fn(Request) -> Reply + Send + Sync + 'static,
    S: Future<Output = ()> + Send,
{
    let (listener, endpoint) = bind(path, !allowed_uids.is_empty()).await?;
    if ready.is_some_and(|sender| sender.send(()).is_err()) {
        endpoint.cleanup()?;
        return Err("startup_cancelled".into());
    }
    let handler = Arc::new(handler);
    let (stop, stop_rx) = watch::channel(false);
    let mut connections = JoinSet::new();
    let mut failure = None;
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => break,
            outcome = connections.join_next(), if !connections.is_empty() => {
                match outcome {
                    Some(Ok(Ok(()))) => {},
                    _ => { failure = Some("ipc_handler_failed".to_owned()); break; },
                }
            },
            accepted = listener.accept() => {
                let (stream, _) = match accepted {
                    Ok(connection) => connection,
                    Err(_) => { failure = Some("socket_accept_failed".to_owned()); break; },
                };
                // Unauthorized and excess peers are closed without parsing their
                // input or spawning additional tasks, keeping admission bounded.
                let peer = match stream.peer_cred() { Ok(peer) => peer, Err(_) => continue };
                if connections.len() >= MAX_CONNECTIONS || !authorized(owner_uid(), &allowed_uids, peer.uid()) {
                    continue;
                }
                connections.spawn(connection(stream, Arc::clone(&handler), stop_rx.clone(), peer.uid() == owner_uid()));
            },
        }
    }
    drop(listener);
    let _ = stop.send(true);
    while let Some(outcome) = connections.join_next().await {
        if !matches!(outcome, Ok(Ok(()))) {
            failure = Some("ipc_handler_failed".into());
        }
    }
    endpoint.cleanup()?;
    match failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::sync::Mutex;
    use tokio::sync::oneshot;
    use tokio::task::JoinHandle;

    /// Gives socket fixtures a short private path beneath the prepared TMPDIR.
    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::Builder::new()
            .prefix("i")
            .tempdir()
            .expect("fixture directory");
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700))
            .expect("fixture permissions");
        dir
    }

    /// Versioned request and error examples remain accepted by the wire contract.
    #[test]
    fn version_one_wire_fixtures_match() {
        let request_value: Value =
            serde_json::from_str(include_str!("../tests/fixtures/snapshot-request-v1.json"))
                .expect("request fixture");
        let parsed: Request =
            serde_json::from_value(request_value.clone()).expect("request contract");
        assert!(validate_request(&parsed).is_ok());
        assert_eq!(
            serde_json::to_value(parsed).expect("request JSON"),
            request_value
        );
        let error_value: Value =
            serde_json::from_str(include_str!("../tests/fixtures/unknown-account-v1.json"))
                .expect("error fixture");
        assert_eq!(
            serde_json::to_value(Reply::error("unknown_account")).expect("error JSON"),
            error_value
        );
    }

    /// Waits for a real connection, with an outer bound for failed server startup.
    async fn ready(path: &Path) {
        timeout(Duration::from_secs(2), async {
            loop {
                if UnixStream::connect(path).await.is_ok() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("server readiness");
    }

    /// Starts an isolated owner-only server and returns its cooperative stop handle.
    async fn start<F>(
        path: &Path,
        handler: F,
    ) -> (oneshot::Sender<()>, JoinHandle<Result<(), String>>)
    where
        F: Fn(Request) -> Reply + Send + Sync + 'static,
    {
        let (stop, stopped) = oneshot::channel();
        let owned_path = path.to_owned();
        let handle = tokio::spawn(async move {
            serve(&owned_path, vec![], handler, async {
                let _ = stopped.await;
            })
            .await
        });
        ready(path).await;
        (stop, handle)
    }

    /// Exchanges raw bytes to exercise framing before request validation.
    async fn raw(path: &Path, bytes: &[u8]) -> Value {
        timeout(Duration::from_secs(2), async {
            let mut stream = UnixStream::connect(path).await.expect("fixture connect");
            stream.write_all(bytes).await.expect("fixture request");
            let frame = read_frame(&mut stream, MAX_REPLY)
                .await
                .expect("fixture response");
            serde_json::from_slice(&frame).expect("response JSON")
        })
        .await
        .expect("bounded exchange")
    }

    #[tokio::test]
    async fn scoped_snapshot_and_status_roundtrip() {
        let dir = fixture();
        let path = dir.path().join("s");
        let (stop, server) = start(&path, |req| {
            Reply::ok(serde_json::json!({
                "method": req.method, "scope": req.accounts
            }))
        })
        .await;
        for (method, accounts) in [
            ("status", vec![]),
            ("snapshot", vec!["acct-a".into()]),
            ("snapshot", vec![]),
        ] {
            let reply = request(
                &path,
                &Request {
                    version: 1,
                    method: method.into(),
                    accounts: accounts.clone(),
                    model: None,
                    observation: None,
                },
                owner_uid(),
            )
            .await
            .expect("valid read");
            assert_eq!(
                serde_json::to_value(reply).expect("JSON"),
                serde_json::json!({
                    "version": 1, "result": { "method": method, "scope": accounts }
                })
            );
        }
        let wrong_uid = owner_uid().wrapping_add(1);
        let req = Request {
            version: 1,
            method: "status".into(),
            accounts: vec![],
            model: None,
            observation: None,
        };
        assert!(request(&path, &req, wrong_uid).await.is_err());
        assert_eq!(
            fs::metadata(&path).expect("socket metadata").mode() & 0o777,
            0o600
        );
        stop.send(()).expect("shutdown signal");
        server.await.expect("server task").expect("clean shutdown");
        assert!(!path.exists());
        assert!(dir.path().join("s.lock").is_file());
        assert_eq!(fs::read_dir(dir.path()).expect("directory").count(), 1);
    }

    /// An authorized reader UID cannot invoke the owner-only ingestion path.
    #[tokio::test]
    async fn non_owner_ingestion_never_reaches_handler() {
        let (mut client, server) = UnixStream::pair().expect("socket pair");
        let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = Arc::clone(&called);
        let handler = Arc::new(move |_: Request| {
            observed.store(true, Ordering::SeqCst);
            Reply::ok(Value::Null)
        });
        let (_stop, receiver) = watch::channel(false);
        let task = tokio::spawn(connection(server, handler, receiver, false));
        let req = Request {
            version: 1,
            method: "ingest".into(),
            accounts: vec![],
            model: None,
            observation: Some(serde_json::json!({"version":1})),
        };
        client
            .write_all(&encode(&req, MAX_REQUEST).expect("request"))
            .await
            .expect("write");
        let frame = read_frame(&mut client, MAX_REPLY).await.expect("reply");
        let reply: Value = serde_json::from_slice(&frame).expect("reply JSON");
        assert_eq!(reply["error"], "unauthorized");
        assert!(!called.load(Ordering::SeqCst));
        task.await
            .expect("connection joined")
            .expect("connection result");
    }

    /// A lost ingestion acknowledgement is unknown and is never replayed automatically.
    #[tokio::test]
    async fn lost_ingestion_reply_is_unknown() {
        let dir = fixture();
        let path = dir.path().join("s");
        let listener = UnixListener::bind(&path).expect("listener");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("private socket");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("one request");
            let _ = read_frame(&mut stream, MAX_OWNER_REQUEST)
                .await
                .expect("ingest frame");
        });
        let req = Request {
            version: 1,
            method: "ingest".into(),
            accounts: vec![],
            model: None,
            observation: Some(serde_json::json!({"version":1})),
        };
        assert!(
            matches!(request(&path, &req, owner_uid()).await, Err(code) if code == "outcome_unknown")
        );
        server.await.expect("server joined");
    }

    #[tokio::test]
    async fn malformed_oversized_and_invalid_requests_are_fixed_errors() {
        let dir = fixture();
        let path = dir.path().join("s");
        let (stop, server) =
            start(&path, |_| panic!("invalid requests must not reach handler")).await;
        for (bytes, error) in [
            (b"secret malformed\n".as_slice(), "invalid_request"),
            (
                b"{\"version\":2,\"method\":\"status\"}\n",
                "unsupported_version",
            ),
            (
                b"{\"version\":1,\"method\":\"mutate\"}\n",
                "unsupported_method",
            ),
            (
                b"{\"version\":1,\"method\":\"status\",\"accounts\":[\"a\"]}\n",
                "invalid_request",
            ),
            (
                b"{\"version\":1,\"method\":\"status\",\"secret\":\"private\"}\n",
                "invalid_request",
            ),
        ] {
            assert_eq!(
                raw(&path, bytes).await,
                serde_json::json!({"version":1,"error":error})
            );
        }
        let oversized = vec![b'x'; MAX_OWNER_REQUEST + 1];
        assert_eq!(raw(&path, &oversized).await["error"], "invalid_request");
        let oversized_read = serde_json::json!({"version":1,"method":"snapshot","accounts":["a".repeat(MAX_REQUEST)]});
        let oversized_read = format!("{oversized_read}\n");
        assert_eq!(
            raw(&path, oversized_read.as_bytes()).await["error"],
            "invalid_request"
        );
        let too_many = Request {
            version: 1,
            method: "snapshot".into(),
            accounts: vec!["a".into(); 257],
            model: None,
            observation: None,
        };
        assert_eq!(
            raw(&path, &encode(&too_many, MAX_REQUEST).expect("encode")).await["error"],
            "invalid_request"
        );
        stop.send(()).expect("shutdown signal");
        server.await.expect("server task").expect("shutdown");
    }

    #[tokio::test]
    async fn refuses_regular_symlink_and_live_socket_without_modification() {
        let dir = fixture();
        let path = dir.path().join("s");
        fs::write(&path, b"preserve").expect("ordinary file");
        assert!(bind(&path, false).await.is_err());
        assert_eq!(fs::read(&path).expect("preserved file"), b"preserve");
        fs::remove_file(&path).expect("remove fixture file");
        let target = dir.path().join("target");
        fs::write(&target, b"preserve").expect("target");
        symlink(&target, &path).expect("symlink");
        assert!(bind(&path, false).await.is_err());
        assert!(
            fs::symlink_metadata(&path)
                .expect("symlink metadata")
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(&target).expect("preserved target"), b"preserve");
        fs::remove_file(&path).expect("remove fixture symlink");
        let live = UnixListener::bind(&path).expect("unmanaged fixture listener");
        let before = fs::symlink_metadata(&path).expect("socket metadata");
        assert!(bind(&path, false).await.is_err());
        assert!(same_inode(
            &before,
            &fs::symlink_metadata(&path).expect("unchanged socket")
        ));
        drop(live);
    }

    #[tokio::test]
    async fn second_server_is_locked_and_owned_stale_socket_is_recovered() {
        let dir = fixture();
        let path = dir.path().join("s");
        let stale = UnixListener::bind(&path).expect("stale fixture");
        drop(stale);
        let (listener, endpoint) = bind(&path, false)
            .await
            .expect("recover owned stale socket");
        assert!(matches!(bind(&path, false).await, Err(error) if error == "socket_already_owned"));
        drop(listener);
        endpoint.cleanup().expect("cleanup");
        drop(endpoint);
        let (listener, endpoint) = bind(&path, false).await.expect("reuse retained lock inode");
        drop(listener);
        drop(endpoint);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn shutdown_preserves_replacement_inode_and_closes_idle_connections() {
        let dir = fixture();
        let path = dir.path().join("s");
        let (stop, server) = start(&path, |_| Reply::ok(Value::Null)).await;
        let mut idle = UnixStream::connect(&path).await.expect("idle connection");
        idle.write_all(b"{\"version\":")
            .await
            .expect("partial request");
        fs::remove_file(&path).expect("replace fixture socket");
        fs::write(&path, b"replacement").expect("replacement file");
        stop.send(()).expect("shutdown signal");
        timeout(Duration::from_secs(1), server)
            .await
            .expect("idle shutdown bounded")
            .expect("server task")
            .expect("shutdown");
        assert_eq!(
            fs::read(&path).expect("replacement preserved"),
            b"replacement"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_joins_started_blocking_handler() {
        let dir = fixture();
        let path = dir.path().join("s");
        let (entered, entered_rx) = oneshot::channel();
        let entered = Mutex::new(Some(entered));
        let (release, released) = std::sync::mpsc::channel();
        let released = Mutex::new(released);
        let (stop, server) = start(&path, move |_| {
            entered
                .lock()
                .expect("entry lock")
                .take()
                .expect("first request")
                .send(())
                .expect("entered");
            released
                .lock()
                .expect("release lock")
                .recv_timeout(Duration::from_secs(2))
                .expect("release handler");
            Reply::ok(Value::Null)
        })
        .await;
        let mut stream = UnixStream::connect(&path).await.expect("connect");
        stream
            .write_all(b"{\"version\":1,\"method\":\"status\"}\n")
            .await
            .expect("request");
        entered_rx.await.expect("handler entered");
        stop.send(()).expect("shutdown signal");
        tokio::task::yield_now().await;
        assert!(
            !server.is_finished(),
            "shutdown must not detach the handler"
        );
        release.send(()).expect("release handler");
        server.await.expect("server task").expect("joined shutdown");
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn handler_panic_is_joined_and_endpoint_cleaned() {
        let dir = fixture();
        let path = dir.path().join("s");
        let (_stop, server) = start(&path, |_| panic!("synthetic handler panic")).await;
        let req = Request {
            version: 1,
            method: "status".into(),
            accounts: vec![],
            model: None,
            observation: None,
        };
        assert!(request(&path, &req, owner_uid()).await.is_err());
        assert_eq!(
            server.await.expect("supervisor survived"),
            Err("ipc_handler_failed".into())
        );
        assert!(!path.exists());
    }

    #[test]
    fn peer_policy_denies_unlisted_uid_and_errors_never_echo_input() {
        assert!(authorized(1000, &[], 1000));
        assert!(!authorized(1000, &[], 1001));
        assert!(authorized(1000, &[1001], 1001));
        assert!(!authorized(1000, &[1001], 1002));
        assert_eq!(
            serde_json::to_value(Reply::error("private collector secret")).expect("JSON")["error"],
            "internal_error"
        );
        for bad in [
            r#"{"version":1,"result":{},"error":"unavailable"}"#,
            r#"{"version":1,"result":{},"extra":true}"#,
        ] {
            assert!(serde_json::from_str::<Reply>(bad).is_err());
        }
        assert!(
            !serde_json::from_str::<Reply>(r#"{"version":2,"result":{}}"#)
                .expect("decode")
                .valid()
        );
        assert!(
            !serde_json::from_str::<Reply>(r#"{"version":1,"error":"private"}"#)
                .expect("decode")
                .valid()
        );
    }

    #[tokio::test]
    async fn refuses_unsafe_directory_lock_and_unprepared_shared_mode() {
        let dir = fixture();
        let path = dir.path().join("s");
        assert!(bind(&path, true).await.is_err());
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o770))
            .expect("unsafe mode fixture");
        assert!(bind(&path, false).await.is_err());
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700))
            .expect("restore fixture mode");
        let target = dir.path().join("target");
        fs::write(&target, b"unchanged").expect("target");
        symlink(&target, dir.path().join("s.lock")).expect("lock symlink");
        assert!(bind(&path, false).await.is_err());
        assert_eq!(fs::read(&target).expect("preserved target"), b"unchanged");
        fs::remove_file(dir.path().join("s.lock")).expect("remove fixture link");
        fs::write(dir.path().join("s.lock"), b"private").expect("unsafe lock");
        fs::set_permissions(dir.path().join("s.lock"), fs::Permissions::from_mode(0o644))
            .expect("unsafe lock mode");
        assert!(bind(&path, false).await.is_err());
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn prepared_shared_directory_preserves_group_and_parent_permissions() {
        let dir = fixture();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o2710))
            .expect("prepared shared directory");
        let original = fs::metadata(dir.path()).expect("parent metadata");
        let path = dir.path().join("s");
        let (listener, endpoint) = bind(&path, true).await.expect("shared socket");
        let socket = fs::metadata(&path).expect("socket metadata");
        assert_eq!(socket.mode() & 0o777, 0o660);
        assert_eq!(socket.gid(), original.gid());
        assert_eq!(
            fs::metadata(dir.path()).expect("parent unchanged").mode(),
            original.mode()
        );
        drop(listener);
        drop(endpoint);
    }

    #[tokio::test]
    async fn oversized_reply_is_explicit_and_domain_failure_stays_an_error() {
        let dir = fixture();
        let path = dir.path().join("s");
        let (stop, server) = start(&path, |request| {
            if request.method == "status" {
                Reply::ok(Value::String("x".repeat(MAX_REPLY)))
            } else {
                Reply::error("storage_error")
            }
        })
        .await;
        for (method, expected) in [
            ("status", "response_too_large"),
            ("snapshot", "storage_error"),
        ] {
            let reply = request(
                &path,
                &Request {
                    version: 1,
                    method: method.into(),
                    accounts: vec![],
                    model: None,
                    observation: None,
                },
                owner_uid(),
            )
            .await
            .expect("well-formed error reply");
            assert_eq!(
                serde_json::to_value(reply).expect("JSON")["error"],
                expected
            );
        }
        stop.send(()).expect("shutdown");
        server.await.expect("server task").expect("joined shutdown");
    }

    #[test]
    fn atomic_publication_preserves_existing_destination() {
        let dir = fixture();
        let source = dir.path().join("source");
        let destination = dir.path().join("destination");
        fs::write(&source, b"new").expect("source");
        fs::write(&destination, b"preserve").expect("destination");
        assert!(publish(&source, &destination).is_err());
        assert_eq!(
            fs::read(&destination).expect("existing destination"),
            b"preserve"
        );
        assert_eq!(fs::read(&source).expect("existing source"), b"new");
    }
}
