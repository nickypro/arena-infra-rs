//! Standard base64 (RFC 4648, padded) — how scripts and data cross the SSH boundary
//! without quoting: the deep check ships its script this way, and background jobs ship
//! their command out and their log bytes back (`jobs`). Two small functions are not worth
//! a dependency.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encode `bytes`, padded, on one line.
pub(crate) fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], chunk.get(1).copied().unwrap_or(0), chunk.get(2).copied().unwrap_or(0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Decode padded base64, as `base64` (coreutils) prints it once its line breaks are
/// removed. Strict — a character outside the alphabet, a length that isn't a multiple of
/// four or padding anywhere but the end is an error, never a best guess: what comes back
/// from a pod is checked, not trusted.
pub(crate) fn decode(text: &str) -> Result<Vec<u8>, String> {
    let s = text.as_bytes();
    if s.len() % 4 != 0 {
        return Err(format!("base64 length {} is not a multiple of 4", s.len()));
    }
    let value = |c: u8| -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some(u32::from(c - b'A')),
            b'a'..=b'z' => Some(u32::from(c - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(c - b'0') + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    };
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let quads = s.len() / 4;
    for (q, quad) in s.chunks(4).enumerate() {
        let pad = quad.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 || (pad > 0 && q + 1 != quads) {
            return Err("misplaced base64 padding".into());
        }
        let mut n = 0u32;
        for &c in &quad[..4 - pad] {
            n = (n << 6) | value(c).ok_or_else(|| format!("invalid base64 character {:?}", c as char))?;
        }
        n <<= 6 * pad as u32;
        let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        out.extend_from_slice(&bytes[..3 - pad]);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648_vectors() {
        for (input, want) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(encode(input.as_bytes()), want, "{input:?}");
            assert_eq!(decode(want).unwrap(), input.as_bytes(), "{want:?}");
        }
        assert_eq!(encode(&[0xff, 0xfe, 0x00, 0x3e, 0x3f]), "//4APj8=");
        assert_eq!(decode("//4APj8=").unwrap(), [0xff, 0xfe, 0x00, 0x3e, 0x3f]);
    }

    #[test]
    fn decode_round_trips_every_byte() {
        let all: Vec<u8> = (0..=255u8).chain((0..=255u8).rev()).collect();
        for len in 0..all.len() {
            assert_eq!(decode(&encode(&all[..len])).unwrap(), &all[..len], "len {len}");
        }
    }

    #[test]
    fn decode_refuses_what_base64_never_prints() {
        for bad in ["Zg=", "Zm9v\n", "Zm 9v", "Zg==Zm9v", "Z===", "Zm9-", "====", "Zm9vYg=a"] {
            assert!(decode(bad).is_err(), "{bad:?}");
        }
    }
}
