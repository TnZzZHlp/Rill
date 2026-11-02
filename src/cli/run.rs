use std::path::Path;

pub fn run(path: &Path) -> u8 {
    crate::runtime::server::run(path)
}
