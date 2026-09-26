//! Value tags and the arithmetic of `wp_cache_incr()` / `wp_cache_decr()`.
//!
//! The engine stores bytes; the tag says how the PHP side encoded them.
//! Integers and floats are stored natively so that incr/decr can run under
//! the shard lock without a round trip through PHP.

pub const TAG_NULL: u8 = 0;
pub const TAG_FALSE: u8 = 1;
pub const TAG_TRUE: u8 = 2;
/// 8 bytes, native-endian i64.
pub const TAG_LONG: u8 = 3;
/// 8 bytes, native-endian f64.
pub const TAG_DOUBLE: u8 = 4;
/// Raw string bytes.
pub const TAG_STRING: u8 = 5;
/// Output of PHP's `serialize()` (arrays and objects).
pub const TAG_SERIALIZED: u8 = 6;

pub fn tag_name(tag: u8) -> &'static str {
    match tag {
        TAG_NULL => "null",
        TAG_FALSE | TAG_TRUE => "bool",
        TAG_LONG => "int",
        TAG_DOUBLE => "float",
        TAG_STRING => "string",
        TAG_SERIALIZED => "serialized",
        _ => "unknown",
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Number {
    Long(i64),
    Double(f64),
}

impl Number {
    pub fn encode(self) -> (u8, [u8; 8]) {
        match self {
            Number::Long(v) => (TAG_LONG, v.to_ne_bytes()),
            Number::Double(v) => (TAG_DOUBLE, v.to_ne_bytes()),
        }
    }
}

/// Reproduces core `WP_Object_Cache::incr()`:
///
/// ```php
/// if ( ! is_numeric( $value ) ) { $value = 0; }
/// $value += (int) $offset;          // decr passes a negative offset
/// if ( $value < 0 ) { $value = 0; }
/// ```
pub fn incr(tag: u8, bytes: &[u8], offset: i64) -> Number {
    let current = match tag {
        TAG_LONG if bytes.len() == 8 => Number::Long(i64::from_ne_bytes(bytes.try_into().unwrap())),
        TAG_DOUBLE if bytes.len() == 8 => {
            Number::Double(f64::from_ne_bytes(bytes.try_into().unwrap()))
        }
        TAG_STRING => parse_numeric(bytes).unwrap_or(Number::Long(0)),
        _ => Number::Long(0),
    };
    let sum = match current {
        Number::Long(v) => match v.checked_add(offset) {
            Some(s) => Number::Long(s),
            // PHP promotes an overflowing int to float.
            None => Number::Double(v as f64 + offset as f64),
        },
        Number::Double(v) => Number::Double(v + offset as f64),
    };
    let negative = match sum {
        Number::Long(v) => v < 0,
        Number::Double(v) => v < 0.0,
    };
    if negative {
        Number::Long(0)
    } else {
        sum
    }
}

/// PHP 8 `is_numeric()` for strings, returning the value `+` would produce:
/// optional surrounding whitespace, optional sign, decimal digits with an
/// optional fraction and exponent. No hex, no octal, no "1_000".
pub fn parse_numeric(bytes: &[u8]) -> Option<Number> {
    let s = std::str::from_utf8(bytes).ok()?;
    let t = s.trim_matches(|c| matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c'));
    let b = t.as_bytes();
    let mut i = 0;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    let int_start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let int_digits = i - int_start;
    let mut is_float = false;
    let mut frac_digits = 0;
    if i < b.len() && b[i] == b'.' {
        is_float = true;
        i += 1;
        let f = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        frac_digits = i - f;
    }
    if int_digits == 0 && frac_digits == 0 {
        return None;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut j = i + 1;
        if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
            j += 1;
        }
        let e = j;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        if j == e {
            return None;
        }
        is_float = true;
        i = j;
    }
    if i != b.len() {
        return None;
    }
    if !is_float {
        if let Ok(v) = t.parse::<i64>() {
            return Some(Number::Long(v));
        }
    }
    t.parse::<f64>().ok().map(Number::Double)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str, off: i64) -> Number {
        incr(TAG_STRING, v.as_bytes(), off)
    }

    #[test]
    fn php_numeric_strings() {
        assert_eq!(parse_numeric(b"5"), Some(Number::Long(5)));
        assert_eq!(parse_numeric(b" 5 "), Some(Number::Long(5)));
        assert_eq!(parse_numeric(b"-7"), Some(Number::Long(-7)));
        assert_eq!(parse_numeric(b"1.5"), Some(Number::Double(1.5)));
        assert_eq!(parse_numeric(b".5"), Some(Number::Double(0.5)));
        assert_eq!(parse_numeric(b"5."), Some(Number::Double(5.0)));
        assert_eq!(parse_numeric(b"1e3"), Some(Number::Double(1000.0)));
        assert_eq!(parse_numeric(b"0x1A"), None);
        assert_eq!(parse_numeric(b"abc"), None);
        assert_eq!(parse_numeric(b""), None);
        assert_eq!(parse_numeric(b"1e"), None);
        assert_eq!(parse_numeric(b"."), None);
        assert!(matches!(
            parse_numeric(b"99999999999999999999"),
            Some(Number::Double(_))
        ));
    }

    #[test]
    fn incr_follows_core() {
        assert_eq!(incr(TAG_LONG, &5i64.to_ne_bytes(), 2), Number::Long(7));
        assert_eq!(incr(TAG_LONG, &5i64.to_ne_bytes(), -9), Number::Long(0));
        assert_eq!(s("10", 1), Number::Long(11));
        assert_eq!(s("1.5", 1), Number::Double(2.5));
        assert_eq!(s("abc", 3), Number::Long(3));
        assert_eq!(incr(TAG_SERIALIZED, b"a:0:{}", 1), Number::Long(1));
        assert_eq!(incr(TAG_TRUE, b"", 1), Number::Long(1));
        assert_eq!(incr(TAG_DOUBLE, &0.5f64.to_ne_bytes(), -1), Number::Long(0));
        assert!(matches!(
            incr(TAG_LONG, &i64::MAX.to_ne_bytes(), 1),
            Number::Double(_)
        ));
    }
}
