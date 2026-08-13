//! The real `ToolRuntime` backend: a spawned, real Jupyter/IPython kernel
//! subprocess (`python3 -m ipykernel_launcher`), driven over its own ZMTP
//! wire protocol (`zmtp`) with HMAC-SHA256-signed messages (`sha256`) --
//! Phase 2 of the boundary `tool_runtime::ToolRuntime`'s own doc comment
//! describes.
//!
//! Scope, confirmed against a real, locally-installed `ipykernel` via raw-
//! socket probing before this module was written (see this project's PR
//! history for Increment 5): `shell` (DEALER), `iopub` (SUB), and `control`
//! (DEALER) are opened -- `stdin` (kernel-side `input()` prompts) and
//! `heartbeat` remain out of scope, matching the plan's own explicit
//! deferrals ("kernel restart-on-crash, rich display data, multi-kernel
//! pooling"). `control` was added specifically to carry `comm_open`/
//! `comm_msg` host-request replies without deadlocking a running `shell`
//! cell (`rlm-runtime.md`'s own stated reason for using a second channel:
//! `shell` processes messages serially, so a reply sent there for a
//! request that originated *during* an in-flight `execute_request` would
//! never be read until that same request finishes) -- confirmed
//! independently by raw-socket probing that the kernel's `control` port
//! answers with the exact same 6-frame `<IDS|MSG>` shape `shell` does, so
//! [`send_shell`](IpythonKernelRuntime::send_shell)/
//! [`recv_shell`](IpythonKernelRuntime::recv_shell)'s signing/framing logic
//! is shared via [`build_signed_message`] rather than duplicated.
//! [`shutdown`](IpythonKernelRuntime::shutdown) now attempts a graceful
//! `shutdown_request` over `control` first (bounded, best-effort --
//! confirmed by the same raw-socket probing that the kernel replies
//! `{"status":"ok","restart":false}` and exits on its own), falling back
//! to the same plain process kill regardless of whether that reply
//! arrived -- harmless against a kernel that already exited gracefully,
//! the same tolerance `rp_server::shutdown`'s own `procutil::kill` call
//! has.
//!
//! The kernel subprocess is *not* detached the way `rp_server`/
//! `worker::spawn` detach their children: it's meant to live and die with
//! exactly the one `AgentSession`/worker process that owns it (one kernel
//! per session, not a shared sidecar), so leaving it an ordinary,
//! non-detached child is correct, not an oversight -- and gets a free
//! safety net for free, confirmed in the same probing: `ipykernel`'s own
//! parent-poller self-terminates the kernel if its immediate parent (this
//! worker process) dies without a clean [`shutdown`](IpythonKernelRuntime::shutdown)
//! call, the same "worker crash" case this project's recovery path
//! already has to tolerate elsewhere. This module still explicitly kills
//! the kernel on a clean [`shutdown`](IpythonKernelRuntime::shutdown)
//! rather than relying on that self-termination, and still reaps the
//! child via the same fire-and-forget `wait()` task `rp_server::
//! ensure_running`/`worker::spawn` use, to avoid zombifying it under a
//! still-running worker (see those functions' own doc comments for why
//! that reaping is necessary at all, not just tidy).

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusty_tokio::process::{Command, Stdio};

use crate::error::{Context, HarnessError, Result};
use crate::procutil;
use crate::sha256::{hmac_sha256_hex, sha256};
use crate::tool_runtime::{BoxFuture, ExecutionOutcome, HostRequest, ToolRuntime};
use crate::zmtp::ZmtpSocket;

/// How long [`start`](IpythonKernelRuntime::start) waits for the kernel
/// process to come up and answer a `kernel_info_request` before giving
/// up. Generous: a cold Python/`ipykernel` interpreter start is much
/// slower than `rp_server::wait_for_health`'s HTTP polling.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

/// How long one [`execute`](IpythonKernelRuntime::execute) call waits for
/// the iopub `status: idle` that marks a turn complete. User code this
/// project's caller supplies could legitimately run for a while; bounded
/// here mainly so a kernel that's wedged (or user code in an infinite
/// loop -- interrupt is explicitly out of v1 scope, see this module's own
/// doc comment) fails the call instead of hanging the whole `prompt()`
/// turn forever.
const EXECUTE_TIMEOUT: Duration = Duration::from_secs(120);

/// How long [`start`](IpythonKernelRuntime::start) waits for the iopub
/// SUB socket's subscription to take effect (confirmed by the kernel's
/// own unsolicited `iopub_welcome` reply to a fresh subscription) before
/// giving up and proceeding anyway -- a missed welcome just means the
/// very first `execute`'s earliest iopub messages could theoretically
/// race the subscription, not a fatal condition worth failing startup
/// over.
const IOPUB_WELCOME_TIMEOUT: Duration = Duration::from_secs(3);

fn ipython_bin() -> std::ffi::OsString {
    std::env::var_os("RUSTY_PRIME_AGENT_IPYTHON_BIN").unwrap_or_else(|| {
        if cfg!(windows) {
            "python".into()
        } else {
            "python3".into()
        }
    })
}

#[derive(Debug, Clone, serde::Serialize)]
struct ConnectionFile {
    shell_port: u16,
    iopub_port: u16,
    stdin_port: u16,
    control_port: u16,
    hb_port: u16,
    ip: String,
    key: String,
    transport: String,
    signature_scheme: String,
    kernel_name: String,
}

/// State carried between an `execute`/`resume_execute` call that paused
/// on a host request and the `resume_execute` call that continues it --
/// `IpythonKernelRuntime` itself is the natural place to hold this
/// (rather than threading it back and forth through the caller) since
/// the caller only ever has the `HostRequest` it needs to reply to, not
/// the kernel-protocol bookkeeping (which shell message this all belongs
/// to, what `result` has already been seen) required to keep draining
/// correctly afterward.
struct PendingExecution {
    request_msg_id: String,
    /// `execute_result`/`error` content seen *before* the pause, if any
    /// -- carried through so a result that arrived earlier in the same
    /// cell isn't lost if a later `await host_request(...)` in that same
    /// cell never produces one of its own.
    result_so_far: Option<String>,
}

