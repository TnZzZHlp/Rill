use super::{
    headers::{HeaderPolicy, Side},
    template,
};
use http::HeaderMap;
use pingora::http::ResponseHeader;
use std::{net::IpAddr, sync::Arc};

#[derive(Debug)]
pub struct PolicyError;

const CAPTURE_LIMIT: usize = 36 * 1024;
const FILTER_LIMIT: usize = 256;
const METADATA_LIMIT: usize = 64 * 1024;

struct CaptureGroup {
    values: [Option<String>; 9],
    bytes: usize,
}
struct ResponseFilter {
    policy: Arc<HeaderPolicy>,
    captures: Option<Arc<CaptureGroup>>,
}

pub struct RequestPolicyState {
    client_ip: IpAddr,
    site: String,
    path: String,
    upstream_authority: Option<String>,
    connection_names: Vec<String>,
    captures: Option<Arc<CaptureGroup>>,
    filters: Vec<ResponseFilter>,
    pub steps_left: u16,
    pub reentries: u8,
    pub error_entered: bool,
}
impl RequestPolicyState {
    pub fn new(
        client_ip: IpAddr,
        site: String,
        path: String,
        connection_names: Vec<String>,
    ) -> Self {
        Self {
            client_ip,
            site,
            path,
            upstream_authority: None,
            connection_names,
            captures: None,
            filters: Vec::new(),
            steps_left: 1024,
            reentries: 0,
            error_entered: false,
        }
    }
    pub fn path(&self) -> &str {
        &self.path
    }
    pub fn set_path(&mut self, path: String) {
        self.path = path;
    }
    pub(crate) fn expand_path(
        &self,
        template: &super::path::PathTemplate,
        limit: usize,
    ) -> Result<String, ()> {
        let empty = std::array::from_fn(|_| None);
        let captures = self.captures.as_ref().map_or(&empty, |group| &group.values);
        template.expand(&self.path, captures, limit).map_err(|_| ())
    }
    pub(crate) fn rewrite(&mut self, template: &super::path::PathTemplate) -> Result<String, ()> {
        let path = self.expand_path(template, 64 * 1024)?;
        self.path.clone_from(&path);
        Ok(path)
    }
    pub fn set_upstream_authority(&mut self, authority: String) {
        self.upstream_authority = Some(authority);
    }
    pub fn set_captures(&mut self, captures: [Option<&str>; 9]) -> Result<(), PolicyError> {
        self.captures = None;
        let mut bytes = captures
            .iter()
            .flatten()
            .try_fold(0usize, |n, s| n.checked_add(s.len()))
            .ok_or(PolicyError)?;
        for (i, filter) in self.filters.iter().enumerate() {
            if let Some(group) = &filter.captures
                && !self.filters[..i].iter().any(|other| {
                    other
                        .captures
                        .as_ref()
                        .is_some_and(|g| Arc::ptr_eq(g, group))
                })
            {
                bytes = bytes.checked_add(group.bytes).ok_or(PolicyError)?;
            }
        }
        if bytes > CAPTURE_LIMIT {
            return Err(PolicyError);
        }
        if captures.iter().any(Option::is_some) {
            let own_bytes = captures.iter().flatten().map(|s| s.len()).sum();
            self.captures = Some(Arc::new(CaptureGroup {
                values: captures.map(|s| s.map(str::to_owned)),
                bytes: own_bytes,
            }));
        }
        Ok(())
    }
    fn context<'a>(&'a self, captures: Option<&'a Arc<CaptureGroup>>) -> template::Context<'a> {
        template::Context {
            client_ip: self.client_ip,
            site: &self.site,
            path: &self.path,
            upstream_authority: self.upstream_authority.as_deref(),
            captures: std::array::from_fn(|i| captures.and_then(|c| c.values[i].as_deref())),
        }
    }
    pub fn apply_header(
        &mut self,
        policy: &Arc<HeaderPolicy>,
        headers: &mut HeaderMap,
    ) -> Result<(), PolicyError> {
        match policy.side {
            Side::Request => policy
                .apply(
                    headers,
                    &self.context(self.captures.as_ref()),
                    &self.connection_names,
                    &[],
                )
                .map_err(|_| PolicyError),
            Side::Response => {
                let count = self.filters.len().checked_add(1).ok_or(PolicyError)?;
                if count > FILTER_LIMIT
                    || count
                        .checked_mul(std::mem::size_of::<ResponseFilter>())
                        .is_none_or(|n| n > METADATA_LIMIT)
                {
                    return Err(PolicyError);
                }
                self.filters.try_reserve_exact(1).map_err(|_| PolicyError)?;
                self.filters.push(ResponseFilter {
                    policy: policy.clone(),
                    captures: self.captures.clone(),
                });
                Ok(())
            }
        }
    }
    pub fn apply_response(
        &self,
        response: &mut ResponseHeader,
        connection_names: &[String],
    ) -> Result<(), PolicyError> {
        super::headers::check(&response.headers).map_err(|_| PolicyError)?;
        let status = response.status.as_u16();
        let mime = response
            .headers
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        self.apply_response_with_metadata(response, connection_names, status, mime.as_deref())
    }
    pub fn apply_response_with_metadata(
        &self,
        response: &mut ResponseHeader,
        connection_names: &[String],
        status: u16,
        mime: Option<&str>,
    ) -> Result<(), PolicyError> {
        super::headers::check(&response.headers).map_err(|_| PolicyError)?;
        let mut headers = response.headers.clone();
        for filter in &self.filters {
            if filter.policy.applies(status, mime) {
                filter
                    .policy
                    .apply(
                        &mut headers,
                        &self.context(filter.captures.as_ref()),
                        connection_names,
                        &[],
                    )
                    .map_err(|_| PolicyError)?;
            }
        }
        let names: Vec<_> = response.headers.keys().cloned().collect();
        for name in names {
            response.remove_header(&name);
        }
        for (name, value) in &headers {
            response
                .append_header(name, value)
                .map_err(|_| PolicyError)?;
        }
        Ok(())
    }
}
