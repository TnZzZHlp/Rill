use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use pingora::http::ResponseHeader;
use tokio::sync::Semaphore;

use crate::config::model::{Action, Route};
use crate::config::{ConfigError, Result};

use super::error_page::ErrorPage;
use super::root::Root;

#[derive(Debug)]
pub struct PreparedStatic {
    pub(crate) root: Root,
    pub(crate) index: String,
    pub(crate) cache: Option<super::cache::Binding>,
    pub(crate) spa: bool,
    pub(crate) precompressed: bool,
    pub(crate) browse: bool,
    pub(crate) error_pages: BTreeMap<u16, ErrorPage>,
    pub(crate) cache_control: Option<String>,
    pub(crate) capacity: Arc<Semaphore>,
    pub request: Duration,
    pub idle: Duration,
}

impl PreparedStatic {
    pub fn prepare(route: &Route, capacity: Arc<Semaphore>) -> Result<Self> {
        let Action::Static {
            root,
            index,
            spa,
            cache_control,
            precompressed,
            browse,
            error_pages,
        } = &route.action
        else {
            return Err(ConfigError::new(
                "INTERNAL",
                "root",
                "static action required",
                &route.action_origin,
            ));
        };
        if let Some(value) = cache_control {
            let invalid = || {
                ConfigError::new(
                    "CONFIG_TYPE",
                    "cache-control",
                    "invalid cache-control header value",
                    &route.action_origin,
                )
            };
            let mut response = ResponseHeader::build(200, None).map_err(|_| invalid())?;
            response
                .insert_header("cache-control", value.as_str())
                .map_err(|_| invalid())?;
        }
        Ok(Self {
            root: Root::prepare(root)?,
            index: index.clone(),
            cache: None,
            spa: *spa,
            precompressed: *precompressed,
            browse: *browse,
            error_pages: error_pages
                .iter()
                .map(|(&status, path)| Ok((status, ErrorPage::prepare(path)?)))
                .collect::<Result<_>>()?,
            cache_control: cache_control.clone(),
            capacity,
            request: route
                .parameters
                .request_timeout
                .as_ref()
                .map_or(Duration::from_secs(300), |value| value.value),
            idle: route
                .parameters
                .idle_timeout
                .as_ref()
                .map_or(Duration::from_secs(60), |value| value.value),
        })
    }
}
