use super::{
    model::{DavError, Result},
    path::DavPath,
};

#[derive(Clone, Debug)]
pub enum State {
    Token(String),
    Etag(String),
}
#[derive(Clone, Debug)]
pub struct Condition {
    pub not: bool,
    pub state: State,
}
#[derive(Clone, Debug)]
pub struct Group {
    pub resource: Option<String>,
    pub alternatives: Vec<Vec<Condition>>,
}
#[derive(Default)]
pub struct Submitted {
    tokens: Vec<(DavPath, String)>,
    _memory: super::budget::Memory,
}
impl std::ops::Deref for Submitted {
    type Target = [(DavPath, String)];
    fn deref(&self) -> &Self::Target {
        &self.tokens
    }
}
#[derive(Clone, Debug, Default)]
pub struct IfHeader {
    pub groups: Vec<Group>,
}
impl IfHeader {
    pub fn parse(value: &str) -> Result<Self> {
        let mut parser = Parser {
            value: value.as_bytes(),
            offset: 0,
        };
        let mut groups: Vec<Group> = Vec::new();
        let mut tagged = None;
        while parser.space() {
            let resource = if parser.peek() == Some(b'<') {
                Some(parser.delimited(b'<', b'>')?)
            } else {
                None
            };
            if tagged
                .replace(resource.is_some())
                .is_some_and(|previous| previous != resource.is_some())
            {
                return Err(DavError::invalid());
            }
            let mut alternatives = Vec::new();
            while parser.space() && parser.peek() == Some(b'(') {
                parser.offset += 1;
                let mut conditions = Vec::new();
                while parser.space() && parser.peek() != Some(b')') {
                    let not = parser.value[parser.offset..].starts_with(b"Not")
                        && matches!(parser.value.get(parser.offset + 3), Some(b' ' | b'\t'));
                    if not {
                        parser.offset += 3;
                        parser.space();
                    }
                    let state = match parser.peek() {
                        Some(b'<') => {
                            let token = parser.delimited(b'<', b'>')?;
                            if !token.is_ascii()
                                || token
                                    .bytes()
                                    .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
                                || token.contains(['<', '>', '"', '\\'])
                                || url::Url::parse(&token).is_err()
                                || token.bytes().enumerate().any(|(i, b)| {
                                    b == b'%'
                                        && (!token
                                            .as_bytes()
                                            .get(i + 1)
                                            .is_some_and(u8::is_ascii_hexdigit)
                                            || !token
                                                .as_bytes()
                                                .get(i + 2)
                                                .is_some_and(u8::is_ascii_hexdigit))
                                })
                            {
                                return Err(DavError::invalid());
                            }
                            State::Token(token)
                        }
                        Some(b'[') => {
                            let etag = parser.etag()?;
                            if !valid_etag(&etag) {
                                return Err(DavError::invalid());
                            }
                            State::Etag(etag)
                        }
                        _ => return Err(DavError::invalid()),
                    };
                    conditions.push(Condition { not, state });
                }
                if conditions.is_empty() || parser.peek() != Some(b')') {
                    return Err(DavError::invalid());
                }
                parser.offset += 1;
                alternatives.push(conditions);
            }
            if alternatives.is_empty() {
                return Err(DavError::invalid());
            }
            if let Some(group) = groups.iter_mut().find(|g| g.resource == resource) {
                group.alternatives.extend(alternatives);
            } else {
                groups.push(Group {
                    resource,
                    alternatives,
                });
            }
        }
        if groups.is_empty() {
            return Err(DavError::invalid());
        }
        Ok(Self { groups })
    }
    pub fn evaluate(
        &self,
        source: &DavPath,
        map: impl Fn(&str) -> Result<DavPath>,
        mut etag: impl FnMut(&DavPath) -> Result<Option<String>>,
        mut token: impl FnMut(&DavPath, &str) -> bool,
    ) -> Result<Submitted> {
        let mut memory = super::budget::Memory::default();
        let mut resources: std::collections::BTreeMap<DavPath, Vec<&Vec<Condition>>> =
            std::collections::BTreeMap::new();
        for group in &self.groups {
            let path = group
                .resource
                .as_deref()
                .map_or_else(|| Ok(source.clone()), &map)?;
            memory.reserve(
                path.key().len()
                    + 256
                    + 2 * group.alternatives.len() * std::mem::size_of::<usize>(),
            )?;
            resources
                .entry(path)
                .or_default()
                .extend(&group.alternatives);
        }
        let mut submitted = Submitted::default();
        for (path, alternatives) in resources {
            let current = etag(&path)?;
            let mut matched = false;
            for list in alternatives {
                let success = list.iter().all(|condition| {
                    let state = match &condition.state {
                        State::Token(value) => token(&path, value),
                        State::Etag(value) => {
                            current.as_deref().is_some_and(|tag| weak_equal(tag, value))
                        }
                    };
                    state != condition.not
                });
                matched |= success;
                for condition in list {
                    if let State::Token(value) = &condition.state
                        && !submitted
                            .tokens
                            .iter()
                            .any(|(p, t)| *p == path && t == value)
                    {
                        submitted._memory.reserve(
                            path.key().len()
                                + value.len()
                                + 2 * std::mem::size_of::<(DavPath, String)>(),
                        )?;
                        submitted
                            .tokens
                            .try_reserve_exact(1)
                            .map_err(|_| DavError::limit())?;
                        submitted.tokens.push((path.clone(), value.clone()));
                    }
                }
            }
            if !matched {
                return Err(DavError::new(412, "DAV_PRECONDITION"));
            }
        }
        Ok(submitted)
    }
}
struct Parser<'a> {
    value: &'a [u8],
    offset: usize,
}
impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.value.get(self.offset).copied()
    }
    fn space(&mut self) -> bool {
        while matches!(self.peek(), Some(b' ' | b'\t')) {
            self.offset += 1;
        }
        self.offset < self.value.len()
    }
    fn etag(&mut self) -> Result<String> {
        self.offset += 1;
        let begin = self.offset;
        if self.value[self.offset..].starts_with(b"W/") {
            self.offset += 2;
        }
        if self.peek() != Some(b'"') {
            return Err(DavError::invalid());
        }
        self.offset += 1;
        while self.peek().is_some_and(|byte| byte != b'"') {
            self.offset += 1;
        }
        if self.peek() != Some(b'"') {
            return Err(DavError::invalid());
        }
        self.offset += 1;
        let tag = std::str::from_utf8(&self.value[begin..self.offset])
            .map_err(|_| DavError::invalid())?
            .to_owned();
        if self.peek() != Some(b']') {
            return Err(DavError::invalid());
        }
        self.offset += 1;
        Ok(tag)
    }
    fn delimited(&mut self, start: u8, end: u8) -> Result<String> {
        if self.peek() != Some(start) {
            return Err(DavError::invalid());
        }
        self.offset += 1;
        let begin = self.offset;
        while self.peek().is_some_and(|b| b != end) {
            if self.peek().is_some_and(|b| b.is_ascii_control()) {
                return Err(DavError::invalid());
            }
            self.offset += 1;
        }
        if self.peek() != Some(end) || begin == self.offset {
            return Err(DavError::invalid());
        }
        let value = std::str::from_utf8(&self.value[begin..self.offset])
            .map_err(|_| DavError::invalid())?
            .to_owned();
        self.offset += 1;
        Ok(value)
    }
}
pub fn valid_etag(value: &str) -> bool {
    let value = value.strip_prefix("W/").unwrap_or(value);
    value.len() >= 2
        && value.starts_with('"')
        && value.ends_with('"')
        && value.as_bytes()[1..value.len() - 1]
            .iter()
            .all(|b| *b >= 0x21 && *b != b'"' && *b != 0x7f)
}
pub fn weak_equal(left: &str, right: &str) -> bool {
    left.strip_prefix("W/").unwrap_or(left) == right.strip_prefix("W/").unwrap_or(right)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn if_header_and_or_not_and_tagged_resources() -> Result<()> {
        let path = DavPath::parse("/a")?;
        let parsed = IfHeader::parse("(<urn:token> Not [\"old\"]) ([\"new\"])")?;
        assert_eq!(
            parsed
                .evaluate(
                    &path,
                    DavPath::parse,
                    |_| Ok(Some("\"new\"".into())),
                    |_, t| t == "urn:token"
                )?
                .len(),
            1
        );
        assert!(IfHeader::parse("(<urn:token>) <http://host/a> (<urn:x>)").is_err());
        assert!(IfHeader::parse("(Not)").is_err());
        IfHeader::parse("([\"a]b\"])")?.evaluate(
            &path,
            DavPath::parse,
            |_| Ok(Some("W/\"a]b\"".into())),
            |_, _| false,
        )?;
        assert_eq!(
            IfHeader::parse("(Not\t<urn:token>) ([\"new\"])")?
                .evaluate(
                    &path,
                    DavPath::parse,
                    |_| Ok(Some("\"new\"".into())),
                    |_, _| true
                )?
                .len(),
            1
        );
        IfHeader::parse("</%61> ([\"wrong\"]) </a> ([\"new\"])")?.evaluate(
            &path,
            DavPath::parse,
            |_| Ok(Some("\"new\"".into())),
            |_, _| false,
        )?;
        assert!(IfHeader::parse("(<not a URI>)").is_err());
        assert!(
            IfHeader::parse("(<urn:token>)")?
                .evaluate(&path, DavPath::parse, |_| Ok(None), |_, _| false)
                .is_err()
        );
        Ok(())
    }
}
