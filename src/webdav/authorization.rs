use super::{
    model::{DavError, Result},
    path::DavPath,
};
use crate::{
    http_policy::{
        execute::{self, MatchContext, Outcome},
        model::{Action, Matcher, MatcherKind},
        state::RequestPolicyState,
    },
    runtime::state::RuntimeSnapshot,
};
use std::{net::IpAddr, sync::Arc};

struct Uniform {
    domain: Option<String>,
    strips: Vec<String>,
}
impl Uniform {
    fn credit(snapshot: &RuntimeSnapshot, site: usize) -> usize {
        fn matcher_credit(matcher: &Matcher) -> usize {
            match &matcher.kind {
                MatcherKind::PathGlob(pattern) => pattern.len() * 3,
                MatcherKind::All(items) | MatcherKind::Any(items) => {
                    items.iter().map(matcher_credit).sum()
                }
                MatcherKind::Not(item) => matcher_credit(item),
                _ => 0,
            }
        }
        snapshot.configuration.sites[site]
            .chain
            .as_ref()
            .and_then(|chain| chain.rules.first())
            .map_or(0, |rule| {
                2 * rule.actions.len() * std::mem::size_of::<String>()
                    + rule
                        .actions
                        .iter()
                        .filter_map(|action| {
                            if let Action::StripPrefix(prefix) = action {
                                Some(prefix.len())
                            } else {
                                None
                            }
                        })
                        .sum::<usize>()
                    + rule.matcher.as_ref().map_or(0, matcher_credit)
            })
    }
    fn prepare(snapshot: &RuntimeSnapshot, site: usize, index: usize) -> Option<Self> {
        let chain = snapshot.configuration.sites.get(site)?.chain.as_ref()?;
        let rule = chain.rules.first()?;
        let domain = rule.matcher.as_ref().map_or(Some(None), domain)?;
        let mut strips = Vec::new();
        for action in &rule.actions {
            match action {
                Action::BasicAuth(_)
                | Action::StaticToken(_)
                | Action::BodyReplace(_)
                | Action::Encode(_) => (),
                Action::Header(header)
                    if header.side == crate::http_policy::headers::Side::Response => {}
                Action::StripPrefix(prefix) => strips.push(prefix.clone()),
                Action::WebDav(dav)
                    if snapshot
                        .policy
                        .dav_indices
                        .get(&(dav.origin.source.path.clone(), dav.origin.offset))
                        == Some(&index) =>
                {
                    return Some(Self { domain, strips });
                }
                _ => return None,
            }
        }
        None
    }
}
fn domain(matcher: &Matcher) -> Option<Option<String>> {
    match &matcher.kind {
        MatcherKind::Method(_)
        | MatcherKind::HeaderEqual(_, _)
        | MatcherKind::HeaderPrefix(_, _)
        | MatcherKind::ClientCidr(_) => Some(None),
        MatcherKind::PathGlob(pattern) => {
            let prefix = pattern.strip_suffix('*')?;
            if !prefix.ends_with('/') || prefix.contains(['*', '?', '\\']) {
                return None;
            }
            Some(Some(prefix.into()))
        }
        MatcherKind::All(matchers) => {
            let mut result: Option<String> = None;
            for matcher in matchers {
                if let Some(prefix) = domain(matcher)? {
                    if let Some(previous) = &result {
                        if prefix.starts_with(previous) {
                            result = Some(prefix);
                        } else if !previous.starts_with(&prefix) {
                            return None;
                        }
                    } else {
                        result = Some(prefix);
                    }
                }
            }
            Some(result)
        }
        MatcherKind::Any(matchers) => {
            let first = domain(matchers.first()?)?;
            if matchers
                .iter()
                .skip(1)
                .all(|matcher| domain(matcher) == Some(first.clone()))
            {
                Some(first)
            } else {
                None
            }
        }
        MatcherKind::Not(matcher) if domain(matcher) == Some(None) => Some(None),
        _ => None,
    }
}
pub struct Authorization {
    snapshot: Arc<RuntimeSnapshot>,
    site: usize,
    index: usize,
    method: http::Method,
    original: Arc<crate::http_policy::state::EntryRequest>,
    successful: Vec<usize>,
    client_ip: IpAddr,
    scheme: &'static str,
    host: String,
    port: u16,
    entry: DavPath,
    steps_left: u16,
    uniform: Option<Uniform>,
    _memory: super::budget::Memory,
}
impl Authorization {
    pub fn new(
        snapshot: Arc<RuntimeSnapshot>,
        site: usize,
        index: usize,
        method: http::Method,
        state: &RequestPolicyState,
        scheme: &'static str,
        authority: &str,
    ) -> Result<Self> {
        let original = state
            .entry_request()
            .cloned()
            .ok_or_else(DavError::invalid)?;
        let memory = super::budget::Memory::acquire(
            2 * original.path.len()
                + std::mem::size_of_val(state.successful_actions())
                + authority.len()
                + Uniform::credit(&snapshot, site)
                + 512,
        )?;
        let origin = url::Url::parse(&format!("{scheme}://{authority}/"))
            .map_err(|_| DavError::invalid())?;
        let uniform = Uniform::prepare(&snapshot, site, index);
        Ok(Self {
            snapshot,
            site,
            index,
            method,
            original: original.clone(),
            _memory: memory,
            successful: state.successful_actions().to_vec(),
            client_ip: state.client_ip(),
            scheme,
            host: origin.host_str().ok_or_else(DavError::invalid)?.into(),
            port: origin
                .port_or_known_default()
                .ok_or_else(DavError::invalid)?,
            entry: DavPath::parse(&original.path)?,
            steps_left: state.steps_left,
            uniform,
        })
    }
    pub fn check_source(&mut self, source: &DavPath) -> Result<()> {
        let mut href = self.entry.href();
        if self.original.path.ends_with('/') && !href.ends_with('/') {
            href.push('/');
        }
        if self.map(&href)? != *source {
            return Err(DavError::new(403, "DAV_SOURCE_ROUTE"));
        }
        Ok(())
    }
    pub fn href_capacity(&self, source: &DavPath, resource: &DavPath) -> usize {
        3 * (self.entry.key().len() + resource.key().len().saturating_sub(source.key().len()) + 2)
    }
    pub fn href(&mut self, source: &DavPath, resource: &DavPath) -> Result<String> {
        if resource.contains(source) && !source.contains(resource) {
            let count = source.key().split('/').filter(|s| !s.is_empty()).count()
                - resource.key().split('/').filter(|s| !s.is_empty()).count();
            let mut external = self.entry.clone();
            for _ in 0..count {
                external = external.parent();
            }
            let mut href = external.href();
            if resource.is_root() && !href.ends_with('/') {
                href.push('/');
            }
            if self.map(&href)? != *resource {
                return Err(DavError::new(403, "DAV_HREF_ROUTE"));
            }
            return Ok(href);
        }
        self.href_from(&self.entry.clone(), source, resource)
    }
    pub fn href_from(
        &mut self,
        entry: &DavPath,
        source: &DavPath,
        resource: &DavPath,
    ) -> Result<String> {
        if !source.contains(resource) {
            return Err(DavError::invalid());
        }
        let suffix = resource
            .key()
            .strip_prefix(source.key())
            .ok_or_else(DavError::invalid)?
            .trim_start_matches('/');
        let mut external = entry.clone();
        for component in suffix.split('/').filter(|s| !s.is_empty()) {
            external = external.child(component)?;
        }
        let mut href = external.href();
        if resource.is_root() && !href.ends_with('/') {
            href.push('/');
        }
        if self.map(&href)? != *resource {
            return Err(DavError::new(403, "DAV_HREF_ROUTE"));
        }
        Ok(href)
    }
    pub fn map(&mut self, destination: &str) -> Result<DavPath> {
        let uri: http::Uri = destination.parse().map_err(|_| DavError::invalid())?;
        if destination.contains('#') || uri.query().is_some() || uri.path().is_empty() {
            return Err(DavError::invalid());
        }
        if let Some(authority) = uri.authority() {
            if authority.as_str().contains('@') || uri.scheme_str() != Some(self.scheme) {
                return Err(DavError::new(403, "DAV_DESTINATION"));
            }
            let parsed = url::Url::parse(&format!("{}://{}/", self.scheme, authority))
                .map_err(|_| DavError::invalid())?;
            if parsed.host_str() != Some(&self.host)
                || parsed.port_or_known_default() != Some(self.port)
            {
                return Err(DavError::new(403, "DAV_DESTINATION"));
            }
        } else if !destination.starts_with('/')
            || destination.starts_with("//")
            || uri.scheme().is_some()
        {
            return Err(DavError::invalid());
        }
        if let Some(uniform) = &self.uniform
            && uniform
                .domain
                .as_ref()
                .is_none_or(|prefix| uri.path().starts_with(prefix))
        {
            let mut path = uri.path().to_owned();
            for prefix in &uniform.strips {
                path = crate::http_policy::path::strip(&path, prefix)
                    .map_err(|_| DavError::invalid())?
                    .ok_or_else(|| DavError::new(403, "DAV_DESTINATION_ROUTE"))?;
            }
            return DavPath::parse(&path);
        }
        let chain = self.snapshot.configuration.sites[self.site]
            .chain
            .as_ref()
            .ok_or_else(DavError::invalid)?;
        let _proof_memory = super::budget::Memory::acquire(
            self.original
                .headers
                .capacity()
                .checked_mul(128)
                .and_then(|n| n.checked_add(3 * uri.path().len() + 1024 * 1024))
                .ok_or_else(DavError::limit)?,
        )?;
        let mut headers = self.original.headers.clone();
        let mut state = RequestPolicyState::new(
            self.client_ip,
            self.snapshot.configuration.sites[self.site].domains[0]
                .value
                .clone(),
            uri.path().to_owned(),
            self.original.connection_names.clone(),
        );
        state.set_auth_headers(self.snapshot.policy.auth_headers[self.site].clone());
        let mut ctx = MatchContext {
            method: &self.method,
            path: uri.path().into(),
            headers: &mut headers,
            state: Some(&mut state),
            client_ip: self.client_ip,
            steps_left: self.steps_left,
            reentries: 0,
            error_status: None,
            captures: [None; 9],
        };
        let mut cursor = None;
        loop {
            let outcome = execute::execute_resume(chain, &self.snapshot.policy, &mut ctx, cursor);
            self.steps_left = ctx.steps_left;
            match outcome {
                Outcome::Authorize(next) => {
                    let action =
                        execute::suspended_action(chain, &next).ok_or_else(DavError::invalid)?;
                    let identity = std::ptr::from_ref(action) as usize;
                    if !self.successful.contains(&identity) {
                        return Err(DavError::new(403, "DAV_DESTINATION_AUTH"));
                    }
                    ctx.state
                        .as_deref_mut()
                        .ok_or_else(DavError::invalid)?
                        .mark_authenticated(identity);
                    ctx.headers.remove(http::header::AUTHORIZATION);
                    ctx.headers.remove(http::header::PROXY_AUTHORIZATION);
                    if let Action::StaticToken(token) = action {
                        ctx.headers.remove(&token.header);
                    }
                    cursor = Some(*next);
                }
                Outcome::WebDav(index) => {
                    let source = self
                        .snapshot
                        .policy
                        .dav_handles
                        .get(self.index)
                        .ok_or_else(DavError::invalid)?;
                    let target = self
                        .snapshot
                        .policy
                        .dav_handles
                        .get(index)
                        .ok_or_else(DavError::invalid)?;
                    if !Arc::ptr_eq(source, target) {
                        return Err(DavError::new(403, "DAV_DESTINATION_STORAGE"));
                    }
                    return DavPath::parse(&ctx.path);
                }
                Outcome::BudgetExceeded => return Err(DavError::limit()),
                _ => return Err(DavError::new(403, "DAV_DESTINATION_ROUTE")),
            }
        }
    }
}
