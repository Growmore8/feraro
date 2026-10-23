//! Time-based one-time passwords, RFC 6238.
//!
//! # Why this is written out rather than pulled in
//!
//! TOTP is HMAC-SHA1 over a counter, truncated to six digits. All of it is
//! below, in about two hundred lines, and that is cheaper than the thing it
//! replaces: a dependency on the authentication path is a dependency whose
//! every future version has to be read, because a subtle change in it is a
//! change to who can sign in. The engine next door holds a zero-dependency
//! line for the same reason, and this is the one part of the gateway where
//! the same argument applies.
//!
//! SHA-1 is the algorithm here not because it is a good hash — it is not,
//! and nothing else in this platform uses it — but because every
//! authenticator app assumes it. A secret that only works in software
//! nobody has is a secret nobody uses. HMAC does not depend on collision
//! resistance, which is the property SHA-1 has lost, so the construction
//! is sound even though the hash is old.
//!
//! # What this module does not do
//!
//! It does not remember anything. Replay — the same code used twice inside
//! its thirty seconds — is the caller's to prevent, because the caller is
//! the thing with a user store to write the last accepted step into. See
//! [`Totp::check`].

use std::fmt::Write as _;

/// How long one code lasts, in seconds. Thirty is what every app assumes.
pub const STEP: u64 = 30;

/// How many digits a code has. Six, for the same reason.
pub const DIGITS: u32 = 6;

/// How many steps either side of now are accepted.
///
/// One, which is a sixty-second window in each direction. A phone whose
/// clock has drifted a little still works; a code from five minutes ago
/// does not.
pub const DRIFT: u64 = 1;

/// Bytes in a fresh secret. Twenty is the RFC's recommendation for SHA-1
/// and what authenticator apps expect.
pub const SECRET_BYTES: usize = 20;

// ── SHA-1 ──────────────────────────────────────────────────────────────

const H0: [u32; 5] = [
    0x6745_2301,
    0xEFCD_AB89,
    0x98BA_DCFE,
    0x1032_5476,
    0xC3D2_E1F0,
];

/// SHA-1 of one message.
fn sha1(message: &[u8]) -> [u8; 20] {
    let mut h = H0;

    // Padding: a one bit, zeros, then the length in bits as a big-endian
    // u64, to a multiple of sixty-four bytes.
    let mut padded = message.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    let bits = (message.len() as u64).wrapping_mul(8);
    padded.extend_from_slice(&bits.to_be_bytes());

    for block in padded.chunks(64) {
        let mut w = [0u32; 80];
        for (i, word) in block.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }

        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, word) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }

    let mut out = [0u8; 20];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// HMAC-SHA1, RFC 2104.
fn hmac_sha1(key: &[u8], message: &[u8]) -> [u8; 20] {
    const BLOCK: usize = 64;

    // A key longer than the block is hashed first; a shorter one is padded
    // with zeros. Both are the RFC's rule, and getting the first wrong
    // silently weakens every long secret.
    let mut shortened = [0u8; BLOCK];
    if key.len() > BLOCK {
        shortened[..20].copy_from_slice(&sha1(key));
    } else {
        shortened[..key.len()].copy_from_slice(key);
    }

    let mut inner = Vec::with_capacity(BLOCK + message.len());
    let mut outer = Vec::with_capacity(BLOCK + 20);
    for byte in shortened {
        inner.push(byte ^ 0x36);
        outer.push(byte ^ 0x5C);
    }
    inner.extend_from_slice(message);
    outer.extend_from_slice(&sha1(&inner));
    sha1(&outer)
}

// ── Base32, RFC 4648 ───────────────────────────────────────────────────

const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// Encodes bytes as unpadded base32, which is what authenticator apps read.
fn base32(bytes: &[u8]) -> String {
    let mut out = String::new();
    let mut buffer: u32 = 0;
    let mut bits = 0;
    for byte in bytes {
        buffer = (buffer << 8) | u32::from(*byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((buffer >> bits) & 0x1F) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((buffer << (5 - bits)) & 0x1F) as usize] as char);
    }
    out
}

/// Decodes unpadded base32, ignoring spaces and case.
///
/// Spaces because a secret shown to a person is shown in groups of four,
/// and somebody retyping it will keep them.
fn unbase32(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut buffer: u32 = 0;
    let mut bits = 0;
    for c in text.chars() {
        if c == ' ' || c == '-' || c == '=' {
            continue;
        }
        let up = c.to_ascii_uppercase() as u8;
        let value = ALPHABET.iter().position(|a| *a == up)? as u32;
        buffer = (buffer << 5) | value;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push(((buffer >> bits) & 0xFF) as u8);
        }
    }
    (!out.is_empty()).then_some(out)
}

