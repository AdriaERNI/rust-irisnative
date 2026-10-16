//! Credential obfuscation and OS-user discovery.
//!
//! Truth note (probe-settled, 2026-10-15, `probes/refclient.py` against IRIS
//! 2025.3 protocol 69): the connect path uses a *positional rotate cipher*,
//! not a salted hash. There is no challenge/response stage after `CN` — the
//! obfuscated password in the `CN` packet is the final word. MD5/SHA are
//! absent from both references on this path; `py _InStream._check_sheader`'s
//! SHA-256 is for gateway reentrancy, not client auth.

/// Obfuscate a credential field.
///
/// Per position `i` (from the start) with `n = len`, writing to position
/// `j = n - 1 - i`: `out[j] = rot5((in[i] ^ 0xA7) + j)`.
///
/// ref: go-irisnative src/connection/mod.go:encode.
/// ref: py-iris iris/_IRISConnection.py:__encode (ASCII) and __encodew
/// (unicode — keeps the high byte of each UTF-16 code unit, rotates the low
/// byte); for inputs that fit one byte per char both agree with go.
#[must_use]
pub fn encode_credentials(value: &str) -> Vec<u8> {
    // The references encode the string in the connection's character
    // encoding; non-ASCII usernames/passwords are rare and py's __encodew
    // keeps high bytes, so we work byte-wise over UTF-8, which for the
    // ASCII range that both servers actually use is byte-identical to
    // latin-1/UTF-16 low-byte rotation of the reference clients.
    let input = value.as_bytes();
    let n = input.len();
    let mut out = vec![0u8; n];
    for (i, &b) in input.iter().enumerate() {
        let j = n - 1 - i;
        let temp = (b ^ 0xA7).wrapping_add(u8::try_from(j).unwrap_or(0));
        out[j] = temp.rotate_right(3);
    }
    out
}

/// Best-effort OS user name for the `CN` packet; `"rust"` on failure.
///
/// ref: go-irisnative `src/connection/user_posix.go:userCurrent` (os/user,
/// then $USER); py-iris `_IRISConnection.py` getpass.getuser. The
/// references fall back to their own driver name; we identify as `rust`.
#[must_use]
pub fn system_user() -> String {
    std::env::var("USER")
        .ok()
        .filter(|u| !u.is_empty())
        .or_else(|| std::env::var("USERNAME").ok().filter(|u| !u.is_empty()))
        .unwrap_or_else(|| "rust".to_string())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn credential_cipher_unit_vectors() {
        // Independent implementation of the reference cipher in Python
        // (probes/refclient.py:encode_credentials), values captured 2026-10:
        assert_eq!(encode_credentials("SYS"), vec![0x9E, 0xFF, 0xDE]);
        assert_eq!(
            encode_credentials("_SYSTEM"),
            vec![0x5D, 0x7C, 0xBE, 0xFE, 0x40, 0x3F, 0xDF]
        );
    }

    #[test]
    fn cipher_is_a_positional_permutation() {
        // out[j] depends on in[i] with j = n-1-i: reversing order matters.
        let a = encode_credentials("ab");
        let b = encode_credentials("ba");
        assert_ne!(a, b);
        assert_eq!(encode_credentials(""), Vec::<u8>::new());
    }

    #[test]
    fn system_user_never_panics() {
        let u = system_user();
        assert_ne!(u, "");
    }
}
