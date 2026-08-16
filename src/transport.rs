//! JSONL request/response/event framing (see `crate::protocol` for the
//! wire contract) over `rusty_tokio`'s native async
//! `io::UnixListener`/`UnixStream` -- genuinely non-blocking on every
//! platform this project targets (Linux/macOS/BSD via epoll/kqueue,
//! Windows via IOCP+AFD-poll), now that `rusty_tokio`'s own Windows
//! AF_UNIX support has landed. See `ARCHITECTURE.md` "IPC Model" for why
//! an earlier revision of this file instead bridged `rustils`' blocking
//! `Net` trait through `spawn_blocking` by hand, and why that bridge was
//! removed once this project could depend on `rusty_tokio`'s own support
//! directly instead.

use std::path::PathBuf;
use std::time::Duration;

use rusty_tokio::io::{UnixListener, UnixStream};

use crate::error::{Context, HarnessError, Result};
use crate::protocol::{Request, Response, SessionEvent};

/// An owned, bound Unix-domain listener.
pub struct Listener {
    inner: UnixListener,
}

impl Listener {
    /// Binds and starts listening at `path`, retrying on `AddrInUse` for
    /// up to `timeout`.
    ///
    /// Stale leftover socket file reclaim (a listener that died without
    /// unlinking it) is *supposed* to be handled underneath by
    /// `rusty_tokio`/`rustils` via a probe `connect()` -- see
    /// `rusty_tokio::io::UnixListener::bind`'s own doc. On Windows it
    /// isn't reliable enough to depend on alone: real `windows-latest`
    /// CI running this project's own `tests/supervisor_restart_recovery.rs`
    /// found that rustils' own stale-vs-live probe can see `WSAENOBUFS`
    /// ("No buffer space available") instead of the expected
    /// `WSAECONNREFUSED` in exactly the race this function's own retry
    /// loop exists for -- and, per real evidence gathered across several
    /// rounds of upstream fixes and diagnostics (see
    /// `docs/decision-request-af-unix-stale-reclaim-race.md` in the
    /// `rustils` repo), that code does not reliably clear within any
    /// bounded retry window this project can afford to wait out: it was
    /// observed persisting for a full 20 continuous seconds, identically
    /// on every one of dozens of fresh probe attempts, in this project's
    /// own real supervisor-restart scenario (which involves a fuller,
    /// more realistic connection history than the isolated rustils-level
    /// regression test that first diagnosed and fixed the underlying
    /// `os error 0` bug -- that fix is real and confirmed on real
    /// hardware, it just isn't the whole story for this project's own,
    /// more complex case).
    ///
    /// So this project no longer trusts `rustils`' internal
    /// stale-vs-live classification alone: on any `AddrInUse` bind
    /// failure, it authoritatively checks liveness itself via
    /// [`probe`] (a real connect + `Ping`/`Pong` round trip, not just
    /// "did `connect()` return without erroring" -- see that function's
    /// own doc comment for why a bare successful `connect()` isn't
    /// sufficient evidence either), and force-removes the socket file
    /// if nothing answers. `probe`'s own "not alive" verdict doesn't
    /// depend on which specific OS error a failed `connect()` produced
    /// -- any failure to complete a real `Ping`/`Pong` round trip within
    /// its budget counts, which is exactly the property that lets this
    /// sidestep the `WSAENOBUFS`-vs-`WSAECONNREFUSED` classification
    /// problem entirely rather than trying to enumerate every anomalous
    /// code Windows might report.
    pub async fn bind_with_retry(
        context: Context,
        path: PathBuf,
        timeout: Duration,
    ) -> Result<Self> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match UnixListener::bind(&path) {
                Ok(inner) => return Ok(Listener { inner }),
                Err(e)
                    if e.kind() == std::io::ErrorKind::AddrInUse
                        && std::time::Instant::now() < deadline =>
                {
                    if !probe(context, path.clone()).await {
                        // Best-effort: if the remove itself fails (e.g.
                        // a genuinely live listener answered in the
                        // instant between the probe and this call --
                        // vanishingly unlikely, but not impossible),
                        // the next loop iteration's own bind attempt is
                        // still the authority on whether the path is
                        // actually free, not this removal's own result.
                        let _ = std::fs::remove_file(&path);
                    }
                    rusty_tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(e) => return Err(HarnessError::io(context, Some(path), e)),
            }
        }
    }

    /// Block until one connection arrives.
    pub async fn accept(&mut self, context: Context) -> Result<LineStream> {
        let (stream, _peer) = self
            .inner
            .accept()
            .await
            .map_err(|e| HarnessError::io(context, None, e))?;
        Ok(LineStream::new(stream))
    }
}