/// A real IPython kernel subprocess, driven over hand-rolled ZMTP -- see
/// this module's own doc comment.
pub struct IpythonKernelRuntime {
    session_dir: PathBuf,
    key: String,
    jupyter_session: String,
    kernel_pid: Option<u32>,
    shell: Option<ZmtpSocket>,
    iopub: Option<ZmtpSocket>,
    control: Option<ZmtpSocket>,
    next_msg_seq: u64,
    pending: Option<PendingExecution>,
}

impl IpythonKernelRuntime {
    /// `session_dir`: where the connection file and the kernel's own
    /// stdout/stderr log are written -- the same directory
    /// `transcript.jsonl`/`worker.log` already live in for this session,
    /// so nothing here needs a state-root lookup of its own.
    pub fn new(session_dir: PathBuf) -> Self {
        IpythonKernelRuntime {
            session_dir,
            key: generate_key_hex(),
            jupyter_session: new_message_id("session"),
            kernel_pid: None,
            shell: None,
            iopub: None,
            control: None,
            next_msg_seq: 0,
            pending: None,
        }
    }

    fn next_msg_id(&mut self) -> String {
        self.next_msg_seq += 1;
        format!(
            "msg-{}-{}-{}",
            std::process::id(),
            self.next_msg_seq,
            self.jupyter_session
        )
    }

    /// Builds and sends one signed Jupyter message on the `shell` socket,
    /// returning its `msg_id` -- `execute` keys its iopub loop's
    /// `parent_header.msg_id` check off this, to tell traffic caused by
    /// *its own* request apart from unrelated kernel-generated iopub
    /// traffic (see that loop's own inline comment).
    async fn send_shell(&mut self, msg_type: &str, content: serde_json::Value) -> Result<String> {
        let msg_id = self.next_msg_id();
        let frames = build_signed_message(
            &self.key,
            &self.jupyter_session,
            &msg_id,
            msg_type,
            &content,
        )?;
        let shell = self.shell.as_mut().ok_or_else(|| {
            HarnessError::conflict(Context::Runtime, "kernel shell socket not connected")
        })?;
        shell
            .send_multipart(&frames.iter().map(Vec::as_slice).collect::<Vec<_>>())
            .await?;
        Ok(msg_id)
    }

    /// Same as [`send_shell`](Self::send_shell), on the `control` socket
    /// instead -- see this module's own doc comment for why `control`
    /// exists at all (avoiding a `shell`-channel deadlock on host-request
    /// replies).
    async fn send_control(&mut self, msg_type: &str, content: serde_json::Value) -> Result<String> {
        let msg_id = self.next_msg_id();
        let frames = build_signed_message(
            &self.key,
            &self.jupyter_session,
            &msg_id,
            msg_type,
            &content,
        )?;
        let control = self.control.as_mut().ok_or_else(|| {
            HarnessError::conflict(Context::Runtime, "kernel control socket not connected")
        })?;
        control
            .send_multipart(&frames.iter().map(Vec::as_slice).collect::<Vec<_>>())
            .await?;
        Ok(msg_id)
    }

    /// Reads one Jupyter message off `shell`, unpacking it into
    /// `(msg_type, content)`. `shell`'s peer is a DEALER-facing ROUTER
    /// (see `zmtp`'s own doc comment on why no client-side identity
    /// framing is needed), so every reply is exactly the 6-frame
    /// `<IDS|MSG>`-delimited shape with no leading topic frame -- unlike
    /// `recv_iopub`, which does search for the delimiter (iopub messages
    /// carry a leading topic frame `shell` replies never do), this stays
    /// consistent with that same delimiter search anyway rather than
    /// hard-coding frame indices, since it costs nothing and one shared
    /// parsing routine is less to keep in sync.
    async fn recv_shell(&mut self) -> Result<(String, serde_json::Value, serde_json::Value)> {
        let shell = self.shell.as_mut().ok_or_else(|| {
            HarnessError::conflict(Context::Runtime, "kernel shell socket not connected")
        })?;
        let frames = shell.recv_multipart().await?;
        parse_jupyter_message(&frames)
    }

    /// Reads one Jupyter message off `control` -- same framing as
    /// `recv_shell`, confirmed by direct raw-socket probing against a real
    /// `ipykernel` before this module opened the socket (see this
    /// module's own doc comment).
    async fn recv_control(&mut self) -> Result<(String, serde_json::Value, serde_json::Value)> {
        let control = self.control.as_mut().ok_or_else(|| {
            HarnessError::conflict(Context::Runtime, "kernel control socket not connected")
        })?;
        let frames = control.recv_multipart().await?;
        parse_jupyter_message(&frames)
    }

    /// Reads one Jupyter message off `iopub`.
    async fn recv_iopub(&mut self) -> Result<(String, serde_json::Value, serde_json::Value)> {
        let iopub = self.iopub.as_mut().ok_or_else(|| {
            HarnessError::conflict(Context::Runtime, "kernel iopub socket not connected")
        })?;
        let frames = iopub.recv_multipart().await?;
        parse_jupyter_message(&frames)
    }

