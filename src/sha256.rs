//! Hand-rolled SHA-256 and HMAC-SHA256 (FIPS 180-4 / RFC 2104), needed
//! only for the Jupyter wire protocol's message-signing scheme
//! (`zmtp`/`ipython_runtime`: every multipart message to/from a real
//! kernel carries an HMAC-SHA256 signature over its header/parent_header/
//! metadata/content frames, using the shared `key` from the kernel's own
//! connection file).
//!
//! Hand-rolled rather than a `sha2`/`hmac` crate dependency, for the same
//! reason `zmtp`'s own wire framing and `http_client`'s HTTP/1.1 parsing
//! are hand-rolled: this project's dependency floor is deliberately
//! narrow (`ARCHITECTURE.md`), and SHA-256 is a small, exactly-specified,
//! test-vector-verifiable algorithm -- unlike ZMTP's or HTTP's own framing,
//! there is no ambiguity in the spec to get subtly wrong, and the unit
//! tests below pin this implementation against the official FIPS 180-4 and
//! RFC 4231 test vectors, not just "it round-trips against itself."
//!
//! The HMAC key itself is *not* meant to be cryptographically strong here
//! (see `ipython_runtime::generate_key`'s own doc comment) -- this module
//! only needs to implement the signing algorithm correctly, not generate
//! secure keys.

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

const H0: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

/// SHA-256 of `data`, as raw bytes.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h = H0;

    // Padding: `0x80`, then zero bytes, then the original bit-length as a
    // big-endian u64, so the total length is a multiple of 64 bytes.
    let bit_len = (data.len() as u64) * 8;
    let mut padded = data.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in padded.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word) in chunk.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

const BLOCK_SIZE: usize = 64;

/// HMAC-SHA256(`key`, `message`), as raw bytes. `message` is passed as
/// multiple parts (rather than one pre-concatenated buffer) since every
/// caller here signs a Jupyter message's four JSON frames
/// (header/parent_header/metadata/content) without wanting to allocate a
/// combined buffer just to call this.
pub fn hmac_sha256_parts(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut key_block = [0u8; BLOCK_SIZE];
    if key.len() > BLOCK_SIZE {
        let hashed = sha256(key);
        key_block[..32].copy_from_slice(&hashed);
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }

    let mut ipad = [0u8; BLOCK_SIZE];
    let mut opad = [0u8; BLOCK_SIZE];
    for i in 0..BLOCK_SIZE {
        ipad[i] = key_block[i] ^ 0x36;
        opad[i] = key_block[i] ^ 0x5c;
    }

    let mut inner_input =
        Vec::with_capacity(BLOCK_SIZE + parts.iter().map(|p| p.len()).sum::<usize>());
    inner_input.extend_from_slice(&ipad);
    for part in parts {
        inner_input.extend_from_slice(part);
    }
    let inner_hash = sha256(&inner_input);

    let mut outer_input = Vec::with_capacity(BLOCK_SIZE + 32);
    outer_input.extend_from_slice(&opad);
    outer_input.extend_from_slice(&inner_hash);
    sha256(&outer_input)
}

/// Lowercase hex-digest form -- the shape Jupyter's own wire protocol
/// signs with (the signature frame is ASCII hex, not raw bytes).
pub fn hmac_sha256_hex(key: &[u8], parts: &[&[u8]]) -> String {
    hmac_sha256_parts(key, parts)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// FIPS 180-4 / NIST test vector.
    #[test]
    fn sha256_of_empty_string() {
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    /// FIPS 180-4 / NIST test vector.
    #[test]
    fn sha256_of_abc() {
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    /// FIPS 180-4 / NIST test vector -- exercises the multi-chunk path
    /// (56 bytes of input needs two 64-byte padded chunks).
    #[test]
    fn sha256_of_two_block_message() {
        let input = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
        assert_eq!(
            hex(&sha256(input)),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    /// RFC 4231 test case 1.
    #[test]
    fn hmac_sha256_rfc4231_case1() {
        let key = [0x0bu8; 20];
        let data = b"Hi There";
        assert_eq!(
            hmac_sha256_hex(&key, &[data]),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    /// RFC 4231 test case 2 -- a key shorter than the block size and
    /// ASCII data, the shape a real, short Jupyter connection-file key
    /// most resembles.
    #[test]
    fn hmac_sha256_rfc4231_case2() {
        let key = b"Jefe";
        let data = b"what do ya want for nothing?";
        assert_eq!(
            hmac_sha256_hex(key, &[data]),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    /// RFC 4231 test case 6: a key *longer* than the block size (exercises
    /// the "hash the key first" branch).
    #[test]
    fn hmac_sha256_rfc4231_case6_long_key() {
        let key = [0xaau8; 131];
        let data = b"Test Using Larger Than Block-Size Key - Hash Key First";
        assert_eq!(
            hmac_sha256_hex(&key, &[data]),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    /// Signing over several parts must equal signing over their
    /// concatenation -- this is the exact shape `zmtp`/`ipython_runtime`
    /// depend on (signing header/parent_header/metadata/content as
    /// separate frames without concatenating them first).
    #[test]
    fn hmac_sha256_parts_matches_concatenation() {
        let key = b"shared-secret";
        let whole = hmac_sha256_hex(key, &[b"hello world, this is one buffer"]);
        let split = hmac_sha256_hex(key, &[b"hello world, ", b"this is one buffer"]);
        assert_eq!(whole, split);
    }
}