/// Connect to the Unix-domain socket bound at `path`.
pub async fn connect(context: Context, path: PathBuf) -> Result<LineStream> {
    let stream = UnixStream::connect(&path)
        .await
        .map_err(|e| HarnessError::io(context, Some(path), e))?;
    Ok(LineStream::new(stream))
}

/// One connection, framed as newline-delimited JSON in both directions.
pub struct LineStream {
    stream: UnixStream,
    /// Bytes read past the last complete line, kept across calls since
    /// one `read` can return more than one line's worth at once.
    buf: Vec<u8>,
    /// Whether anything has been written on this connection yet.
    ///
    /// Exists for exactly one caller: `daemon::Supervisor::
    /// handle_public_connection` turns a late `Conflict` (a fence
    /// rejection, most often) into a terminal `Response::Error` so the
    /// client is told why rather than watching the connection close --
    /// but only when the handler hasn't already started streaming, since
    /// appending a `Response` line to a half-written `SessionEvent`
    /// stream would replace one error with a worse, more confusing one.
    wrote_any: bool,
}

impl LineStream {
    fn new(stream: UnixStream) -> Self {
        LineStream {
            stream,
            buf: Vec::new(),
            wrote_any: false,
        }
    }

    /// See [`LineStream::wrote_any`].
    pub fn has_written(&self) -> bool {
        self.wrote_any
    }

