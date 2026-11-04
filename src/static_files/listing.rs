use std::io;
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path};

use nix::errno::Errno;

use super::root::Root;

const MAX_SCANNED: usize = 4096;
const MAX_VISIBLE: usize = 512;
const MAX_BYTES: usize = 256 * 1024;

#[derive(Debug)]
pub(crate) enum ListingError {
    Io(io::Error),
    Limit,
}

impl From<io::Error> for ListingError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && !name
            .chars()
            .any(|c| c.is_control() || c == '\\' || c == '/')
}

fn encoded(input: &str) -> String {
    let mut output = String::new();
    for byte in input.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            output.push(char::from(byte));
        } else {
            use std::fmt::Write;
            let _ = write!(output, "%{byte:02X}");
        }
    }
    output
}

fn escaped(input: &str) -> String {
    let mut output = String::new();
    for c in input.chars() {
        match c {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            '\'' => output.push_str("&#39;"),
            _ => output.push(c),
        }
    }
    output
}

fn append(output: &mut String, value: &str) -> Result<(), ListingError> {
    if output
        .len()
        .checked_add(value.len())
        .is_none_or(|len| len > MAX_BYTES)
    {
        return Err(ListingError::Limit);
    }
    output.push_str(value);
    Ok(())
}

fn changed() -> ListingError {
    ListingError::Io(io::Error::from_raw_os_error(Errno::ESTALE as i32))
}

fn verify_directory(
    root: &Root,
    requested: &Path,
    directory: &std::fs::File,
) -> Result<(), ListingError> {
    if root.same_directory(requested, directory).unwrap_or(false) {
        Ok(())
    } else {
        Err(changed())
    }
}

fn verify_entry(
    root: &Root,
    requested: &Path,
    directory: &std::fs::File,
    name: &Path,
    ino: u64,
) -> Result<(), ListingError> {
    verify_directory(root, requested, directory)?;
    if root
        .same_entry(&requested.join(name), directory, name, ino)
        .unwrap_or(false)
    {
        Ok(())
    } else {
        Err(changed())
    }
}

pub(crate) fn render(
    root: &Root,
    requested: &Path,
    directory: std::fs::File,
) -> Result<Vec<u8>, ListingError> {
    let mut base = String::from("/");
    for component in requested.components() {
        let Component::Normal(component) = component else {
            return Err(ListingError::Io(io::Error::from(
                io::ErrorKind::InvalidInput,
            )));
        };
        let name = component
            .to_str()
            .filter(|name| {
                !name
                    .chars()
                    .any(|c| c.is_control() || c == '\\' || c == '/')
            })
            .ok_or_else(|| ListingError::Io(io::Error::from(io::ErrorKind::InvalidInput)))?;
        append(&mut base, &encoded(name))?;
        append(&mut base, "/")?;
    }
    let mut directory = root.open_directory(directory)?;
    let stable = std::fs::File::from(directory.as_fd().try_clone_to_owned()?);
    verify_directory(root, requested, &stable)?;
    let mut entries = Vec::new();
    for (index, entry) in directory.iter().enumerate() {
        if index >= MAX_SCANNED {
            return Err(ListingError::Limit);
        }
        let entry =
            entry.map_err(|error| ListingError::Io(io::Error::from_raw_os_error(error as i32)))?;
        let bytes = entry.file_name().to_bytes();
        let Ok(name) = std::str::from_utf8(bytes) else {
            continue;
        };
        if !valid_name(name) {
            continue;
        }
        let name_path = Path::new(std::ffi::OsStr::from_bytes(bytes));
        verify_entry(root, requested, &stable, name_path, entry.ino())?;
        let opened = root.open(&requested.join(name_path));
        verify_entry(root, requested, &stable, name_path, entry.ino())?;
        let file = match opened {
            Ok(file) => file,
            Err(error)
                if matches!(
                    error.raw_os_error().map(Errno::from_raw),
                    Some(
                        Errno::ENOENT
                            | Errno::ENOTDIR
                            | Errno::EACCES
                            | Errno::EPERM
                            | Errno::EXDEV
                            | Errno::ELOOP
                            | Errno::EAGAIN
                            | Errno::EINVAL
                            | Errno::ENAMETOOLONG
                    )
                ) =>
            {
                continue;
            }
            Err(error) => return Err(ListingError::Io(error)),
        };
        let metadata = file.metadata()?;
        if !metadata.is_file() && !metadata.is_dir() {
            continue;
        }
        if entries.len() >= MAX_VISIBLE {
            return Err(ListingError::Limit);
        }
        entries.push((name.to_owned(), metadata.is_dir()));
    }
    verify_directory(root, requested, &stable)?;
    entries.sort_unstable_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
    let mut output = String::new();
    append(
        &mut output,
        "<!doctype html><html><head><meta charset=\"utf-8\"></head><body><ul>",
    )?;
    for (name, is_directory) in entries {
        append(&mut output, "<li><a href=\"")?;
        append(&mut output, &base)?;
        append(&mut output, &encoded(&name))?;
        if is_directory {
            append(&mut output, "/")?;
        }
        append(&mut output, "\">")?;
        append(&mut output, &escaped(&name))?;
        if is_directory {
            append(&mut output, "/")?;
        }
        append(&mut output, "</a></li>")?;
    }
    append(&mut output, "</ul></body></html>")?;
    Ok(output.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::{ListingError, MAX_BYTES, append, encoded, escaped, valid_name};

    #[test]
    fn names_are_encoded_and_escaped_for_separate_contexts() {
        assert_eq!(encoded("a%/#? \"é"), "a%25%2F%23%3F%20%22%C3%A9");
        assert_eq!(escaped("&<>\"'"), "&amp;&lt;&gt;&quot;&#39;");
        assert!(!valid_name(".hidden"));
        assert!(!valid_name("bad\\name"));
        assert!(!valid_name("bad\nname"));
    }

    #[test]
    fn replaced_directory_and_entry_are_not_mixed() -> Result<(), Box<dyn std::error::Error>> {
        use super::super::root::Root;
        use crate::config::source::{Located, Origin};
        use std::fs;
        use std::os::unix::fs::MetadataExt;
        use std::path::Path;
        let base = std::env::temp_dir().join(format!(
            "rill-listing-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        fs::create_dir_all(base.join("root/dir"))?;
        fs::write(base.join("root/dir/same"), b"old")?;
        let root_path = base.join("root");
        let root = Root::prepare(&Located::new(
            root_path.clone(),
            Origin::file(&base.join("config")),
        ))?;
        let dir = root.open(Path::new("dir"))?;
        let ino = fs::symlink_metadata(root_path.join("dir/same"))?.ino();
        fs::rename(root_path.join("dir"), base.join("old"))?;
        fs::create_dir(root_path.join("dir"))?;
        fs::write(root_path.join("dir/same"), b"new")?;
        assert!(!root.same_directory(Path::new("dir"), &dir)?);
        assert!(!root.same_entry(Path::new("dir/same"), &dir, Path::new("same"), ino)?);
        assert!(matches!(
            super::render(&root, Path::new("dir"), dir),
            Err(super::ListingError::Io(_))
        ));
        fs::remove_dir_all(base)?;
        Ok(())
    }

    #[test]
    fn output_limit_is_checked_before_append() {
        let mut output = "x".repeat(MAX_BYTES);
        assert!(matches!(append(&mut output, "y"), Err(ListingError::Limit)));
        assert_eq!(output.len(), MAX_BYTES);
    }
}
