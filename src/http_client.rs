//! A minimal HTTP/1.1 client, just enough to talk to `rp-server` (the
//! `rusty_provider` sidecar, see `rp_server`) on `127.0.0.1`: health
//! checks, `POST /v1/chat/completions`, and (see `mcp_client`)
//! `POST /mcp`. Hand-rolled rather than a `reqwest`/`hyper` dependency,
//! matching this project's own "deliberately narrow" dependency floor
//! (`ARCHITECTURE.md`) -- every call this makes is a single
//! request/response round trip to a plaintext, loopback-only peer this
//! project itself spawned, not a general-purpose HTTP client's job (no
//! TLS, no redirects, no connection reuse).
//!
//! Every request sends `Connection: close` and reads to EOF rather than
//! parsing `Content-Length` framing -- `rp-server` closing the
//! connection after one response is what actually terminates the read,
//! which is simpler and just as correct for a client that never reuses a
//! connection. `Transfer-Encoding: chunked` responses *are* decoded
//! (`decode_chunked`), unlike `Content-Length` framing which this client
//! never needs to parse (EOF already tells it when the body is
//! complete) -- confirmed necessary by direct probing (see `mcp_client`'s
//! own doc comment): `rp-server`'s `/mcp` endpoint always answers
//! chunked, even for a single non-streaming request, so a client that
//! only understood `Content-Length` framing would misparse every MCP
//! response.

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

/// `(status, response headers as (lowercased name, value) pairs, body)`.
type HttpResponse = (u16, Vec<(String, String)>, String);

/// `GET path` against `127.0.0.1:port`. Returns the status code and raw
/// body text.
pub async fn get(port: u16, path: &str) -> Result<(u16, String)> {
    let (status, _headers, body) = request(port, "GET", path, &[], None).await?;
    Ok((status, body))
}

/// `POST path` with a JSON body against `127.0.0.1:port`. Returns the
/// status code and raw body text (left unparsed -- callers that expect
/// JSON parse it themselves, since a non-2xx response's body is usually
/// a plain-text or differently-shaped error, not the success schema).
pub async fn post_json(port: u16, path: &str, body: &str) -> Result<(u16, String)> {
    let (status, _headers, body) = request(port, "POST", path, &[], Some(body)).await?;
    Ok((status, body))
}

/// Like [`post_json`], but with extra request headers and the response
/// headers handed back too -- `mcp_client` needs both: `Accept:
/// application/json, text/event-stream` (`rp-server`'s `/mcp` endpoint
/// rejects anything else with 406) on the way out, and the
/// `Mcp-Session-Id` response header on the way back after `initialize`.
/// Response headers are returned as `(lowercased name, value)` pairs, so
/// callers can match case-insensitively without doing it themselves.
pub async fn post_json_with_headers(
    port: u16,
    path: &str,
    body: &str,
    extra_headers: &[(&str, &str)],
) -> Result<HttpResponse> {
    request(port, "POST", path, extra_headers, Some(body)).await
}

async fn request(
    port: u16,
    method: &str,
    path: &str,
    extra_headers: &[(&str, &str)],
    body: Option<&str>,
) -> Result<HttpResponse> {
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
             Content-Length: {}\r\n",
            body.len()
        );
        for (name, value) in extra_headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str("\r\n");
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

fn parse_response(raw: &[u8]) -> Result<HttpResponse> {
    let text = String::from_utf8_lossy(raw);
    let (head, body) = text.split_once("\r\n\r\n").ok_or_else(|| {
        HarnessError::protocol(
            Context::Provider,
            "malformed HTTP response: no header/body split",
        )
    })?;
    let mut lines = head.lines();
    let status_line = lines.next().ok_or_else(|| {
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
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_lowercase(), value.trim().to_string()))
        })
        .collect();

    let is_chunked = headers
        .iter()
        .any(|(name, value)| name == "transfer-encoding" && value.eq_ignore_ascii_case("chunked"));
    let body = if is_chunked {
        decode_chunked(body.as_bytes())?
    } else {
        body.to_string()
    };
    Ok((status, headers, body))
}

/// Decodes an HTTP/1.1 `Transfer-Encoding: chunked` body: a sequence of
/// `<hex length>\r\n<that many bytes>\r\n`, repeated, terminated by a
/// zero-length chunk (`0\r\n`, then a final `\r\n` -- any trailer
/// headers between them are ignored, this project's own peer never
/// sends any). Chunk-extensions after a `;` on the length line (e.g.
/// `1a;foo=bar`) are also ignored -- `rp-server`'s SSE framing never
/// sends any, and a client that doesn't understand a given extension is
/// supposed to ignore it per RFC 9112 anyway.
fn decode_chunked(body: &[u8]) -> Result<String> {
    let mut out = Vec::new();
    let mut rest = body;
    loop {
        let newline = rest
            .iter()
            .position(|&b| b == b'\n')
            .ok_or_else(|| HarnessError::protocol(Context::Provider, "truncated chunk header"))?;
        let size_line = String::from_utf8_lossy(&rest[..newline]);
        let size_hex = size_line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16).map_err(|_| {
            HarnessError::protocol(
                Context::Provider,
                format!("invalid chunk size {size_hex:?}"),
            )
        })?;
        rest = &rest[newline + 1..];
        if size == 0 {
            break;
        }
        if rest.len() < size {
            return Err(HarnessError::protocol(
                Context::Provider,
                "truncated chunk body",
            ));
        }
        out.extend_from_slice(&rest[..size]);
        rest = &rest[size..];
        // Each chunk's data is followed by a trailing `\r\n` before the
        // next chunk's size line.
        rest = rest.strip_prefix(b"\r\n").unwrap_or(rest);
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_chunked_reassembles_multiple_chunks() {
        let raw = b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        assert_eq!(decode_chunked(raw).unwrap(), "hello world");
    }

    #[test]
    fn decode_chunked_handles_a_single_empty_chunk() {
        let raw = b"0\r\n\r\n";
        assert_eq!(decode_chunked(raw).unwrap(), "");
    }

    #[test]
    fn decode_chunked_ignores_chunk_extensions() {
        let raw = b"5;foo=bar\r\nhello\r\n0\r\n\r\n";
        assert_eq!(decode_chunked(raw).unwrap(), "hello");
    }

    #[test]
    fn parse_response_decodes_a_chunked_body() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\ndata\r\n0\r\n\r\n";
        let (status, headers, body) = parse_response(raw).unwrap();
        assert_eq!(status, 200);
        assert!(headers
            .iter()
            .any(|(n, v)| n == "transfer-encoding" && v == "chunked"));
        assert_eq!(body, "data");
    }

    #[test]
    fn parse_response_leaves_a_content_length_body_untouched() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\ndata";
        let (status, _headers, body) = parse_response(raw).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, "data");
    }

    #[test]
    fn parse_response_lowercases_header_names_for_case_insensitive_lookup() {
        let raw = b"HTTP/1.1 200 OK\r\nMcp-Session-Id: abc-123\r\n\r\n";
        let (_status, headers, _body) = parse_response(raw).unwrap();
        assert_eq!(
            headers.iter().find(|(n, _)| n == "mcp-session-id"),
            Some(&("mcp-session-id".to_string(), "abc-123".to_string()))
        );
    }
}