    /// Reads `iopub` until either `execute_request` `request_msg_id`
    /// completes (`status: idle`, returns `Ok(None)`) or the kernel opens
    /// a `host.request`-targeted comm and is now blocked awaiting a reply
    /// (returns `Ok(Some(HostRequest))`) -- the shared drain loop behind
    /// both [`execute`](ToolRuntime::execute) and
    /// [`resume_execute`](ToolRuntime::resume_execute), which differ only
    /// in how they arrive at a `request_msg_id` to drain and an initial
    /// `(stdout, result)` to accumulate into. `comm_open` is confirmed (by
    /// direct raw-socket probing before this was written, see
    /// `ipython_runtime`'s own module doc comment) to broadcast on
    /// `iopub` the same way `stream`/`execute_result` do, carrying the
    /// same `parent_header.msg_id` as the cell that opened it -- so this
    /// reuses the existing `parent_header` filter rather than needing a
    /// separate one, on the same v1 assumption every other branch here
    /// already makes: `host_request(...)` is always awaited synchronously
    /// within the cell that opened it, not fired off from a background
    /// task the way `rlm-runtime.md` warns a real Jupyter client must
    /// also tolerate.
    async fn drain_until_idle_or_host_request(
        &mut self,
        request_msg_id: &str,
        stdout: &mut String,
        result: &mut Option<String>,
    ) -> Result<Option<HostRequest>> {
        loop {
            let (msg_type, parent_header, content) = self.recv_iopub().await?;
            if parent_header.get("msg_id").and_then(|v| v.as_str()) != Some(request_msg_id) {
                continue;
            }
            match msg_type.as_str() {
                "stream" => {
                    let name = content
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("stdout");
                    let text = content.get("text").and_then(|v| v.as_str()).unwrap_or("");
                    if name == "stderr" {
                        stdout.push_str("[stderr] ");
                    }
                    stdout.push_str(text);
                }
                "execute_result" => {
                    *result = content
                        .get("data")
                        .and_then(|d| d.get("text/plain"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                }
                "error" => {
                    let ename = content
                        .get("ename")
                        .and_then(|v| v.as_str())
                        .unwrap_or("Error");
                    let evalue = content.get("evalue").and_then(|v| v.as_str()).unwrap_or("");
                    *result = Some(format!("{ename}: {evalue}"));
                }
                "comm_open"
                    if content.get("target_name").and_then(|v| v.as_str())
                        == Some("host.request") =>
                {
                    let comm_id = content
                        .get("comm_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let payload = content
                        .get("data")
                        .cloned()
                        .unwrap_or(serde_json::json!({}));
                    let kind = payload
                        .get("kind")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    return Ok(Some(HostRequest {
                        comm_id,
                        kind,
                        payload,
                    }));
                }
                "status"
                    if content.get("execution_state").and_then(|v| v.as_str()) == Some("idle") =>
                {
                    return Ok(None);
                }
                _ => {}
            }
        }
    }

    /// Runs [`drain_until_idle_or_host_request`](Self::drain_until_idle_or_host_request)
    /// under `EXECUTE_TIMEOUT`, then either drains the matching
    /// `execute_reply` off `shell` and returns a finished
    /// `ExecutionOutcome` (same "keep `recv_shell` from seeing stale
    /// replies" reasoning the original single-call `execute` always had),
    /// or stashes `self.pending` so a later `resume_execute` can continue
    /// exactly where this call paused.
    async fn drive_execution(
        &mut self,
        request_msg_id: String,
        mut stdout: String,
        mut result: Option<String>,
    ) -> Result<ExecutionOutcome> {
        let pending = rusty_tokio::time::timeout(
            EXECUTE_TIMEOUT,
            self.drain_until_idle_or_host_request(&request_msg_id, &mut stdout, &mut result),
        )
        .await
        .map_err(|_| {
            HarnessError::conflict(Context::Runtime, "kernel execute_request timed out")
        })??;

        match pending {
            None => {
                // Best-effort: idle on iopub always follows execute_reply
                // on shell in practice (confirmed by direct probing), so
                // this should already be sitting in the socket buffer.
                let _ = rusty_tokio::time::timeout(Duration::from_secs(5), self.recv_shell()).await;
                Ok(ExecutionOutcome {
                    stdout,
                    result,
                    pending_host_request: None,
                })
            }
            Some(host_request) => {
                self.pending = Some(PendingExecution {
                    request_msg_id,
                    result_so_far: result.clone(),
                });
                Ok(ExecutionOutcome {
                    stdout,
                    result,
                    pending_host_request: Some(host_request),
                })
            }
        }
    }
}

impl ToolRuntime for IpythonKernelRuntime {
    fn start(&mut self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            paths_ensure_dir(&self.session_dir)?;

            let shell_port = pick_free_port()?;
            let iopub_port = pick_free_port()?;
            let stdin_port = pick_free_port()?;
            let control_port = pick_free_port()?;
            let hb_port = pick_free_port()?;

            let connection = ConnectionFile {
                shell_port,
                iopub_port,
                stdin_port,
                control_port,
                hb_port,
                ip: "127.0.0.1".to_string(),
                key: self.key.clone(),
                transport: "tcp".to_string(),
                signature_scheme: "hmac-sha256".to_string(),
                kernel_name: "python3".to_string(),
            };
            let connection_path = self.session_dir.join("ipython-connection.json");
            let json = serde_json::to_string_pretty(&connection).map_err(|e| {
                HarnessError::json(Context::Runtime, Some(connection_path.clone()), e)
            })?;
            std::fs::write(&connection_path, json).map_err(|e| {
                HarnessError::io(Context::Runtime, Some(connection_path.clone()), e)
            })?;

            let log_path = self.session_dir.join("ipython.log");
            let log_file = std::fs::File::create(&log_path)
                .map_err(|e| HarnessError::io(Context::Runtime, Some(log_path), e))?;

            let mut cmd = Command::new(ipython_bin());
            cmd.arg("-m")
                .arg("ipykernel_launcher")
                .arg("-f")
                .arg(&connection_path);
            cmd.stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::from(log_file));
            // Deliberately *not* `procutil::prepare_detached` -- see this
            // module's own doc comment for why staying an ordinary child
            // of this worker process is correct here, not an oversight.
            let mut child = cmd.spawn().map_err(|e| {
                HarnessError::io(
                    Context::Runtime,
                    Some(std::path::PathBuf::from(ipython_bin())),
                    e,
                )
            })?;
            let pid = child.id();
            self.kernel_pid = Some(pid);
            // Same zombie-avoidance reasoning as `rp_server::ensure_running`/
            // `worker::spawn`'s own reaper tasks.
            rusty_tokio::spawn(async move {
                let _ = child.wait().await;
            });

            // The kernel process needs a moment after spawn before its
            // sockets are actually listening -- retry the connect (not
            // just the ZMTP handshake) until `STARTUP_TIMEOUT` elapses,
            // the same "poll until ready" shape as `rp_server::
            // wait_for_health`.
            let deadline = std::time::Instant::now() + STARTUP_TIMEOUT;
            let shell = loop {
                match ZmtpSocket::connect("127.0.0.1", shell_port, "DEALER").await {
                    Ok(shell) => break shell,
                    Err(err) => {
                        if std::time::Instant::now() >= deadline {
                            return Err(err);
                        }
                        rusty_tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            };
            self.shell = Some(shell);

            let mut iopub = ZmtpSocket::connect("127.0.0.1", iopub_port, "SUB").await?;
            iopub.subscribe_all().await?;
            // Best-effort: the kernel answers a fresh SUB subscription
            // with an unsolicited `iopub_welcome` message -- reading it
            // confirms the subscription is actually live before this
            // runtime's first `execute` sends anything, without resorting
            // to a fixed sleep (see this module's own const doc comment).
            let _ = rusty_tokio::time::timeout(IOPUB_WELCOME_TIMEOUT, iopub.recv_multipart()).await;
            self.iopub = Some(iopub);

            // By this point `shell`'s own connect-retry loop above already
            // proved the kernel process is fully up, so a single attempt
            // (no retry loop) is enough here.
            let control = ZmtpSocket::connect("127.0.0.1", control_port, "DEALER").await?;
            self.control = Some(control);

            let (msg_type, _parent_header, _content) =
                rusty_tokio::time::timeout(STARTUP_TIMEOUT, async {
                    self.send_shell("kernel_info_request", serde_json::json!({}))
                        .await?;
                    self.recv_shell().await
                })
                .await
                .map_err(|_| {
                    HarnessError::conflict(Context::Runtime, "kernel_info_request timed out")
                })??;
            if msg_type != "kernel_info_reply" {
                return Err(HarnessError::protocol(
                    Context::Runtime,
                    format!("expected kernel_info_reply, got {msg_type}"),
                ));
            }
            Ok(())
        })
    }

    fn execute(&mut self, code: &str) -> BoxFuture<'_, Result<ExecutionOutcome>> {
        let code = code.to_string();
        Box::pin(async move {
            let request_msg_id = self
                .send_shell(
                    "execute_request",
                    serde_json::json!({
                        "code": code,
                        "silent": false,
                        "store_history": true,
                        "user_expressions": {},
                        "allow_stdin": false,
                        "stop_on_error": true,
                    }),
                )
                .await?;
            self.drive_execution(request_msg_id, String::new(), None)
                .await
        })
    }

    fn resume_execute(
        &mut self,
        comm_id: &str,
        reply: serde_json::Value,
    ) -> BoxFuture<'_, Result<ExecutionOutcome>> {
        let comm_id = comm_id.to_string();
        Box::pin(async move {
            self.send_control(
                "comm_msg",
                serde_json::json!({"comm_id": comm_id, "data": reply}),
            )
            .await?;
            let pending = self.pending.take().ok_or_else(|| {
                HarnessError::conflict(
                    Context::Runtime,
                    "resume_execute called with no pending host request",
                )
            })?;
            self.drive_execution(pending.request_msg_id, String::new(), pending.result_so_far)
                .await
        })
    }

