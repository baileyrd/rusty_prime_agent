//! A minimal ZMTP 3.0 client (NULL security mechanism only), just enough
//! to talk to a real Jupyter/IPython kernel's `shell` (DEALER) and
//! `iopub` (SUB) sockets -- see `ipython_runtime`.
//!
//! Hand-rolled rather than the `zeromq` crate: that crate depends
//! directly on real `tokio` (`cargo tree` against `zeromq = "0.4"`
//! confirms `tokio` v1.53 and `tokio-util` pulled in unconditionally --
//! its `async-std-runtime` feature swaps to a *different* foreign runtime,
//! not a fix), which would mean two competing async runtimes in one
//! process -- exactly the anti-pattern `rp_server`'s own doc comment
//! explains why `rp-server` runs as a separate OS process rather than
//! being linked in as a library. Hand-rolling the wire protocol directly
//! on `rusty_tokio::io::TcpStream` avoids that entirely, and is no bigger
//! an undertaking than `http_client`'s own hand-rolled HTTP/1.1 client --
//! arguably smaller, since ZMTP's framing is fixed-width binary with no
//! text header parsing.
//!
//! The subset implemented here was confirmed byte-exact against a real,
//! running `ipykernel` kernel via raw-socket probes before being ported to
//! Rust (greeting handshake, `READY` command exchange, DEALER request/
//! reply, SUB subscribe-and-receive) -- see this project's PR history for
//! Increment 5. Only what a `shell`+`iopub` client needs is implemented:
//! the NULL mechanism (no CURVE/PLAIN -- a kernel this process itself
//! spawned on loopback has no need for either), and DEALER/SUB socket
//! semantics. `stdin`/`control`/`heartbeat` sockets are out of scope for
//! v1 (see `ipython_runtime`'s own doc comment).

use rusty_tokio::io::TcpStream;

use crate::error::{Context, HarnessError, Result};

const GREETING_LEN: usize = 64;

/// One connected ZMTP peer, past the greeting/`READY` handshake.
pub struct ZmtpSocket {
    stream: TcpStream,
}

impl ZmtpSocket {
    /// Connects to `host:port` and performs the ZMTP 3.0 NULL-mechanism
    /// greeting plus the `READY` command exchange, declaring this end's
    /// socket type as `socket_type` (`"DEALER"` for `shell`, `"SUB"` for
    /// `iopub`). Does not itself send a SUB subscription -- see
    /// [`subscribe_all`](Self::subscribe_all).
    pub async fn connect(host: &str, port: u16, socket_type: &str) -> Result<Self> {
        let mut stream = TcpStream::connect((host, port))
            .await
            .map_err(|e| HarnessError::io(Context::Runtime, None, e))?;

        stream
            .write_all(&greeting())
            .await
            .map_err(|e| HarnessError::io(Context::Runtime, None, e))?;
        let their_greeting = read_exact(&mut stream, GREETING_LEN).await?;
        if their_greeting.len() != GREETING_LEN
            || their_greeting[0] != 0xff
            || their_greeting[9] != 0x7f
        {
            return Err(HarnessError::protocol(
                Context::Runtime,
                "malformed ZMTP greeting from kernel",
            ));
        }

        stream
            .write_all(&ready_command(socket_type))
            .await
            .map_err(|e| HarnessError::io(Context::Runtime, None, e))?;
        // The kernel's own READY command -- its exact contents (its
        // socket type, e.g. ROUTER for our DEALER) aren't needed for
        // anything this client does, only that the handshake completed.
        let _their_ready = read_frame(&mut stream).await?;

        Ok(Self { stream })
    }

    /// Subscribes to every topic on a SUB socket (empty prefix matches
    /// everything) -- required once, right after `connect`, before a SUB
    /// socket's peer will forward it any traffic at all.
    pub async fn subscribe_all(&mut self) -> Result<()> {
        self.send_multipart(&[&[0x01]]).await
    }

    /// Sends `parts` as one ZMTP multipart message (every frame but the
    /// last carries the MORE flag).
    pub async fn send_multipart(&mut self, parts: &[&[u8]]) -> Result<()> {
        let mut out = Vec::new();
        for (i, part) in parts.iter().enumerate() {
            out.extend(encode_frame(part, i + 1 != parts.len()));
        }
        self.stream
            .write_all(&out)
            .await
            .map_err(|e| HarnessError::io(Context::Runtime, None, e))
    }

    /// Reads one full multipart message (every frame up to and including
    /// the first one with MORE unset).
    pub async fn recv_multipart(&mut self) -> Result<Vec<Vec<u8>>> {
        let mut frames = Vec::new();
        loop {
            let (body, more) = read_frame(&mut self.stream).await?;
            frames.push(body);
            if !more {
                break;
            }
        }
        Ok(frames)
    }
}

/// The 64-byte ZMTP 3.0 greeting: 10-byte signature, 2-byte version
/// (3.0), 20-byte NUL-padded ASCII mechanism name, 1-byte as-server flag,
/// 31 filler bytes.
fn greeting() -> [u8; GREETING_LEN] {
    let mut g = [0u8; GREETING_LEN];
    g[0] = 0xff;
    g[9] = 0x7f;
    g[10] = 0x03; // version major
    g[11] = 0x00; // version minor
    g[12..16].copy_from_slice(b"NULL"); // mechanism, NUL-padded to 20 bytes
                                        // g[16..32] already zero (NUL padding); g[32] (as-server) already 0;
                                        // g[33..64] filler already zero.
    g
}

