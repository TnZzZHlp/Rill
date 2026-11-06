use crate::config::source::Origin;

#[derive(Clone, Debug)]
pub struct Chain {
    pub rules: Vec<Rule>,
    pub errors: Option<Vec<Rule>>,
    pub origin: Origin,
}
#[derive(Clone, Debug)]
pub struct Rule {
    pub id: Option<String>,
    pub matcher: Option<Matcher>,
    pub actions: Vec<Action>,
    pub origin: Origin,
}
#[derive(Clone, Debug)]
pub struct Matcher {
    pub kind: MatcherKind,
    pub origin: Origin,
}
#[derive(Clone, Debug)]
pub enum MatcherKind {
    All(Vec<Matcher>),
    Any(Vec<Matcher>),
    Not(Box<Matcher>),
    Method(String),
    HeaderEqual(String, String),
    HeaderPrefix(String, String),
    PathGlob(String),
    PathRegex(String),
    ClientCidr(crate::config::model::Network),
    ErrorStatus(u16),
}
#[derive(Clone, Debug)]
pub struct DynamicProxy {
    pub prefix: String,
    pub anonymous: bool,
    pub ports: Vec<u16>,
    pub origin: Origin,
}
#[derive(Clone, Debug)]
pub enum Action {
    BodyReplace(std::sync::Arc<crate::body_transform::model::Replace>),
    Encode(std::sync::Arc<crate::body_transform::model::Encode>),
    Header(std::sync::Arc<super::headers::HeaderPolicy>),
    BasicAuth(crate::auth::model::Basic),
    StaticToken(crate::auth::model::Token),
    Rewrite(super::path::PathTemplate),
    StripPrefix(String),
    TryFiles {
        root: crate::config::source::Located<std::path::PathBuf>,
        candidates: Vec<super::path::PathTemplate>,
    },
    DynamicProxy(DynamicProxy),
    Continue,
    Reenter,
    Route {
        raw: Box<crate::config::model::RawRoute>,
        index: Option<usize>,
    },
    Respond {
        status: u16,
        text: std::sync::Arc<str>,
        content_type: String,
    },
    Branch(Vec<Branch>),
}
#[derive(Clone, Debug)]
pub struct Branch {
    pub matcher: Option<Matcher>,
    pub actions: Vec<Action>,
    pub origin: Origin,
}