    fn shutdown(&mut self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            if let Some(pid) = self.kernel_pid.take() {
                // Best-effort graceful shutdown first, over `control` (the
                // channel this exists to carry, per this module's own doc
                // comment) -- confirmed by direct raw-socket probing
                // against a real `ipykernel` before this was written: the
                // kernel replies `{"status":"ok","restart":false}` and
                // then exits on its own. Bounded and best-effort: a wedged
                // or already-gone kernel must not hang this call, and
                // `procutil::kill` right after is always still run
                // regardless of whether the graceful reply arrived --
                // harmless against a kernel that already exited on its
                // own, the same tolerance `rp_server::shutdown`'s own
                // `procutil::kill` call has.
                if self.control.is_some() {
                    let graceful = rusty_tokio::time::timeout(
                        Duration::from_secs(3),
                        self.send_control(
                            "shutdown_request",
                            serde_json::json!({"restart": false}),
                        ),
                    )
                    .await;
                    if graceful.is_ok() {
                        let _ =
                            rusty_tokio::time::timeout(Duration::from_secs(3), self.recv_control())
                                .await;
                    }
                }
                let _ = procutil::kill(pid);
            }
            self.shell = None;
            self.iopub = None;
            self.control = None;
            Ok(())
        })
    }
}

/// Builds the 6 signed frames of one Jupyter message
/// (`<IDS|MSG>`/signature/header/parent_header/metadata/content) --
/// shared by [`IpythonKernelRuntime::send_shell`] and
/// [`IpythonKernelRuntime::send_control`] rather than duplicated, since
/// both channels use identical framing (confirmed by direct raw-socket
/// probing against a real `ipykernel`, see this module's own doc
/// comment). `parent_header`/`metadata` are always the empty object: this
/// module only ever originates *requests* (never replies to a message it
/// received), so there is never a real parent to attach.
fn build_signed_message(
    key: &str,
    session: &str,
    msg_id: &str,
    msg_type: &str,
    content: &serde_json::Value,
) -> Result<Vec<Vec<u8>>> {
    let header = serde_json::json!({
        "msg_id": msg_id,
        "msg_type": msg_type,
        "username": "rusty-prime-agent",
        "session": session,
        "date": iso8601_now(),
        "version": "5.3",
    });
    let header_b =
        serde_json::to_vec(&header).map_err(|e| HarnessError::json(Context::Runtime, None, e))?;
    let parent_b = b"{}".to_vec();
    let meta_b = b"{}".to_vec();
    let content_b =
        serde_json::to_vec(content).map_err(|e| HarnessError::json(Context::Runtime, None, e))?;
    let sig = hmac_sha256_hex(key.as_bytes(), &[&header_b, &parent_b, &meta_b, &content_b]);
    Ok(vec![
        b"<IDS|MSG>".to_vec(),
        sig.into_bytes(),
        header_b,
        parent_b,
        meta_b,
        content_b,
    ])
}

