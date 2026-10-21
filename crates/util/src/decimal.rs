//! Decimal digits without allocating.

/// The decimal digits of `n`, written into the end of `buf`.
pub fn write(mut n: u64, buf: &mut [u8; 20]) -> &str {
    let mut at = buf.len();
    loop {
        at -= 1;
        buf[at] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    std::str::from_utf8(&buf[at..]).unwrap_or("0")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_digits() {
        assert_eq!(write(0, &mut [0; 20]), "0");
        assert_eq!(write(7, &mut [0; 20]), "7");
        assert_eq!(write(10, &mut [0; 20]), "10");
        assert_eq!(write(1234567890, &mut [0; 20]), "1234567890");
    }

    #[test]
    fn writes_the_largest_value() {
        assert_eq!(write(u64::MAX, &mut [0; 20]), "18446744073709551615");
    }
}
