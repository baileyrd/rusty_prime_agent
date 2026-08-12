//! A minimal HTTP/1.1 client, just enough to talk to `rp-server` (the
//! `rusty_provider` sidecar, see `rp_server`) on `127.0.0.1`: health
//! checks and `POST /v1/chat/completions`. Hand-rolled rather than a
//! `reqwest`/`hyper` dependency, matching this project's own "deliberately
//! narrow" dependency floor (`ARCHITECTURE.md`) -- every call this makes
//! is a single request/response round trip to a plaintext, loopback-only
//! peer this project itself spawned, not a general-purpose HTTP client's
//! job (no TLS, no redirects, no connection reuse, no chunked encoding).
//!
//! Every request sends `Connection: close` and reads to EOF rather than
//! parsing `Content-Length`/chunked framing -- `rp-server` closing the
//! connection after one response is what actually terminates the read,
//! which is simpler and just as correct for a client that never reuses a
//! connection.

use std::time::Duration;

use rusty_tokio::io::TcpStream;

use crate::error::{Context, HarnessError, Result};

/// Per-call budget: connect, send the request, and read the full
/// response. For a health check this is generous relative to a loopback
/// round trip; for a chat completion it has to cover actual model
/// inference time -- measured against `OllamaProvider` (a tiny model,
/// CPU-only) a single completion took ~29s, uncomfortably close to a
/// smaller bound. Kept well under `client::PROMPT_RESPONSE_TIMEOUT`
/// (120s) so this call's own timeout fires first and reports "rp-server
/// did not respond" rather than the client-side wrapper timing out first
/// with a vaguer "daemon did not respond".
const REQUEST_TIMEOUT: Duration = Duration::from_secs(100);

/// `GET path` against `127.0.0.1:port`. Returns the status code and raw
/// body text.
pub async fn get(port: u16, path: &str) -> Result<(u16, String)> {
    request(port, "GET", path, None).await
}

/// `POST path` with a JSON body against `127.0.0.1:port`. Returns the
/// status code and raw body text (left unparsed -- callers that expect
/// JSON parse it themselves, since a non-2xx response's body is usually
/// a plain-text or differently-shaped error, not the success schema).
pub async fn post_json(port: u16, path: &str, body: &str) -> Result<(u16, String)> {
    request(port, "POST", path, Some(body)).await
}

async fn request(port: u16, method: &str, path: &str, body: Option<&str>) -> Result<(u16, String)> {
    let attempt = async {
        let stream = TcpStream::connect(("127.0.0.1", port))
            .await
            .map_err(|e| HarnessError::io(Context::Provider, None, e))?;

        let body = body.unwrap_or("");
        let mut request = format!(
            "{method} {path} HTTP/1.1\r\n\
             Host: 127.0.0.1:{port}\r\n\
             Connection: close\r\n\
             Content-Type: application/json\r\n\
             Content-Length: {}\r\n\r\n",
            body.len()
        );
        request.push_str(body);
        stream
            .write_all(request.as_bytes())
            .await
            .map_err(|e| HarnessError::io(Context::Provider, None, e))?;

        let mut raw = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            let n = stream
                .read(&mut chunk)
                .await
                .map_err(|e| HarnessError::io(Context::Provider, None, e))?;
            if n == 0 {
                break;
            }
            raw.extend_from_slice(&chunk[..n]);
        }
        parse_response(&raw)
    };
    rusty_tokio::time::timeout(REQUEST_TIMEOUT, attempt)
        .await
        .map_err(|_| {
            HarnessError::conflict(Context::Provider, "rp-server did not respond in time")
        })?
}

fn parse_response(raw: &[u8]) -> Result<(u16, String)> {
    let text = String::from_utf8_lossy(raw);
    let (head, body) = text.split_once("\r\n\r\n").ok_or_else(|| {
        HarnessError::protocol(
            Context::Provider,
            "malformed HTTP response: no header/body split",
        )
    })?;
    let status_line = head.lines().next().ok_or_else(|| {
        HarnessError::protocol(Context::Provider, "malformed HTTP response: empty")
    })?;
    // "HTTP/1.1 200 OK" -- the second whitespace-separated token.
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| {
            HarnessError::protocol(
                Context::Provider,
                format!("malformed HTTP status line: {status_line:?}"),
            )
        })?;
    Ok((status, body.to_string()))
}
