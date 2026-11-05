use crate::static_files::path::decode;

#[derive(Clone, Debug)]
pub struct PathTemplate {
    parts: Vec<Part>,
}
#[derive(Clone, Debug)]
enum Part {
    Literal(String),
    Path,
    Capture(usize),
}
#[derive(Debug)]
pub enum PathError {
    InvalidPath,
    InvalidTemplate,
}
impl PathTemplate {
    pub fn compile(mut input: &str) -> Result<Self, PathError> {
        let mut parts = Vec::new();
        let mut probe = String::new();
        while !input.is_empty() {
            let index = input.find(['{', '}']).unwrap_or(input.len());
            if index != 0 {
                let literal = &input[..index];
                probe.push_str(literal);
                parts.push(Part::Literal(literal.to_owned()));
                input = &input[index..];
            }
            if input.is_empty() {
                break;
            }
            if !input.starts_with('{') {
                return Err(PathError::InvalidTemplate);
            }
            let end = input.find('}').ok_or(PathError::InvalidTemplate)?;
            let name = &input[1..end];
            let part = match name {
                "path" => Part::Path,
                _ if name.len() == 9
                    && name.starts_with("capture.")
                    && (b'1'..=b'9').contains(&name.as_bytes()[8]) =>
                {
                    Part::Capture(usize::from(name.as_bytes()[8] - b'1'))
                }
                _ => return Err(PathError::InvalidTemplate),
            };
            if matches!(part, Part::Path) && probe.is_empty() {
                probe.push_str("/x");
            } else {
                probe.push('x');
            }
            parts.push(part);
            input = &input[end + 1..];
        }
        if validate(&probe).is_err() {
            let mut bytes = probe.into_bytes();
            let dynamic = parts.iter().any(|part| !matches!(part, Part::Literal(_)));
            if dynamic {
                for i in 0..bytes.len() {
                    if bytes[i] == b'%'
                        && (bytes.get(i + 1).is_none_or(|b| !b.is_ascii_hexdigit())
                            || bytes.get(i + 2).is_none_or(|b| !b.is_ascii_hexdigit()))
                    {
                        bytes[i] = b'x';
                    }
                }
            }
            let sanitized = String::from_utf8(bytes).map_err(|_| PathError::InvalidPath)?;
            validate(&sanitized)?;
        }
        Ok(Self { parts })
    }
    pub fn expand(
        &self,
        path: &str,
        captures: &[Option<String>; 9],
        limit: usize,
    ) -> Result<String, PathError> {
        let mut result = String::new();
        for part in &self.parts {
            let value = match part {
                Part::Literal(v) => v.as_str(),
                Part::Path => path,
                Part::Capture(i) => captures[*i].as_deref().unwrap_or(""),
            };
            if result
                .len()
                .checked_add(value.len())
                .is_none_or(|n| n > limit)
            {
                return Err(PathError::InvalidPath);
            }
            result.push_str(value);
        }
        validate(&result)?;
        Ok(result)
    }
    pub fn allocated_bytes(&self) -> usize {
        self.parts
            .capacity()
            .saturating_mul(std::mem::size_of::<Part>())
            .saturating_add(
                self.parts
                    .iter()
                    .map(|p| {
                        if let Part::Literal(s) = p {
                            s.capacity()
                        } else {
                            0
                        }
                    })
                    .sum::<usize>(),
            )
    }
}
pub fn validate(path: &str) -> Result<(), PathError> {
    if path.len() > 64 * 1024
        || path.contains(['?', '#'])
        || path.bytes().any(|b| b.is_ascii_whitespace())
    {
        return Err(PathError::InvalidPath);
    }
    decode(path).map(|_| ()).map_err(|_| PathError::InvalidPath)
}
pub fn strip(path: &str, prefix: &str) -> Result<Option<String>, PathError> {
    validate(path)?;
    if path == prefix {
        return Ok(Some("/".to_owned()));
    }
    if prefix == "/" {
        return Ok(Some(path.to_owned()));
    }
    if prefix.ends_with('/') {
        return Ok(path.strip_prefix(prefix).map(|rest| format!("/{rest}")));
    }
    Ok(path
        .strip_prefix(prefix)
        .filter(|s| s.starts_with('/'))
        .map(str::to_owned))
}
