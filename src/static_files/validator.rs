use std::fs::Metadata;
use std::os::unix::fs::MetadataExt;

use super::representation::Encoding;

pub fn weak_etag(metadata: &Metadata, encoding: Encoding) -> String {
    let encoding = match encoding {
        Encoding::Brotli => "br",
        Encoding::Gzip => "gzip",
        Encoding::Identity => "identity",
    };
    format!(
        "W/\"{}-{:x}-{:x}-{:x}-{:x}-{:x}-{:x}-{:x}\"",
        encoding,
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec()
    )
}

pub fn if_none_match<'a>(headers: impl IntoIterator<Item = &'a str>, etag: &str) -> bool {
    let Some(expected) = etag.strip_prefix("W/") else {
        return false;
    };
    let mut matched = false;
    let mut any = false;
    let mut wildcard = false;
    for header in headers {
        let value = header.trim_matches([' ', '\t']);
        if value == "*" {
            if any {
                return false;
            }
            matched = true;
            any = true;
            wildcard = true;
            continue;
        }
        if wildcard {
            return false;
        }
        let bytes = value.as_bytes();
        let mut at = 0;
        loop {
            while bytes.get(at).is_some_and(|b| matches!(b, b' ' | b'\t')) {
                at += 1;
            }
            if bytes.get(at..at + 2) == Some(b"W/") {
                at += 2;
            }
            if bytes.get(at) != Some(&b'"') {
                return false;
            }
            let start = at;
            at += 1;
            while let Some(&byte) = bytes.get(at) {
                if byte == b'"' {
                    break;
                }
                if byte < 0x21 || byte == 0x7f {
                    return false;
                }
                at += 1;
            }
            if bytes.get(at) != Some(&b'"') {
                return false;
            }
            at += 1;
            matched |= &value[start..at] == expected;
            any = true;
            while bytes.get(at).is_some_and(|b| matches!(b, b' ' | b'\t')) {
                at += 1;
            }
            if at == bytes.len() {
                break;
            }
            if bytes.get(at) != Some(&b',') {
                return false;
            }
            at += 1;
        }
    }
    any && matched
}

#[cfg(test)]
mod tests {
    use super::if_none_match;

    #[test]
    fn weak_comparison_and_invalid_fields() {
        assert!(if_none_match(["\"other\", W/\"abc\""], "W/\"abc\""));
        assert!(if_none_match(["\"abc\""], "W/\"abc\""));
        assert!(if_none_match(["*"], "W/\"abc\""));
        assert!(!if_none_match(["\"abc\", broken"], "W/\"abc\""));
        assert!(!if_none_match(["W/\"other\""], "W/\"abc\""));
        assert!(!if_none_match(["\u{a0}*"], "W/\"abc\""));
        assert!(!if_none_match(["\u{a0}W/\"abc\""], "W/\"abc\""));
    }
}
