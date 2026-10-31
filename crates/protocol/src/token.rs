//! The join token: 128 bits from `/dev/urandom`, compared in constant time.

use std::io;

use tokio::io::AsyncReadExt;

/// 16 bytes from `/dev/urandom` as 32 lowercase hex characters.
pub async fn new_token() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    let mut random = tokio::fs::File::open("/dev/urandom").await?;
    random.read_exact(&mut bytes).await?;
    Ok(hex(&bytes))
}

/// Compares in constant time over the bytes; the lengths are compared first.
pub fn same(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (left, right) in a.iter().zip(b) {
        diff |= left ^ right;
    }
    diff == 0
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(DIGITS[usize::from(byte >> 4)] as char);
        out.push(DIGITS[usize::from(byte & 0xf)] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn new_token_is_32_hex_chars() {
        let token = new_token().await.expect("reads urandom");
        assert_eq!(token.len(), 32);
        assert!(
            token
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "{token}"
        );
    }

    #[tokio::test]
    async fn two_tokens_differ() {
        let one = new_token().await.expect("reads urandom");
        let two = new_token().await.expect("reads urandom");
        assert_ne!(one, two);
    }

    #[test]
    fn same_compares_the_bytes() {
        let token = "0123456789abcdef0123456789abcdef";
        assert!(same(token, token));
        assert!(!same(token, "0123456789abcdef0123456789abcdee"));
        assert!(!same(token, "0123456789abcdef0123456789abcde"));
        assert!(!same("", token));
        assert!(same("", ""));
    }
}
