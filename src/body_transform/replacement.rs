#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Part {
    Literal(Box<[u8]>),
    Capture(u8),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidReplacement;

pub fn parse(replacement: &str) -> Result<Vec<Part>, InvalidReplacement> {
    let input = replacement.as_bytes();
    let mut parts = Vec::new();
    let mut literal = Vec::new();
    let mut index = 0;
    while index < input.len() {
        if input[index] != b'$' {
            literal.push(input[index]);
            index += 1;
            continue;
        }
        let next = *input.get(index + 1).ok_or(InvalidReplacement)?;
        match next {
            b'$' => literal.push(b'$'),
            b'1'..=b'9' => {
                if replacement[index + 2..]
                    .chars()
                    .next()
                    .is_some_and(|character| character.is_alphanumeric() || character == '_')
                {
                    return Err(InvalidReplacement);
                }
                if !literal.is_empty() {
                    parts.push(Part::Literal(
                        std::mem::take(&mut literal).into_boxed_slice(),
                    ));
                }
                parts.push(Part::Capture(next - b'0'));
            }
            _ => return Err(InvalidReplacement),
        }
        index += 2;
    }
    if !literal.is_empty() {
        parts.push(Part::Literal(literal.into_boxed_slice()));
    }
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacement_accepts_captures_and_dollar_escape_but_rejects_ambiguous_forms() {
        assert_eq!(
            parse("é$$:$1!").expect("valid replacement"),
            vec![
                Part::Literal("é$:".as_bytes().into()),
                Part::Capture(1),
                Part::Literal(b"!".as_slice().into()),
            ]
        );
        for invalid in ["$", "$0", "$10", "$1name", "$1é", "$1_", "${name}"] {
            assert_eq!(parse(invalid), Err(InvalidReplacement), "{invalid}");
        }
    }
}
