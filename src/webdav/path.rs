use super::model::{DavError, Result};
use std::path::{Path, PathBuf};

const HREF: percent_encoding::AsciiSet = percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct DavPath(String);
impl DavPath {
    pub fn parse(raw: &str) -> Result<Self> {
        if raw.len() > 64 * 1024 {
            return Err(DavError::invalid());
        }
        let decoded = crate::static_files::path::decode(raw).map_err(|_| DavError::invalid())?;
        let text = decoded.to_str().ok_or_else(DavError::invalid)?;
        if text.len() > 4096 {
            return Err(DavError::invalid());
        }
        Ok(Self(
            text.split('/')
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("/"),
        ))
    }
    pub fn from_key(key: &str) -> Result<Self> {
        if key.len() > 4096
            || key.contains(['\\', '\0'])
            || key.chars().any(char::is_control)
            || (!key.is_empty()
                && key
                    .split('/')
                    .any(|part| part.is_empty() || matches!(part, "." | "..")))
        {
            return Err(DavError::invalid());
        }
        Ok(Self(key.into()))
    }
    pub fn relative(&self) -> &Path {
        if self.0.is_empty() {
            Path::new(".")
        } else {
            Path::new(&self.0)
        }
    }
    pub fn key(&self) -> &str {
        &self.0
    }
    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }
    pub fn contains(&self, other: &Self) -> bool {
        self == other
            || self.is_root()
            || other
                .0
                .strip_prefix(&self.0)
                .is_some_and(|s| s.starts_with('/'))
    }
    pub fn parent(&self) -> Self {
        Self(
            self.0
                .rsplit_once('/')
                .map_or("", |(parent, _)| parent)
                .to_owned(),
        )
    }
    pub fn name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or("")
    }
    pub fn child(&self, name: &str) -> Result<Self> {
        if name.is_empty()
            || name.contains(['/', '\\'])
            || matches!(name, "." | "..")
            || name.chars().any(char::is_control)
        {
            return Err(DavError::invalid());
        }
        let mut path = PathBuf::from(&self.0);
        path.push(name);
        let text = path.to_str().ok_or_else(DavError::invalid)?;
        if text.len() > 4096 {
            return Err(DavError::limit());
        }
        Ok(Self(text.to_owned()))
    }
    pub fn href(&self) -> String {
        let mut out = String::from("/");
        for (i, part) in self.0.split('/').enumerate() {
            if i != 0 {
                out.push('/');
            }
            out.extend(percent_encoding::utf8_percent_encode(part, &HREF));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dav_path_decodes_once_and_uses_one_lock_key() -> Result<()> {
        assert_eq!(DavPath::parse("/a//%62/")?, DavPath::parse("/a/b")?);
        assert_eq!(DavPath::parse("/%252e")?.key(), "%2e");
        for path in ["/%2e", "/a/../b", "/%2f", "/%5c", "/%ff", "/%00"] {
            assert!(DavPath::parse(path).is_err());
        }
        assert!(DavPath::parse("/a")?.contains(&DavPath::parse("/a/b")?));
        assert!(!DavPath::parse("/a")?.contains(&DavPath::parse("/ab")?));
        Ok(())
    }
}
