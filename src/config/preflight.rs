use super::error::{ConfigError, Result};
use super::source::Origin;

pub fn check(origin: &Origin) -> Result<()> {
    let bytes = origin.source.text.as_bytes();
    let mut index = 0;
    let mut nodes = 0_u32;
    let mut comments = 0_u32;
    let mut slashdashes = 0_u32;
    let mut line_comment = false;
    let mut string: Option<(usize, usize)> = None;
    while index < bytes.len() {
        if line_comment {
            if matches!(bytes[index], b'\n' | b'\r' | 0x0b | 0x0c)
                || bytes[index..].starts_with(b"\xc2\x85")
                || bytes[index..].starts_with(b"\xe2\x80\xa8")
                || bytes[index..].starts_with(b"\xe2\x80\xa9")
            {
                line_comment = false;
            }
            index += 1;
            continue;
        }
        if comments > 0 {
            if bytes[index..].starts_with(b"/*") {
                comments += 1;
                index += 2;
            } else if bytes[index..].starts_with(b"*/") {
                comments -= 1;
                index += 2;
            } else {
                index += 1;
            }
        } else if let Some((hashes, quotes)) = string {
            if hashes == 0 && bytes[index] == b'\\' {
                index = (index + 2).min(bytes.len());
            } else if bytes[index..].starts_with(&b"\"\"\""[..quotes])
                && bytes
                    .get(index + quotes..index + quotes + hashes)
                    .is_some_and(|end| end.iter().all(|byte| *byte == b'#'))
            {
                index += quotes + hashes;
                string = None;
            } else {
                index += 1;
            }
        } else if bytes[index..].starts_with(b"/-") {
            slashdashes += 1;
            index += 2;
        } else if bytes[index..].starts_with(b"//") {
            line_comment = true;
            index += 2;
        } else if bytes[index..].starts_with(b"/*") {
            comments = 1;
            index += 2;
        } else if bytes[index] == b'"' || bytes[index] == b'#' {
            slashdashes = 0;
            let start = index;
            while index < bytes.len() && bytes[index] == b'#' {
                index += 1;
            }
            let hashes = index - start;
            if index < bytes.len() && bytes[index] == b'"' {
                let quotes = if bytes[index..].starts_with(b"\"\"\"") {
                    3
                } else {
                    1
                };
                index += quotes;
                string = Some((hashes, quotes));
            } else if hashes == 0 {
                index += 1;
            }
        } else {
            if bytes[index].is_ascii()
                && !matches!(bytes[index], b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
            {
                slashdashes = 0;
            }
            if bytes[index] == b'{' {
                nodes += 1;
            }
            if bytes[index] == b'}' {
                nodes = nodes.saturating_sub(1);
            }
            index += 1;
        }
        if nodes > 32 || comments > 32 || slashdashes > 32 {
            let location = Origin {
                offset: index.saturating_sub(1),
                ..origin.clone()
            };
            return Err(ConfigError::new(
                "CONFIG_LIMIT",
                "document",
                "KDL node, block-comment or slashdash nesting exceeds 32 levels",
                &location,
            ));
        }
    }
    Ok(())
}