// ── The secret ─────────────────────────────────────────────────────────

/// One login's authenticator secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Totp {
    secret: Vec<u8>,
}

impl Totp {
    /// A fresh random secret.
    ///
    /// Fails only if the operating system will not give random bytes, which
    /// is a condition to refuse under rather than work around: a predictable
    /// second factor is worse than none, because it is trusted.
    pub fn fresh() -> Result<Self, getrandom::Error> {
        let mut secret = vec![0u8; SECRET_BYTES];
        getrandom::getrandom(&mut secret)?;
        Ok(Totp { secret })
    }

    /// Reads one back from its stored base32 form.
    pub fn parse(text: &str) -> Option<Self> {
        unbase32(text).map(|secret| Totp { secret })
    }

    /// The base32 form, which is what is stored and what is typed in.
    pub fn to_base32(&self) -> String {
        base32(&self.secret)
    }

    /// The same, in groups of four, for reading off a screen.
    pub fn to_grouped(&self) -> String {
        let flat = self.to_base32();
        let mut out = String::new();
        for (i, c) in flat.chars().enumerate() {
            if i > 0 && i % 4 == 0 {
                out.push(' ');
            }
            out.push(c);
        }
        out
    }

    /// The `otpauth:` URI an authenticator app scans.
    ///
    /// `issuer` appears twice — once as a label prefix and once as a
    /// parameter — because apps disagree about which they read, and an
    /// entry called "admin@…" with no platform beside it is an entry
    /// nobody can place.
    pub fn uri(&self, issuer: &str, login: &str) -> String {
        let label = format!("{}:{}", escape(issuer), escape(login));
        let mut uri = format!("otpauth://totp/{label}?secret={}", self.to_base32());
        let _ = write!(uri, "&issuer={}", escape(issuer));
        let _ = write!(uri, "&algorithm=SHA1&digits={DIGITS}&period={STEP}");
        uri
    }

    /// The code for one time step.
    pub fn code_at(&self, step: u64) -> String {
        let mac = hmac_sha1(&self.secret, &step.to_be_bytes());
        // Dynamic truncation: the low nibble of the last byte picks where to
        // read four bytes from, and the top bit is masked off so the result
        // is the same on every platform's signed arithmetic.
        let offset = (mac[19] & 0x0F) as usize;
        let binary = u32::from_be_bytes([
            mac[offset] & 0x7F,
            mac[offset + 1],
            mac[offset + 2],
            mac[offset + 3],
        ]);
        let modulus = 10u32.pow(DIGITS);
        format!("{:0width$}", binary % modulus, width = DIGITS as usize)
    }

    /// Which step a unix time falls in.
    pub const fn step_of(unix_seconds: u64) -> u64 {
        unix_seconds / STEP
    }

    /// Checks a typed code, and says which step it was.
    ///
    /// # Why it returns the step
    ///
    /// A code is valid for thirty seconds, so somebody who reads one over a
    /// shoulder — or off a screen share — can use it before it expires.
    /// Returning the step lets the caller record it and refuse anything at
    /// or below it next time, which turns a one-time password into one that
    /// is actually used once. A module that only answered yes or no could
    /// not offer that, and every caller would get it wrong.
    ///
    /// The comparison is constant-time in the digits, so a wrong code does
    /// not leak how many of its digits were right.
    pub fn check(&self, typed: &str, now_unix: u64) -> Option<u64> {
        let typed: String = typed.chars().filter(|c| c.is_ascii_digit()).collect();
        if typed.len() != DIGITS as usize {
            return None;
        }
        let here = Self::step_of(now_unix);
        // Oldest first, so a code that matches two steps — which cannot
        // happen with a real secret, but would with a degenerate one —
        // records the later step and leaves less room for replay.
        let first = here.saturating_sub(DRIFT);
        (first..=here.saturating_add(DRIFT)).find(|&step| same(&self.code_at(step), &typed))
    }
}

