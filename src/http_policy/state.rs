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
enum FilterPolicy {
    Header(Arc<HeaderPolicy>),
    Replace(Arc<crate::body_transform::model::Replace>),
    Encode(Arc<crate::body_transform::model::Encode>),
}
struct ResponseFilter {
    policy: FilterPolicy,
    captures: Option<Arc<CaptureGroup>>,
}

pub struct RequestPolicyState {
    client_ip: IpAddr,
    site: String,
    path: String,
    upstream_authority: Option<String>,
    connection_names: Vec<String>,
    auth_headers: Arc<[http::HeaderName]>,
    authenticated: bool,
    authenticated_actions: Vec<usize>,
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
            auth_headers: Arc::from([]),
            authenticated: false,
            authenticated_actions: Vec::new(),
            captures: None,
            filters: Vec::new(),
            steps_left: 1024,
            reentries: 0,
            error_entered: false,
        }
    }
    pub fn mark_authenticated(&mut self, action: usize) {
        self.authenticated = true;
        if !self.authenticated_actions.contains(&action) {
            self.authenticated_actions.push(action);
        }
    }
    pub fn authenticated_action(&self, action: usize) -> bool {
        self.authenticated_actions.contains(&action)
    }
    pub fn authenticated(&self) -> bool {
        self.authenticated
    }
    pub fn set_auth_headers(&mut self, headers: Arc<[http::HeaderName]>) {
        self.auth_headers = headers;
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
                    &self.auth_headers,
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
                    policy: FilterPolicy::Header(policy.clone()),
                    captures: self.captures.clone(),
                });
                Ok(())
            }
        }
    }
    fn register_body(&mut self, policy: FilterPolicy) -> Result<(), PolicyError> {
        let rules = self
            .filters
            .iter()
            .map(|filter| match &filter.policy {
                FilterPolicy::Replace(r) => r.rules.len(),
                _ => 0,
            })
            .sum::<usize>();
        if self.filters.len() >= FILTER_LIMIT
            || (self.filters.len() + 1) * std::mem::size_of::<ResponseFilter>() > METADATA_LIMIT
        {
            return Err(PolicyError);
        }
        match &policy {
            FilterPolicy::Replace(r) if rules + r.rules.len() > 16 => return Err(PolicyError),
            FilterPolicy::Encode(_)
                if self
                    .filters
                    .iter()
                    .any(|f| matches!(f.policy, FilterPolicy::Encode(_))) =>
            {
                return Err(PolicyError);
            }
            _ => (),
        }
        self.filters.try_reserve_exact(1).map_err(|_| PolicyError)?;
        self.filters.push(ResponseFilter {
            policy,
            captures: self.captures.clone(),
        });
        Ok(())
    }
    pub fn register_replace(
        &mut self,
        policy: &Arc<crate::body_transform::model::Replace>,
    ) -> Result<(), PolicyError> {
        self.register_body(FilterPolicy::Replace(policy.clone()))
    }
    pub fn register_encode(
        &mut self,
        policy: &Arc<crate::body_transform::model::Encode>,
    ) -> Result<(), PolicyError> {
        self.register_body(FilterPolicy::Encode(policy.clone()))
    }
    pub fn has_body_transform(&self) -> bool {
        self.filters
            .iter()
            .any(|f| !matches!(f.policy, FilterPolicy::Header(_)))
    }
    pub fn body_declarations(
        &self,
    ) -> (
        Vec<Arc<crate::body_transform::model::Replace>>,
        Option<Arc<crate::body_transform::model::Encode>>,
    ) {
        let replacements = self
            .filters
            .iter()
            .filter_map(|f| {
                if let FilterPolicy::Replace(r) = &f.policy {
                    Some(r.clone())
                } else {
                    None
                }
            })
            .collect();
        let encode = self.filters.iter().find_map(|f| {
            if let FilterPolicy::Encode(e) = &f.policy {
                Some(e.clone())
            } else {
                None
            }
        });
        (replacements, encode)
    }
    pub fn body_rules(
        &self,
        mime: Option<&str>,
    ) -> Vec<Arc<crate::body_transform::model::Replace>> {
        self.filters
            .iter()
            .filter_map(|filter| match &filter.policy {
                FilterPolicy::Replace(r)
                    if crate::body_transform::model::matches(&r.mimes, mime) =>
                {
                    Some(r.clone())
                }
                _ => None,
            })
            .collect()
    }
    pub fn body_encode(
        &self,
        mime: Option<&str>,
    ) -> Option<Arc<crate::body_transform::model::Encode>> {
        self.filters.iter().find_map(|filter| match &filter.policy {
            FilterPolicy::Encode(e) if crate::body_transform::model::matches(&e.mimes, mime) => {
                Some(e.clone())
            }
            _ => None,
        })
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
            if let FilterPolicy::Header(policy) = &filter.policy
                && policy.applies(status, mime)
            {
                policy
                    .apply(
                        &mut headers,
                        &self.context(filter.captures.as_ref()),
                        connection_names,
                        &self.auth_headers,
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
