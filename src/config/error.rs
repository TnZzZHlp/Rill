use std::error::Error;
use std::fmt;

use super::source::Origin;

pub type Result<T> = std::result::Result<T, ConfigError>;

#[derive(Debug)]
pub struct ConfigError {
    pub code: &'static str,
    pub item: Box<str>,
    pub reason: Box<str>,
    pub origin: Origin,
    pub related: Vec<Origin>,
}

impl ConfigError {
    pub fn new(code: &'static str, item: &str, reason: &str, origin: &Origin) -> Self {
        Self {
            code,
            item: item.into(),
            reason: reason.into(),
            origin: origin.clone(),
            related: Vec::new(),
        }
    }

    pub fn related(mut self, origin: &Origin) -> Self {
        self.related.push(origin.clone());
        self
    }
}

fn location(f: &mut fmt::Formatter<'_>, origin: &Origin) -> fmt::Result {
    let (line, column) = origin.position();
    write!(f, "{:?}:{line}:{column}", origin.source.path)
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ", self.code)?;
        location(f, &self.origin)?;
        write!(f, " [{:?}]: {}", self.item, self.reason)?;
        let mut seen = std::collections::BTreeSet::new();
        seen.insert((
            &self.origin.source.path,
            self.origin.offset,
            self.origin.length,
        ));
        for origin in self
            .related
            .iter()
            .flat_map(|origin| std::iter::once(origin).chain(origin.references.iter()))
            .chain(&self.origin.references)
        {
            if seen.insert((&origin.source.path, origin.offset, origin.length)) {
                write!(f, "\n  related: ")?;
                location(f, origin)?;
            }
        }
        Ok(())
    }
}

impl Error for ConfigError {}