    /// Reads one `\n`-terminated line (the trailing `\n`, and a `\r`
    /// immediately before it, are stripped). `Ok(None)` is a clean EOF
    /// with no partial line pending; a non-empty read followed by EOF
    /// with no trailing newline is still returned as one final line,
    /// matching `BufRead::read_line`'s own convention.
    pub async fn read_line(&mut self, context: Context) -> Result<Option<String>> {
        loop {
            if let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
                let mut line: Vec<u8> = self.buf.drain(..=pos).collect();
                line.pop(); // trailing '\n'
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                let text = String::from_utf8(line)
                    .map_err(|e| HarnessError::protocol(context, format!("non-utf8 line: {e}")))?;
                return Ok(Some(text));
            }
            let mut chunk = [0u8; 8192];
            let n = self
                .stream
                .read(&mut chunk)
                .await
                .map_err(|e| HarnessError::io(context, None, e))?;
            if n == 0 {
                if self.buf.is_empty() {
                    return Ok(None);
                }
                let text = String::from_utf8(std::mem::take(&mut self.buf))
                    .map_err(|e| HarnessError::protocol(context, format!("non-utf8 line: {e}")))?;
                return Ok(Some(text));
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }

    pub async fn write_line(&mut self, context: Context, mut line: String) -> Result<()> {
        line.push('\n');
        // Set before the write, not after: a *partial* write still put
        // bytes on the wire, so a follow-up `Response` would corrupt the
        // stream just as badly as it would after a complete one.
        self.wrote_any = true;
        self.stream
            .write_all(line.as_bytes())
            .await
            .map_err(|e| HarnessError::io(context, None, e))
    }

    async fn read_json<T>(&mut self, context: Context) -> Result<Option<T>>
    where
        T: serde::de::DeserializeOwned,
    {
        match self.read_line(context).await? {
            None => Ok(None),
            Some(line) => serde_json::from_str(&line)
                .map(Some)
                .map_err(|e| HarnessError::json(context, None, e)),
        }
    }

    async fn write_json<T>(&mut self, context: Context, value: &T) -> Result<()>
    where
        T: serde::Serialize,
    {
        let line =
            serde_json::to_string(value).map_err(|e| HarnessError::json(context, None, e))?;
        self.write_line(context, line).await
    }

    /// Client/supervisor -> server direction (public transport: CLI ->
    /// supervisor; private transport: supervisor -> worker, forwarding
    /// an attach/prompt on the client's behalf).
    pub async fn write_request(&mut self, context: Context, request: &Request) -> Result<()> {
        self.write_json(context, request).await
    }

    /// Server side: read the one request a connection carries.
    pub async fn read_request(&mut self, context: Context) -> Result<Option<Request>> {
        self.read_json(context).await
    }

    /// Server -> client direction, terminal for non-streaming requests.
    pub async fn write_response(&mut self, context: Context, response: &Response) -> Result<()> {
        self.write_json(context, response).await
    }

    /// Relay side (supervisor reading a worker's reply to forward on).
    pub async fn read_response(&mut self, context: Context) -> Result<Option<Response>> {
        self.read_json(context).await
    }

    /// Server -> client, after `SessionAttachStarted`: one line per new
    /// transcript turn / marker / end-of-stream.
    pub async fn write_event(&mut self, context: Context, event: &SessionEvent) -> Result<()> {
        self.write_json(context, event).await
    }

    /// Relay side (supervisor reading a worker's attach stream to
    /// forward on).
    pub async fn read_event(&mut self, context: Context) -> Result<Option<SessionEvent>> {
        self.read_json(context).await
    }
}

/// Per-attempt budget for [`probe`]/[`wait_ready`]'s connect+ping+pong
/// round trip -- short, since a live server answers a bare `Ping`
/// near-instantly; a stuck attempt should fail fast so the retry loop
/// gets another chance rather than burning its whole overall timeout on
/// one doomed connection.
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// Connects to `path` and confirms something is genuinely answering by
/// sending [`Request::Ping`] and requiring [`Response::Pong`] back
/// within [`PROBE_TIMEOUT`]. A bare `connect()` succeeding is not
/// sufficient evidence: a listen endpoint can accept a connection into
/// its backlog for a brief window even after its owning process is
/// already gone, so a client that trusts `connect()` alone can hang
/// forever waiting for a reply nobody will ever send (caught by this
/// project's own `tests/supervisor_restart_recovery.rs`).
pub async fn probe(context: Context, path: PathBuf) -> bool {
    let attempt = async {
        let mut conn = connect(context, path).await?;
        conn.write_request(context, &Request::Ping).await?;
        match conn.read_response(context).await? {
            Some(Response::Pong) => Ok(()),
            Some(other) => Err(HarnessError::protocol(
                context,
                format!("expected Pong, got {other:?}"),
            )),
            None => Err(HarnessError::protocol(
                context,
                "connection closed before Pong",
            )),
        }
    };
    matches!(
        rusty_tokio::time::timeout(PROBE_TIMEOUT, attempt).await,
        Ok(Ok(()))
    )
}

/// Polls [`probe`] until it succeeds or `overall_timeout` elapses.
/// Shared by `client::daemon_start`'s supervisor readiness wait and
/// `worker::wait_ready`'s worker readiness wait -- identical shape,
/// different socket.
pub async fn wait_ready(context: Context, path: PathBuf, overall_timeout: Duration) -> Result<()> {
    let deadline = std::time::Instant::now() + overall_timeout;
    loop {
        if probe(context, path.clone()).await {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(HarnessError::conflict(
                context,
                format!("{} did not become ready in time", path.display()),
            ));
        }
        rusty_tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
