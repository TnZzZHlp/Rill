use super::{dns, prepared, target};
use crate::http_policy::model::DynamicProxy;
use crate::runtime::state::RuntimeSnapshot;
use tokio::sync::OwnedSemaphorePermit;

pub struct Ready {
    pub permit: OwnedSemaphorePermit,
    pub url: url::Url,
    pub addresses: Vec<std::net::SocketAddr>,
    pub prefix: String,
    pub site: usize,
}

pub async fn prepare(
    raw_path_and_query: &str,
    dynamic: &DynamicProxy,
    snapshot: &RuntimeSnapshot,
    site: usize,
) -> Result<Ready, u16> {
    let raw = raw_path_and_query
        .strip_prefix(&dynamic.prefix)
        .ok_or(400_u16)?;
    let url = target::parse(raw, &dynamic.ports).map_err(|_| 400_u16)?;
    let permit = prepared::try_active().ok_or(503_u16)?;
    let listeners = snapshot
        .configuration
        .server
        .http
        .iter()
        .chain(&snapshot.configuration.server.https)
        .map(|address| address.value)
        .collect();
    let addresses = dns::resolve(&url, listeners)
        .await
        .map_err(|failure| match failure {
            dns::Failure::Invalid => 400_u16,
            dns::Failure::Denied => 403,
            dns::Failure::Capacity => 503,
            dns::Failure::Unavailable => 502,
        })?;
    Ok(Ready {
        permit,
        url,
        addresses,
        prefix: dynamic.prefix.clone(),
        site,
    })
}
