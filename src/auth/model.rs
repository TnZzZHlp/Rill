use crate::config::source::{Located, Origin};
use std::{fmt, path::PathBuf};

#[derive(Clone)]
pub struct Secret(pub String);
impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}
#[derive(Clone)]
pub struct TokenDigest(pub [u8; 32]);
impl fmt::Debug for TokenDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}
#[derive(Clone)]
pub struct User {
    pub name: String,
    pub hash: Secret,
    pub origin: Origin,
}
impl fmt::Debug for User {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("User")
            .field("name", &"[REDACTED]")
            .field("hash", &self.hash)
            .field("origin", &self.origin)
            .finish()
    }
}
#[derive(Clone, Debug)]
pub struct Basic {
    pub realm: String,
    pub challenge: String,
    pub users: Vec<User>,
    pub origin: Origin,
}
#[derive(Clone, Debug)]
pub struct Token {
    pub header: http::HeaderName,
    pub file: Located<PathBuf>,
    pub origin: Origin,
}