fn paths_ensure_dir(dir: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(dir)
        .map_err(|e| HarnessError::io(Context::Runtime, Some(dir.to_path_buf()), e))
}

/// Same "ask the OS for a free ephemeral port" allocation as
/// `rp_server`'s own `pick_free_port` (bind `:0`, read it back, drop the
/// listener before the kernel binds it itself) -- not shared with that
/// module's copy since this one tags its own failures `Context::Runtime`
/// rather than `Context::Provider`, and the logic itself is five lines.
fn pick_free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .map_err(|e| HarnessError::io(Context::Runtime, None, e))?;
    listener
        .local_addr()
        .map(|addr| addr.port())
        .map_err(|e| HarnessError::io(Context::Runtime, None, e))
}

/// A local, non-cryptographic HMAC key -- adequate here for the same
/// reason `session::new_session_id`'s own non-cryptographic id generation
/// is: this key's only job is pairing this process with a kernel
/// subprocess it itself just spawned on loopback, and the connection
/// file's on-disk permissions (this session's own state directory,
/// already private to this project's single-local-user trust model) are
/// the real security boundary here, exactly as they are for a real
/// Jupyter installation's own connection files. No `rand` dependency
/// needed: mixing a nanosecond timestamp, this process's pid, and a
/// per-call sequence-like salt through `sha256` gives more than enough
/// non-collision for a value that only ever needs to be unpredictable to
/// a process that *isn't* this one and its own freshly-spawned child.
fn generate_key_hex() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seed = format!(
        "rusty-prime-agent-ipython-key-{nanos:x}-{}",
        std::process::id()
    );
    sha256(seed.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Same shape/uniqueness reasoning as `session::new_session_id` -- a
/// Jupyter `session` (and message) id has no cryptographic-randomness
/// requirement of its own; the wire protocol only needs it to be a
/// unique-enough string, not an RFC-4122-conformant UUID.
fn new_message_id(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{prefix}-{nanos:x}-{}", std::process::id())
}

/// Finds the `<IDS|MSG>` delimiter frame (iopub messages carry zero or
/// more leading topic frames before it; shell replies never do -- see
/// `recv_shell`'s own doc comment) and unpacks `(msg_type, parent_header,
/// content)` from the four JSON frames that follow the signature.
/// `parent_header` is what lets `execute`'s iopub loop tell a status/
/// stream/result message caused by *its own* `execute_request` apart from
/// unrelated kernel traffic (most notably the kernel's own unsolicited
/// `busy`/`idle` pair right after boot, which has no bearing on any
/// request this client has sent -- see `execute`'s own doc comment for
/// the bug this caused before `parent_header` was checked). Signature
/// verification of inbound messages is deliberately skipped: this is a
/// kernel subprocess this process itself just spawned over loopback, the
/// same trust boundary `session_autonomous --quality-gate`'s own
/// unsandboxed shell execution already accepts (see that command's own
/// doc comment) -- there is no third party positioned to forge a message
/// on a loopback socket this process alone holds both ends of, so
/// verifying would only catch this project's own signing bugs, which the
/// `sha256`/`zmtp` unit tests plus this module's own real-kernel
/// integration test already cover more directly.
fn parse_jupyter_message(
    frames: &[Vec<u8>],
) -> Result<(String, serde_json::Value, serde_json::Value)> {
    let idx = frames
        .iter()
        .position(|f| f.as_slice() == b"<IDS|MSG>")
        .ok_or_else(|| HarnessError::protocol(Context::Runtime, "missing <IDS|MSG> delimiter"))?;
    let header = frames.get(idx + 2).ok_or_else(|| {
        HarnessError::protocol(
            Context::Runtime,
            "truncated Jupyter message: missing header",
        )
    })?;
    let parent_header = frames.get(idx + 3).ok_or_else(|| {
        HarnessError::protocol(
            Context::Runtime,
            "truncated Jupyter message: missing parent_header",
        )
    })?;
    let content = frames.get(idx + 5).ok_or_else(|| {
        HarnessError::protocol(
            Context::Runtime,
            "truncated Jupyter message: missing content",
        )
    })?;
    let header: serde_json::Value = serde_json::from_slice(header)
        .map_err(|e| HarnessError::json(Context::Runtime, None, e))?;
    let parent_header: serde_json::Value = serde_json::from_slice(parent_header)
        .map_err(|e| HarnessError::json(Context::Runtime, None, e))?;
    let content: serde_json::Value = serde_json::from_slice(content)
        .map_err(|e| HarnessError::json(Context::Runtime, None, e))?;
    let msg_type = header
        .get("msg_type")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Ok((msg_type, parent_header, content))
}

/// `YYYY-MM-DDTHH:MM:SS.ffffffZ` for the current time -- Jupyter's own
/// header `date` field convention. Implemented directly against
/// `SystemTime` (Howard Hinnant's `civil_from_days` calendar algorithm,
/// public domain / CC0) rather than pulling in `chrono` for one field
/// that (per direct testing against a real kernel) is never actually
/// parsed strictly by the kernel side -- correctness here is about not
/// sending kernel a value it *could* choke on, not about this project
/// needing calendar arithmetic anywhere else.
fn iso8601_now() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    let secs = now.as_secs() as i64;
    let micros = now.subsec_micros();
    let days = secs.div_euclid(86400);
    let secs_of_day = secs.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    let hh = secs_of_day / 3600;
    let mm = (secs_of_day % 3600) / 60;
    let ss = secs_of_day % 60;
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}.{micros:06}Z")
}

