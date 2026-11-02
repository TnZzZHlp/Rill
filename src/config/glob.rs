use std::fs;
use std::path::{Component, Path, PathBuf};

use super::error::{ConfigError, Result};
use super::source::Origin;

#[derive(Clone)]
enum Token {
    Star,
    Any,
    Byte(u8),
    Class { negate: bool, ranges: Vec<(u8, u8)> },
}

fn tokens(pattern: &str, origin: &Origin) -> Result<Vec<Token>> {
    let mut result = Vec::new();
    let mut input = pattern.bytes().peekable();
    while let Some(byte) = input.next() {
        match byte {
            b'*' => result.push(Token::Star),
            b'?' => result.push(Token::Any),
            b'[' => {
                let negate = matches!(input.peek(), Some(b'!' | b'^'));
                if negate {
                    input.next();
                }
                let mut ranges = Vec::new();
                loop {
                    let start = input.next().ok_or_else(|| {
                        ConfigError::new(
                            "CONFIG_TYPE",
                            "import",
                            "unclosed glob character class",
                            origin,
                        )
                    })?;
                    if start == b']' {
                        if ranges.is_empty() {
                            return Err(ConfigError::new(
                                "CONFIG_TYPE",
                                "import",
                                "empty glob character class",
                                origin,
                            ));
                        }
                        break;
                    }
                    let end = if input.peek() == Some(&b'-') {
                        input.next();
                        input.next().filter(|b| *b != b']').ok_or_else(|| {
                            ConfigError::new("CONFIG_TYPE", "import", "invalid glob range", origin)
                        })?
                    } else {
                        start
                    };
                    if start > end {
                        return Err(ConfigError::new(
                            "CONFIG_TYPE",
                            "import",
                            "reversed glob range",
                            origin,
                        ));
                    }
                    ranges.push((start, end));
                }
                result.push(Token::Class { negate, ranges });
            }
            b'\\' => {
                return Err(ConfigError::new(
                    "CONFIG_TYPE",
                    "import",
                    "glob escaping is not supported",
                    origin,
                ));
            }
            byte => result.push(Token::Byte(byte)),
        }
    }
    Ok(result)
}

fn matches(tokens: &[Token], name: &[u8]) -> bool {
    let mut previous = vec![false; name.len() + 1];
    previous[0] = true;
    for token in tokens {
        let mut current = vec![false; name.len() + 1];
        if matches!(token, Token::Star) {
            current[0] = previous[0];
        }
        for i in 1..=name.len() {
            current[i] = match token {
                Token::Star => previous[i] || current[i - 1],
                Token::Any => previous[i - 1],
                Token::Byte(byte) => previous[i - 1] && name[i - 1] == *byte,
                Token::Class { negate, ranges } => {
                    previous[i - 1]
                        && (ranges
                            .iter()
                            .any(|(start, end)| (*start..=*end).contains(&name[i - 1]))
                            != *negate)
                }
            };
        }
        previous = current;
    }
    previous[name.len()]
}

pub fn expand(path: &Path, origin: &Origin) -> Result<Vec<PathBuf>> {
    let mut paths = vec![PathBuf::new()];
    if path.components().count() > 64 {
        return Err(ConfigError::new(
            "CONFIG_LIMIT",
            "import",
            "import path has too many components",
            origin,
        ));
    }
    for component in path.components() {
        let Component::Normal(name) = component else {
            for path in &mut paths {
                path.push(component.as_os_str());
            }
            continue;
        };
        let name = name.to_str().ok_or_else(|| {
            ConfigError::new("CONFIG_TYPE", "import", "import path must be UTF-8", origin)
        })?;
        if !name.contains(['*', '?', '[']) {
            for path in &mut paths {
                path.push(name);
            }
            continue;
        }
        let pattern = tokens(name, origin)?;
        let mut expanded = Vec::new();
        for directory in paths {
            let entries = fs::read_dir(&directory).map_err(|_| {
                ConfigError::new(
                    "RESOURCE_IO",
                    "import",
                    "cannot list import directory",
                    origin,
                )
            })?;
            for entry in entries {
                let entry = entry.map_err(|_| {
                    ConfigError::new(
                        "RESOURCE_IO",
                        "import",
                        "cannot read import directory entry",
                        origin,
                    )
                })?;
                let name = entry.file_name();
                let name = name.to_str().ok_or_else(|| {
                    ConfigError::new(
                        "CONFIG_TYPE",
                        "import",
                        "glob directory entry must be UTF-8",
                        origin,
                    )
                })?;
                if matches(&pattern, name.as_bytes()) {
                    expanded.push(entry.path());
                    if expanded.len() > 512 {
                        return Err(ConfigError::new(
                            "CONFIG_LIMIT",
                            "import",
                            "glob matches too many paths",
                            origin,
                        ));
                    }
                }
            }
        }
        paths = expanded;
    }
    paths.sort();
    if paths.is_empty() {
        return Err(ConfigError::new(
            "RESOURCE_IO",
            "import",
            "import pattern matches no files",
            origin,
        ));
    }
    Ok(paths)
}