/// Compares two strings without returning early.
fn same(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.bytes().zip(b.bytes()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Percent-encodes what an `otpauth:` label may not carry.
fn escape(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn sha1_matches_the_published_vectors() {
        // The three from FIPS 180-1 and RFC 3174. If these are wrong,
        // everything above them is wrong in a way no TOTP test would name.
        assert_eq!(
            hex(&sha1(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(hex(&sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(
            hex(&sha1(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
        // A message that lands exactly on a padding boundary, which is where
        // a hand-written padding rule goes wrong.
        assert_eq!(
            hex(&sha1(&[b'a'; 55])),
            "c1c8bbdc22796e28c0e15163d20899b65621d65a"
        );
        assert_eq!(
            hex(&sha1(&[b'a'; 56])),
            "c2db330f6083854c99d4b5bfb6e8f29f201be699"
        );
    }

    #[test]
    fn hmac_sha1_matches_rfc_2202() {
        assert_eq!(
            hex(&hmac_sha1(&[0x0b; 20], b"Hi There")),
            "b617318655057264e28bc0b6fb378c8ef146be00"
        );
        assert_eq!(
            hex(&hmac_sha1(b"Jefe", b"what do ya want for nothing?")),
            "effcdf6ae5eb2fa2d27416d5f184df9c259a7c79"
        );
        // A key longer than the block, which must be hashed first. Getting
        // this branch wrong silently weakens every long secret.
        assert_eq!(
            hex(&hmac_sha1(
                &[0xaa; 80],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "aa4ae5e15272d00e95705637ce8a3b55ed402112"
        );
    }

    #[test]
    fn codes_match_the_rfc_6238_vectors() {
        // RFC 6238's SHA-1 table, whose secret is the ASCII "12345678901234567890".
        let totp = Totp {
            secret: b"12345678901234567890".to_vec(),
        };
        for (unix, expected) in [
            (59u64, "287082"),
            (1_111_111_109, "081804"),
            (1_111_111_111, "050471"),
            (1_234_567_890, "005924"),
            (2_000_000_000, "279037"),
            (20_000_000_000, "353130"),
        ] {
            assert_eq!(totp.code_at(Totp::step_of(unix)), expected, "at {unix}");
        }
    }

    #[test]
    fn base32_round_trips_and_reads_what_a_person_retypes() {
        let totp = Totp::fresh().unwrap();
        let flat = totp.to_base32();
        assert_eq!(Totp::parse(&flat).unwrap(), totp);
        // In groups, lower case, with the spaces a person would keep.
        assert_eq!(
            Totp::parse(&totp.to_grouped().to_ascii_lowercase()).unwrap(),
            totp
        );
        // The known vector, so the alphabet itself is pinned.
        assert_eq!(
            Totp {
                secret: b"12345678901234567890".to_vec()
            }
            .to_base32(),
            "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ"
        );
    }

    #[test]
    fn a_code_is_accepted_a_minute_either_side_and_not_beyond() {
        let totp = Totp::fresh().unwrap();
        let now = 1_700_000_000u64;
        let code = totp.code_at(Totp::step_of(now));

        assert_eq!(totp.check(&code, now), Some(Totp::step_of(now)));
        // A phone whose clock has drifted a little still works.
        assert!(totp.check(&code, now - STEP).is_some());
        assert!(totp.check(&code, now + STEP).is_some());
        // Two minutes is not drift, it is an old code.
        assert!(totp.check(&code, now + STEP * 3).is_none());
        assert!(totp.check(&code, now - STEP * 3).is_none());
    }

    #[test]
    fn nothing_that_is_not_six_digits_is_a_code() {
        let totp = Totp::fresh().unwrap();
        let now = 1_700_000_000u64;
        for not_a_code in ["", "12345", "1234567", "abcdef", "   ", "12 34 5"] {
            assert!(totp.check(not_a_code, now).is_none(), "{not_a_code:?}");
        }
        // Spaces inside a real code are forgiven, because a person reading
        // one off a phone will type them.
        let code = totp.code_at(Totp::step_of(now));
        let spaced = format!("{} {}", &code[..3], &code[3..]);
        assert!(totp.check(&spaced, now).is_some());
    }

    #[test]
    fn two_secrets_do_not_accept_each_others_codes() {
        let a = Totp::fresh().unwrap();
        let b = Totp::fresh().unwrap();
        let now = 1_700_000_000u64;
        assert!(b.check(&a.code_at(Totp::step_of(now)), now).is_none());
    }

    #[test]
    fn the_uri_names_the_platform_and_carries_the_secret() {
        let totp = Totp {
            secret: b"12345678901234567890".to_vec(),
        };
        let uri = totp.uri("Feraro", "admin@feraro.local");
        assert!(
            uri.starts_with("otpauth://totp/Feraro:admin%40feraro.local?"),
            "{uri}"
        );
        assert!(
            uri.contains("secret=GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ"),
            "{uri}"
        );
        // Both, because apps disagree about which they read.
        assert!(uri.contains("issuer=Feraro"), "{uri}");
        assert!(uri.contains("digits=6"), "{uri}");
        assert!(uri.contains("period=30"), "{uri}");
    }

    #[test]
    fn a_secret_that_is_not_base32_is_not_a_secret() {
        assert!(Totp::parse("").is_none());
        assert!(Totp::parse("!!!!").is_none());
        // 0, 1 and 8 are not in the alphabet, which is the point of it.
        assert!(Totp::parse("ABCD0180").is_none());
    }
}
