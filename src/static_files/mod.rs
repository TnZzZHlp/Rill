pub(crate) mod cache;
mod error_page;
mod listing;
mod path;
mod prepared;
mod range;
mod representation;
mod response;
mod root;
mod serve;
mod validator;

pub use cache::Stats as StaticCacheStats;
pub use prepared::PreparedStatic;
pub use serve::serve;