/// Days-since-Unix-epoch -> `(year, month, day)`. Port of Howard
/// Hinnant's `civil_from_days` (chrono-compatible calendar algorithm --
/// see e.g. <http://howardhinnant.github.io/date_algorithms.html>).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = mp + if mp < 10 { 3 } else { -9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Inverse of [`civil_from_days`] -- `(year, month, day)` ->
/// days-since-Unix-epoch. Only used by this module's own unit tests, to
/// round-trip-check [`civil_from_days`] against a known-correct inverse
/// rather than hand-verifying epoch-day numbers.
#[cfg(test)]
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_epoch_is_1970_01_01() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
    }

    #[test]
    fn civil_from_days_day_one_is_1970_01_02() {
        assert_eq!(civil_from_days(1), (1970, 1, 2));
    }

    #[test]
    fn days_from_civil_inverts_civil_from_days_at_epoch() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
    }

    #[test]
    fn civil_days_round_trip_across_a_wide_range() {
        for days in (-20000..20000).step_by(137) {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(
                days_from_civil(y, m, d),
                days,
                "round-trip failed for day {days}"
            );
        }
    }

    #[test]
    fn civil_from_days_handles_a_leap_day() {
        // 2024-02-29 is a real leap day; round-tripping it must land back
        // on the same date, not silently roll over into March.
        let days = days_from_civil(2024, 2, 29);
        assert_eq!(civil_from_days(days), (2024, 2, 29));
    }

    #[test]
    fn iso8601_now_has_the_expected_shape() {
        let s = iso8601_now();
        assert_eq!(s.len(), "YYYY-MM-DDTHH:MM:SS.ffffffZ".len());
        assert!(s.ends_with('Z'));
        assert_eq!(s.as_bytes()[4], b'-');
        assert_eq!(s.as_bytes()[7], b'-');
        assert_eq!(s.as_bytes()[10], b'T');
    }

    #[test]
    fn parse_jupyter_message_finds_delimiter_with_leading_topic_frames() {
        let frames: Vec<Vec<u8>> = vec![
            b"some.topic".to_vec(),
            b"<IDS|MSG>".to_vec(),
            b"deadbeef".to_vec(),
            br#"{"msg_type":"status"}"#.to_vec(),
            br#"{"msg_id":"abc-123"}"#.to_vec(),
            b"{}".to_vec(),
            br#"{"execution_state":"idle"}"#.to_vec(),
        ];
        let (msg_type, parent_header, content) = parse_jupyter_message(&frames).unwrap();
        assert_eq!(msg_type, "status");
        assert_eq!(parent_header["msg_id"], "abc-123");
        assert_eq!(content["execution_state"], "idle");
    }

    #[test]
    fn parse_jupyter_message_works_with_no_leading_topic_frame() {
        let frames: Vec<Vec<u8>> = vec![
            b"<IDS|MSG>".to_vec(),
            b"deadbeef".to_vec(),
            br#"{"msg_type":"kernel_info_reply"}"#.to_vec(),
            b"{}".to_vec(),
            b"{}".to_vec(),
            br#"{"status":"ok"}"#.to_vec(),
        ];
        let (msg_type, parent_header, content) = parse_jupyter_message(&frames).unwrap();
        assert_eq!(msg_type, "kernel_info_reply");
        assert_eq!(parent_header, serde_json::json!({}));
        assert_eq!(content["status"], "ok");
    }

    #[test]
    fn generate_key_hex_produces_a_non_empty_hex_string() {
        let key = generate_key_hex();
        assert_eq!(key.len(), 64);
        assert!(key.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// Real end-to-end coverage against a genuine `ipykernel` subprocess
    /// -- deliberately `#[ignore]`d for the same infra reasons as
    /// `tests/ollama_provider.rs`'s own real-model tests: this needs a
    /// real `python3` with `ipykernel` installed (`pip install
    /// ipykernel`), neither of which this project's own CI provisions.
    /// Unlike a full CLI/model round trip (which would also need a real
    /// Ollama setup to get a model to actually *decide* to call
    /// `execute_python` -- small models are not reliable tool-callers,
    /// see `tests/ollama_provider.rs`'s own tool-call test comments),
    /// this drives `IpythonKernelRuntime` directly, so it's a
    /// deterministic proof of the kernel wire protocol itself (spawn,
    /// handshake, execute, capture stdout/result, shutdown) independent
    /// of any model's tool-calling behavior. Run explicitly:
    ///
    /// ```sh
    /// cargo test --bin harness ipython_runtime::tests::real_kernel -- --ignored
    /// ```
    #[rusty_tokio::test]
    #[ignore]
    async fn real_kernel_executes_code_and_reports_stdout_and_result() {
        let dir = std::env::temp_dir().join(format!(
            "rpa-ipython-kernel-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp session dir");

        let mut runtime = IpythonKernelRuntime::new(dir.clone());
        runtime
            .start()
            .await
            .expect("kernel should spawn and complete the kernel_info handshake");

        let outcome = runtime
            .execute("print('hello from a real kernel')\n6 * 7")
            .await
            .expect("execute_request should round-trip");
        assert!(
            outcome.stdout.contains("hello from a real kernel"),
            "expected stdout to contain the print() output, got: {:?}",
            outcome.stdout
        );
        assert_eq!(outcome.result.as_deref(), Some("42"));

        // Kernel state (variables) persists across calls within the same
        // session -- the whole point of a persistent kernel over a
        // one-shot subprocess-per-call design.
        let outcome2 = runtime
            .execute("x = 10\nx + 5")
            .await
            .expect("execute_request should round-trip");
        assert_eq!(outcome2.result.as_deref(), Some("15"));

        let outcome3 = runtime
            .execute("x * 2")
            .await
            .expect("execute_request should round-trip");
        assert_eq!(
            outcome3.result.as_deref(),
            Some("20"),
            "kernel state (the `x` variable) should persist across execute calls"
        );

        // A Python-level exception should come back as a normal
        // (non-Err) `ExecutionOutcome`, not a plumbing failure -- the
        // model is meant to see it and recover, the same as any other
        // tool error.
        let error_outcome = runtime
            .execute("raise ValueError('boom')")
            .await
            .expect("a Python exception must not surface as a HarnessError");
        assert!(
            error_outcome
                .result
                .as_deref()
                .unwrap_or("")
                .contains("ValueError"),
            "expected the error result to mention ValueError, got: {:?}",
            error_outcome.result
        );

        runtime.shutdown().await.expect("shutdown should succeed");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Real end-to-end coverage that the `control` channel connected in
    /// `start()` is actually live and speaks the same framing `shell`
    /// does -- confirmed independently by raw-socket probing before this
    /// module opened the socket (see this module's own doc comment), this
    /// test proves the same thing through the actual `send_control`/
    /// `recv_control` methods rather than a standalone probe script.
    /// `#[ignore]`d for the same real-`ipykernel`-install reason as its
    /// siblings.
    #[rusty_tokio::test]
    #[ignore]
    async fn real_kernel_control_channel_round_trips_kernel_info_request() {
        let dir = std::env::temp_dir().join(format!(
            "rpa-ipython-control-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp session dir");

        let mut runtime = IpythonKernelRuntime::new(dir.clone());
        runtime
            .start()
            .await
            .expect("kernel should spawn and complete the kernel_info handshake");

        let request_msg_id = runtime
            .send_control("kernel_info_request", serde_json::json!({}))
            .await
            .expect("control channel should accept kernel_info_request");
        let (msg_type, parent_header, content) =
            rusty_tokio::time::timeout(Duration::from_secs(10), runtime.recv_control())
                .await
                .expect("control channel should reply within the timeout")
                .expect("control channel reply should parse as a Jupyter message");

        assert_eq!(msg_type, "kernel_info_reply");
        assert_eq!(
            parent_header.get("msg_id").and_then(|v| v.as_str()),
            Some(request_msg_id.as_str())
        );
        assert_eq!(content.get("status").and_then(|v| v.as_str()), Some("ok"));

        runtime.shutdown().await.expect("shutdown should succeed");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Real end-to-end coverage of the whole `host.request` comm
    /// protocol: defines `host_request` exactly the way `worker::
    /// bootstrap_kernel` does (including the `control_handlers`
    /// monkeypatch confirmed necessary by direct raw-socket probing
    /// before this was written -- stock `ipykernel` never routes
    /// `comm_msg` through `control` on its own), then proves the full
    /// round trip through the actual `execute`/`resume_execute` methods
    /// rather than a standalone probe script: `execute` pauses on
    /// `pending_host_request` when the kernel opens the comm and blocks
    /// awaiting a reply, and `resume_execute` both delivers that reply
    /// and lets the cell finish, producing the value the awaited call
    /// returned. `#[ignore]`d for the same real-`ipykernel`-install
    /// reason as its siblings.
    #[rusty_tokio::test]
    #[ignore]
    async fn real_kernel_host_request_round_trips_through_execute_and_resume() {
        let session_dir = std::env::temp_dir().join(format!(
            "rpa-ipython-hostreq-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&session_dir).expect("create temp session dir");

        let mut runtime = IpythonKernelRuntime::new(session_dir.clone());
        runtime
            .start()
            .await
            .expect("kernel should spawn and complete the kernel_info handshake");

        // Exactly the code `worker::bootstrap_kernel` sends for
        // `host_request` (minus the `rlm_heartbeat` definition, which
        // this test isn't exercising).
        let setup_code = "\
import asyncio
from ipykernel.comm import Comm
_host_request_kernel = get_ipython().kernel
_host_request_kernel.control_handlers['comm_msg'] = _host_request_kernel.comm_manager.comm_msg
_host_request_kernel.control_handlers['comm_close'] = _host_request_kernel.comm_manager.comm_close
_host_request_loop = asyncio.get_event_loop()
def host_request(kind, payload=None):
    comm = Comm(target_name='host.request', data={'kind': kind, **(payload or {})})
    fut = _host_request_loop.create_future()
    def _on_msg(msg):
        def _resolve():
            if not fut.done():
                fut.set_result(msg['content']['data'])
        _host_request_loop.call_soon_threadsafe(_resolve)
    comm.on_msg(_on_msg)
    return fut
";
        let setup_outcome = runtime
            .execute(setup_code)
            .await
            .expect("host_request setup code should round-trip");
        assert!(
            setup_outcome.pending_host_request.is_none(),
            "setup code alone must not open any comm"
        );

        let outcome = runtime
            .execute("result = await host_request('ping', {'x': 1})\nresult")
            .await
            .expect("the await-cell should round-trip up to the pause");
        let pending = outcome
            .pending_host_request
            .expect("the kernel should be paused awaiting a host-request reply");
        assert_eq!(pending.kind, "ping");
        assert_eq!(pending.payload.get("x").and_then(|v| v.as_i64()), Some(1));
        assert!(
            !pending.comm_id.is_empty(),
            "expected a non-empty comm_id to reply to"
        );

        // A single-key payload, deliberately: `serde_json::json!`'s
        // default `Value::Object` is a `BTreeMap` (alphabetically
        // ordered, not insertion-ordered), so a real reply's Python
        // `repr()` on the far side would come back key-sorted rather
        // than in the order this literal was written -- a single key
        // sidesteps that rather than asserting against a specific
        // ordering this project's own JSON layer doesn't promise.
        let resumed = runtime
            .resume_execute(&pending.comm_id, serde_json::json!({"echo": "pong"}))
            .await
            .expect("resume_execute should deliver the reply and let the cell finish");
        assert!(
            resumed.pending_host_request.is_none(),
            "the cell should have finished after exactly one host request"
        );
        assert_eq!(
            resumed.result.as_deref(),
            Some("{'echo': 'pong'}"),
            "the awaited call should have returned the exact reply this test sent"
        );

        runtime.shutdown().await.expect("shutdown should succeed");

        let _ = std::fs::remove_dir_all(&session_dir);
    }

    /// Real end-to-end coverage for `skills.rs`'s whole point: a real
    /// Python package on disk becomes `import`-able inside a real kernel
    /// once its parent directory is added to `sys.path` -- the exact
    /// operation `worker::run`'s `install_skills` performs. Deliberately
    /// drives `sys.path.insert`/`import`/the function call directly
    /// (rather than going through a real model's own tool-calling
    /// decision) for the same determinism reason
    /// `real_kernel_executes_code_and_reports_stdout_and_result` does --
    /// small-model tool-call reliability is a separate, already-documented
    /// caveat elsewhere in this project's test suite, not something this
    /// test needs to also depend on. `#[ignore]`d for the same real-
    /// `ipykernel`-install reason as its sibling above.
    #[rusty_tokio::test]
    #[ignore]
    async fn real_kernel_imports_a_skill_package_after_sys_path_insert() {
        let session_dir = std::env::temp_dir().join(format!(
            "rpa-ipython-skill-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&session_dir).expect("create temp session dir");

        // A real, minimal Python package -- the same shape `skills::
        // discover` expects (`SKILL.md` is irrelevant to Python itself;
        // only `__init__.py` matters for the import to work).
        let skills_dir = session_dir.join("skills");
        let package_dir = skills_dir.join("doubler");
        std::fs::create_dir_all(&package_dir).expect("create skill package dir");
        std::fs::write(
            package_dir.join("__init__.py"),
            "def double(x):\n    return x * 2\n",
        )
        .expect("write __init__.py");

        let mut runtime = IpythonKernelRuntime::new(session_dir.clone());
        runtime
            .start()
            .await
            .expect("kernel should spawn and complete the kernel_info handshake");

        let skills_dir_json = serde_json::to_string(&skills_dir.display().to_string()).unwrap();
        runtime
            .execute(&format!(
                "import sys; sys.path.insert(0, {skills_dir_json})"
            ))
            .await
            .expect("sys.path.insert should round-trip");

        let outcome = runtime
            .execute("import doubler\ndoubler.double(21)")
            .await
            .expect("import + call should round-trip");
        assert_eq!(
            outcome.result.as_deref(),
            Some("42"),
            "expected the real skill package's function to run, got: {outcome:?}"
        );

        runtime.shutdown().await.expect("shutdown should succeed");

        let _ = std::fs::remove_dir_all(&session_dir);
    }

    /// Real end-to-end coverage for the kernel-side half of `/heartbeat`'s
    /// sibling, `rlm_heartbeat()`: defines it exactly the way `worker::
    /// bootstrap_kernel` does, calls it, and confirms the marker
    /// `session::HEARTBEAT_MARKER` watches for actually shows up in
    /// `execute()`'s stdout. Deliberately does not go through
    /// `worker::run`/a real daemon (that needs a real model to decide to
    /// call `execute_python` in the first place -- small-model tool-call
    /// reliability is a separate, already-documented caveat elsewhere in
    /// this project's test suite) -- this is the same "prove the
    /// mechanism directly and deterministically" reasoning as this
    /// module's other real-kernel tests. `#[ignore]`d for the same real-
    /// `ipykernel`-install reason as its siblings.
    #[rusty_tokio::test]
    #[ignore]
    async fn real_kernel_rlm_heartbeat_prints_the_marker() {
        let session_dir = std::env::temp_dir().join(format!(
            "rpa-ipython-heartbeat-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&session_dir).expect("create temp session dir");

        let mut runtime = IpythonKernelRuntime::new(session_dir.clone());
        runtime
            .start()
            .await
            .expect("kernel should spawn and complete the kernel_info handshake");

        // Exactly the code `worker::bootstrap_kernel` sends.
        let marker = crate::session::HEARTBEAT_MARKER;
        let define_code = format!(
            "def rlm_heartbeat(every=None):\n    print({marker:?} + (every or \"\"))\n    return \"heartbeat requested\"\n"
        );
        runtime
            .execute(&define_code)
            .await
            .expect("defining rlm_heartbeat should round-trip");

        let outcome = runtime
            .execute("rlm_heartbeat()")
            .await
            .expect("calling rlm_heartbeat should round-trip");
        assert!(
            outcome.stdout.contains(marker),
            "expected the heartbeat marker in stdout, got: {:?}",
            outcome.stdout
        );
        assert_eq!(outcome.result.as_deref(), Some("'heartbeat requested'"));

        // The `every` argument (parity with `prime-agent`'s
        // `rlm_heartbeat.create(interval=...)`) rides along after the
        // marker on the same printed line -- see `session::
        // extract_heartbeat_marker`'s own doc comment.
        let outcome = runtime
            .execute("rlm_heartbeat(every=\"10m\")")
            .await
            .expect("calling rlm_heartbeat(every=...) should round-trip");
        assert!(
            outcome.stdout.contains(&format!("{marker}10m")),
            "expected the marker followed by the every argument, got: {:?}",
            outcome.stdout
        );

        runtime.shutdown().await.expect("shutdown should succeed");

        let _ = std::fs::remove_dir_all(&session_dir);
    }
}
