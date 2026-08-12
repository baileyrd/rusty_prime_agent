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
    /// Binds and starts listening at `path`, retrying on `AddrInUse`
    /// for up to `timeout`. Stale leftover socket file reclaim (a
    /// listener that died without unlinking it) is handled underneath
    /// by `rusty_tokio`/`rustils` via a probe `connect()` -- see
    /// `rusty_tokio::io::UnixListener::bind`'s own doc -- but that probe
    /// can itself transiently see a listen backlog as still "live" for
    /// a brief window right after its owning process was force-killed,
    /// before the OS finishes tearing it down (the same race
    /// [`probe`]'s own doc comment describes on the client-connect
    /// side). Observed in this project's own
    /// `tests/supervisor_restart_recovery.rs`: a supervisor started
    /// immediately after force-killing the previous one could fail to
    /// reclaim its predecessor's still-warm socket file on the first
    /// try. A short retry window is the fix -- the file reliably reads
    /// as stale a moment later, once the OS has actually finished
    /// releasing it.
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
}

impl LineStream {
    fn new(stream: UnixStream) -> Self {
        LineStream {
            stream,
            buf: Vec::new(),
        }
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
