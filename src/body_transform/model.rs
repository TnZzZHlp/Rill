use crate::config::source::Origin;

#[derive(Clone, Debug)]
pub enum RuleKind {
    Literal(String, String),
    Regex(String, Vec<super::replacement::Part>),
}

#[derive(Clone, Debug)]
pub struct Rule {
    pub kind: RuleKind,
    pub origin: Origin,
}

#[derive(Clone, Debug)]
pub struct Replace {
    pub mimes: Vec<String>,
    pub rules: Vec<Rule>,
    pub origin: Origin,
}

#[derive(Clone, Debug)]
pub struct Encode {
    pub mimes: Vec<String>,
    pub gzip: bool,
    pub zstd: bool,
    pub origin: Origin,
}

pub fn matches(mimes: &[String], mime: Option<&str>) -> bool {
    mime.and_then(|s| s.split(';').next())
        .is_some_and(|s| mimes.iter().any(|m| m.eq_ignore_ascii_case(s.trim())))
}
