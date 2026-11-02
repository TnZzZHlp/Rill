use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone)]
pub struct Source {
    pub path: PathBuf,
    pub text: String,
}

impl fmt::Debug for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Source")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug)]
pub struct Origin {
    pub source: Arc<Source>,
    pub offset: usize,
    pub length: usize,
    pub references: Vec<Origin>,
}

impl Origin {
    pub fn file(path: &Path) -> Self {
        Self {
            source: Arc::new(Source {
                path: path.to_path_buf(),
                text: String::new(),
            }),
            offset: 0,
            length: 0,
            references: Vec::new(),
        }
    }

    pub fn position(&self) -> (usize, usize) {
        let mut end = self.offset.min(self.source.text.len());
        while !self.source.text.is_char_boundary(end) {
            end -= 1;
        }
        let before = &self.source.text[..end];
        let (mut line, mut column) = (1, 1);
        let mut characters = before.chars().peekable();
        while let Some(character) = characters.next() {
            if matches!(
                character,
                '\r' | '\n' | '\u{0085}' | '\u{000b}' | '\u{000c}' | '\u{2028}' | '\u{2029}'
            ) {
                if character == '\r' && characters.peek() == Some(&'\n') {
                    characters.next();
                }
                line += 1;
                column = 1;
            } else {
                column += 1;
            }
        }
        (line, column)
    }

    pub fn relative_path(&self, value: &str) -> PathBuf {
        self.source
            .path
            .parent()
            .unwrap_or(Path::new("/"))
            .join(value)
    }
}

#[derive(Clone, Debug)]
pub struct Located<T> {
    pub value: T,
    pub origin: Origin,
}

impl<T> Located<T> {
    pub fn new(value: T, origin: Origin) -> Self {
        Self { value, origin }
    }
}
