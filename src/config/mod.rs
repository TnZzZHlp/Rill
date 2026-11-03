pub mod dependencies;
mod error;
pub mod glob;
mod loader;
pub mod model;
mod parser;
mod preflight;
pub mod prepare;
mod profile;
mod snippet;
pub mod source;
mod validate;

pub use error::{ConfigError, Result};
pub use loader::{LoadedConfig, load, load_tracked};
pub use model::{NormalizedConfig, ParsedConfig, ValidatedConfig};
pub use parser::parse;
pub use profile::normalize;
pub use validate::{check_ip, normalize_domain, normalized_ip, validate};
