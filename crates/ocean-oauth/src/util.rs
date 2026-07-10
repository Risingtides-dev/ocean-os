//! Small helpers: wall-clock milliseconds and minimal percent encode/decode
//! for query strings and form values (no `url` dependency).

use std::time::{SystemTime, UNIX_EPOCH};

/// Current wall clock as Unix milliseconds, saturating to 0 if the clock is
/// somehow before the epoch.
pub(crate) fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// RFC 3986 percent-encoding: unreserved characters (`A-Za-z0-9 -._~`) pass
/// through, everything else becomes `%XX`. Used for both query strings and
/// `application/x-www-form-urlencoded` bodies (none of our values contain
/// spaces, so the `%20` vs `+` distinction is moot).
pub(crate) fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for &byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                out.push('%');
                out.push_str(&format!("{byte:02X}"));
            }
        }
    }
    out
}

/// Percent-decode a query/form value, treating `+` as a space (form decoding).
pub(crate) fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'+' {
            out.push(b' ');
            i += 1;
        } else if b == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (from_hex(bytes[i + 1]), from_hex(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
            } else {
                out.push(b);
                i += 1;
            }
        } else {
            out.push(b);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn from_hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Build an ordered query string from `(key, value)` pairs. Order is preserved
/// verbatim so provider authorize-URL param ordering stays stable and
/// assertable.
pub(crate) fn build_query(params: &[(&str, &str)]) -> String {
    params
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Look up a single query/form parameter. A present-but-empty value is treated
/// as absent (matches `URLSearchParams.get(x) || fallback` semantics).
pub(crate) fn query_get(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        let (raw_key, raw_value) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        if percent_decode(raw_key) == key {
            let value = percent_decode(raw_value);
            return if value.is_empty() { None } else { Some(value) };
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{build_query, percent_decode, percent_encode, query_get};

    #[test]
    fn build_query_preserves_order_and_percent_encodes() {
        // Order is preserved verbatim; values are RFC 3986 percent-encoded.
        let q = build_query(&[
            ("zeta", "last"),
            ("alpha", "first"),
            ("scope", "a b:c"),
            ("redirect", "http://localhost:1/x"),
        ]);
        assert_eq!(
            q,
            "zeta=last&alpha=first&scope=a%20b%3Ac&redirect=http%3A%2F%2Flocalhost%3A1%2Fx"
        );
    }

    #[test]
    fn build_query_empty_pairs_yields_empty_string() {
        assert_eq!(build_query(&[]), "");
    }

    #[test]
    fn percent_encode_unreserved_passthrough_and_specials_encoded() {
        // Unreserved set passes through untouched.
        assert_eq!(percent_encode("AZaz09-._~"), "AZaz09-._~");
        // The two encodings that matter for OAuth scopes: space and colon.
        assert_eq!(percent_encode(" "), "%20");
        assert_eq!(percent_encode("a:b"), "a%3Ab");
        assert_eq!(
            percent_encode("openid profile a:b"),
            "openid%20profile%20a%3Ab"
        );
        // Hex digits are uppercase.
        assert_eq!(percent_encode("/"), "%2F");
    }

    #[test]
    fn percent_decode_handles_percent_escapes_and_plus() {
        assert_eq!(percent_decode("a%20b%3Ac"), "a b:c");
        assert_eq!(percent_decode("a+b"), "a b"); // form-style '+' -> space
        assert_eq!(percent_decode("plain"), "plain");
        // A trailing '%' with no hex pair passes through literally.
        assert_eq!(percent_decode("100%"), "100%");
    }

    #[test]
    fn query_get_decodes_and_treats_empty_or_missing_as_none() {
        assert_eq!(
            query_get("code=ab%2Dc&state=zz", "code"),
            Some("ab-c".to_string())
        );
        assert_eq!(query_get("state=zz", "code"), None); // missing entirely
        assert_eq!(query_get("code=", "code"), None); // present but empty
        assert_eq!(query_get("code=&state=1", "code"), None); // empty among peers
        assert_eq!(
            query_get("q=hello+world", "q"),
            Some("hello world".to_string())
        );
        // First occurrence wins (URLSearchParams.get semantics).
        assert_eq!(query_get("k=one&k=two", "k"), Some("one".to_string()));
    }
}