/// A ZMTP `READY` command declaring `Socket-Type: <socket_type>` -- the
/// command frame body is `len(name) + name + (1-byte prop-name-length +
/// prop-name + 4-byte-BE prop-value-length + prop-value)*`; only the
/// `Socket-Type` property is sent, matching what a real kernel's own
/// `READY` reply from `libzmq` sends back (plus an `Identity` property
/// this client never needs to set).
fn ready_command(socket_type: &str) -> Vec<u8> {
    let mut prop = Vec::new();
    prop.push(b"Socket-Type".len() as u8);
    prop.extend_from_slice(b"Socket-Type");
    prop.extend_from_slice(&(socket_type.len() as u32).to_be_bytes());
    prop.extend_from_slice(socket_type.as_bytes());

    let mut body = Vec::new();
    body.push(b"READY".len() as u8);
    body.extend_from_slice(b"READY");
    body.extend_from_slice(&prop);

    // COMMAND flag (0x04), short-length form -- `body` here is always a
    // few dozen bytes, never near the 256-byte short-form cutoff.
    let mut frame = Vec::with_capacity(body.len() + 2);
    frame.push(0x04);
    frame.push(body.len() as u8);
    frame.extend_from_slice(&body);
    frame
}

/// Encodes one ZMTP frame: 1 flag byte (bit0 = MORE, bit1 = LONG), then
/// either a 1-byte or (if `body.len() >= 256`) 8-byte big-endian length,
/// then the body.
fn encode_frame(body: &[u8], more: bool) -> Vec<u8> {
    let mut flag: u8 = if more { 0x01 } else { 0x00 };
    let mut frame = Vec::with_capacity(body.len() + 9);
    if body.len() < 256 {
        frame.push(flag);
        frame.push(body.len() as u8);
    } else {
        flag |= 0x02;
        frame.push(flag);
        frame.extend_from_slice(&(body.len() as u64).to_be_bytes());
    }
    frame.extend_from_slice(body);
    frame
}

/// Reads one ZMTP frame (flag byte + length + body), returning its body
/// and whether MORE was set. Works for both COMMAND and ordinary traffic
/// frames -- the framing header shape is identical either way, only the
/// COMMAND bit's meaning to the caller differs.
async fn read_frame(stream: &mut TcpStream) -> Result<(Vec<u8>, bool)> {
    let flag = read_exact(stream, 1).await?[0];
    let more = flag & 0x01 != 0;
    let long = flag & 0x02 != 0;
    let length = if long {
        let len_bytes = read_exact(stream, 8).await?;
        u64::from_be_bytes(len_bytes.try_into().expect("read_exact(8) returns 8 bytes")) as usize
    } else {
        read_exact(stream, 1).await?[0] as usize
    };
    let body = read_exact(stream, length).await?;
    Ok((body, more))
}

/// Reads exactly `n` bytes, looping over short reads -- `TcpStream::read`
/// only ever fills up to the slice it's given (never more), so repeatedly
/// reading into the unfilled remainder is sufficient; no internal
/// leftover-byte buffering is needed the way a byte-stream framing
/// decoder that reads ahead would require.
async fn read_exact(stream: &mut TcpStream, n: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    let mut filled = 0;
    while filled < n {
        let read = stream
            .read(&mut buf[filled..])
            .await
            .map_err(|e| HarnessError::io(Context::Runtime, None, e))?;
        if read == 0 {
            return Err(HarnessError::protocol(
                Context::Runtime,
                "kernel connection closed mid-frame",
            ));
        }
        filled += read;
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_frame_short_form_sets_more_bit() {
        let frame = encode_frame(b"hi", true);
        assert_eq!(frame, vec![0x01, 2, b'h', b'i']);
    }

    #[test]
    fn encode_frame_short_form_clears_more_bit_on_last_frame() {
        let frame = encode_frame(b"hi", false);
        assert_eq!(frame, vec![0x00, 2, b'h', b'i']);
    }

    #[test]
    fn encode_frame_long_form_for_bodies_at_or_over_256_bytes() {
        let body = vec![0x42u8; 256];
        let frame = encode_frame(&body, false);
        assert_eq!(frame[0], 0x02); // LONG bit set, MORE clear
        assert_eq!(&frame[1..9], &256u64.to_be_bytes());
        assert_eq!(&frame[9..], body.as_slice());
    }

    #[test]
    fn greeting_matches_zmtp_3_0_null_mechanism_layout() {
        let g = greeting();
        assert_eq!(g.len(), 64);
        assert_eq!(g[0], 0xff);
        assert_eq!(g[9], 0x7f);
        assert_eq!(&g[10..12], &[0x03, 0x00]);
        assert_eq!(&g[12..16], b"NULL");
        assert!(g[16..32].iter().all(|&b| b == 0));
    }

    #[test]
    fn ready_command_declares_socket_type() {
        let cmd = ready_command("DEALER");
        // flag byte, length byte, then body.
        assert_eq!(cmd[0], 0x04);
        let body = &cmd[2..];
        assert_eq!(body[0], 5); // len("READY")
        assert_eq!(&body[1..6], b"READY");
        assert_eq!(body[6], b"Socket-Type".len() as u8);
        assert_eq!(&body[7..18], b"Socket-Type");
        assert_eq!(&body[18..22], &6u32.to_be_bytes()); // len("DEALER")
        assert_eq!(&body[22..28], b"DEALER");
    }
}
