use super::model::{Action, Matcher, MatcherKind};
use crate::config::{ConfigError, Result, model::ValidatedConfig, source::Origin};
use regex_automata::nfa::thompson::{self, pikevm::PikeVM};
use regex_syntax::ast::{Ast, RepetitionKind, RepetitionRange};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

const MIB: usize = 1024 * 1024;
static POLICY_BYTES: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug)]
pub struct PreparedPolicy {
    pub regexes: Vec<CompiledRegex>,
    pub indices: BTreeMap<(PathBuf, usize), usize>,
    pub roots: BTreeMap<(PathBuf, usize), crate::static_files::root::Root>,
    pub filesystem_sites: Vec<bool>,
    bytes: usize,
}
#[derive(Debug)]
pub struct CompiledRegex {
    pub vm: PikeVM,
}

impl Drop for PreparedPolicy {
    fn drop(&mut self) {
        drop(std::mem::take(&mut self.regexes));
        drop(std::mem::take(&mut self.indices));
        drop(std::mem::take(&mut self.roots));
        drop(std::mem::take(&mut self.filesystem_sites));
        POLICY_BYTES.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}
struct CompilePermit(usize);
impl CompilePermit {
    fn acquire(bytes: usize, origin: &Origin) -> Result<Self> {
        POLICY_BYTES
            .try_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                old.checked_add(bytes).filter(|n| *n <= 64 * MIB)
            })
            .map_err(|_| error("POLICY_LIMIT", origin))?;
        Ok(Self(bytes))
    }

    fn retain(&mut self, policy: &mut PreparedPolicy, bytes: usize, origin: &Origin) -> Result<()> {
        let total = policy
            .bytes
            .checked_add(bytes)
            .filter(|n| *n <= 8 * MIB)
            .ok_or_else(|| error("POLICY_LIMIT", origin))?;
        if bytes > self.0 {
            return Err(error("POLICY_INVALID_REGEX", origin));
        }
        POLICY_BYTES.fetch_sub(self.0 - bytes, Ordering::AcqRel);
        self.0 = 0;
        policy.bytes = total;
        Ok(())
    }
}
impl Drop for CompilePermit {
    fn drop(&mut self) {
        POLICY_BYTES.fetch_sub(self.0, Ordering::AcqRel);
    }
}
fn error(code: &'static str, origin: &Origin) -> ConfigError {
    ConfigError::new(
        code,
        "policy",
        "policy compilation limit or invalid expression",
        origin,
    )
}
impl PreparedPolicy {
    fn reserve(&mut self, bytes: usize, origin: &Origin) -> Result<()> {
        let total = self
            .bytes
            .checked_add(bytes)
            .filter(|n| *n <= 8 * MIB)
            .ok_or_else(|| error("POLICY_LIMIT", origin))?;
        POLICY_BYTES
            .try_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                old.checked_add(bytes).filter(|n| *n <= 64 * MIB)
            })
            .map_err(|_| error("POLICY_LIMIT", origin))?;
        self.bytes = total;
        Ok(())
    }
}
fn repeat_ok(ast: &Ast) -> bool {
    match ast {
        Ast::Repetition(r) => {
            let ok = match r.op.kind {
                RepetitionKind::Range(
                    RepetitionRange::Exactly(n) | RepetitionRange::AtLeast(n),
                ) => n <= 1000,
                RepetitionKind::Range(RepetitionRange::Bounded(a, b)) => a <= 1000 && b <= 1000,
                _ => true,
            };
            ok && repeat_ok(&r.ast)
        }
        Ast::Group(g) => repeat_ok(&g.ast),
        Ast::Alternation(a) => a.asts.iter().all(repeat_ok),
        Ast::Concat(c) => c.asts.iter().all(repeat_ok),
        _ => true,
    }
}
fn glob(pattern: &str, origin: &Origin) -> Result<String> {
    if pattern.len() > 1024 {
        return Err(error("POLICY_LIMIT", origin));
    }
    let mut out = String::from("\\A(?:");
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        match c {
            '*' => out.push_str("(?s:.*)"),
            '?' => out.push_str("(?s:.)"),
            '\\' => {
                let c = chars
                    .next()
                    .ok_or_else(|| error("POLICY_INVALID_MATCHER", origin))?;
                out.push_str(&regex_syntax::escape(&c.to_string()));
            }
            c => out.push_str(&regex_syntax::escape(&c.to_string())),
        }
    }
    out.push_str(")\\z");
    Ok(out)
}
fn compile_regex(
    p: &mut PreparedPolicy,
    pattern: &str,
    origin: &Origin,
    check_repeat: bool,
) -> Result<()> {
    if check_repeat && pattern.len() > 4096 {
        return Err(error("POLICY_INVALID_REGEX", origin));
    }
    let ast = regex_syntax::ast::parse::Parser::new()
        .parse(pattern)
        .map_err(|_| error("POLICY_INVALID_REGEX", origin))?;
    if check_repeat && !repeat_ok(&ast) {
        return Err(error("POLICY_INVALID_REGEX", origin));
    }
    drop(ast);
    let wrapper = std::mem::size_of::<PikeVM>()
        .saturating_add(std::mem::size_of::<CompiledRegex>())
        .saturating_add(origin.source.path.capacity())
        .saturating_add(128);
    if std::mem::size_of::<thompson::State>() > 4 * std::mem::size_of::<usize>() {
        return Err(error("POLICY_INVALID_REGEX", origin));
    }
    let metadata = pattern
        .len()
        .checked_add(1)
        .and_then(|len| len.checked_mul(64))
        .and_then(|bytes| bytes.checked_add(4096))
        .ok_or_else(|| error("POLICY_LIMIT", origin))?;
    let admission = (2 * MIB)
        .checked_add(metadata)
        .and_then(|bytes| bytes.checked_add(wrapper))
        .ok_or_else(|| error("POLICY_LIMIT", origin))?;
    let mut permit = CompilePermit::acquire(admission, origin)?;
    let vm = PikeVM::builder()
        .thompson(thompson::Config::new().nfa_size_limit(Some(2 * MIB)))
        .build(pattern)
        .map_err(|_| error("POLICY_INVALID_REGEX", origin))?;
    let nfa_bytes = vm.get_nfa().memory_usage();
    if nfa_bytes > 2 * MIB {
        return Err(error("POLICY_INVALID_REGEX", origin));
    }
    permit.retain(p, nfa_bytes.saturating_add(wrapper), origin)?;
    p.indices
        .insert((origin.source.path.clone(), origin.offset), p.regexes.len());
    p.regexes.push(CompiledRegex { vm });
    Ok(())
}
fn origin_bytes(origin: &Origin) -> usize {
    origin
        .references
        .capacity()
        .saturating_mul(std::mem::size_of::<Origin>())
        .saturating_add(origin.references.iter().map(origin_bytes).sum::<usize>())
}
fn visit_matcher(p: &mut PreparedPolicy, m: &Matcher) -> Result<()> {
    p.reserve(
        std::mem::size_of::<Matcher>().saturating_add(origin_bytes(&m.origin)),
        &m.origin,
    )?;
    match &m.kind {
        MatcherKind::All(ms) | MatcherKind::Any(ms) => {
            p.reserve(
                ms.capacity()
                    .saturating_sub(ms.len())
                    .saturating_mul(std::mem::size_of::<Matcher>()),
                &m.origin,
            )?;
            for m in ms {
                visit_matcher(p, m)?;
            }
        }
        MatcherKind::Not(m) => visit_matcher(p, m)?,
        MatcherKind::PathRegex(s) => {
            p.reserve(s.capacity(), &m.origin)?;
            compile_regex(p, s, &m.origin, true)?;
        }
        MatcherKind::PathGlob(s) => {
            p.reserve(s.capacity(), &m.origin)?;
            compile_regex(p, &glob(s, &m.origin)?, &m.origin, false)?;
        }
        MatcherKind::HeaderEqual(a, b) | MatcherKind::HeaderPrefix(a, b) => {
            p.reserve(a.capacity() + b.capacity(), &m.origin)?
        }
        MatcherKind::Method(s) => p.reserve(s.capacity(), &m.origin)?,
        MatcherKind::ClientCidr(_) | MatcherKind::ErrorStatus(_) => (),
    }
    Ok(())
}
fn visit_action(p: &mut PreparedPolicy, a: &Action, origin: &Origin) -> Result<()> {
    match a {
        Action::Header(header) => p.reserve(
            header
                .allocated_bytes()
                .saturating_add(std::mem::size_of::<super::headers::HeaderPolicy>())
                .saturating_add(2 * std::mem::size_of::<usize>()),
            origin,
        )?,
        Action::Rewrite(template) => p.reserve(template.allocated_bytes(), origin)?,
        Action::StripPrefix(prefix) => p.reserve(prefix.capacity(), origin)?,
        Action::TryFiles { root, candidates } => {
            let bytes = root
                .value
                .as_os_str()
                .len()
                .saturating_add(origin_bytes(&root.origin))
                .saturating_add(
                    candidates
                        .capacity()
                        .saturating_mul(std::mem::size_of::<super::path::PathTemplate>()),
                )
                .saturating_add(
                    candidates
                        .iter()
                        .map(super::path::PathTemplate::allocated_bytes)
                        .sum::<usize>(),
                );
            p.reserve(bytes, &root.origin)?;
            let key = (root.origin.source.path.clone(), root.origin.offset);
            if !p.roots.contains_key(&key) {
                p.reserve(
                    root.origin.source.path.as_os_str().len().saturating_add(
                        std::mem::size_of::<crate::static_files::root::Root>() + 128,
                    ),
                    &root.origin,
                )?;
                let prepared = crate::static_files::root::Root::prepare(root)?;
                p.roots.insert(key, prepared);
            }
        }
        Action::Continue | Action::Reenter => (),
        Action::Route { raw, .. } => {
            let payload = match &raw.action {
                crate::config::model::RawAction::Proxy(value) => value.value.capacity(),
                crate::config::model::RawAction::Root(value) => value.value.capacity(),
            };
            let raw_extra = raw
                .pattern
                .value
                .capacity()
                .saturating_add(origin_bytes(&raw.pattern.origin))
                .saturating_add(raw.index.as_ref().map_or(0, |v| {
                    v.value.capacity().saturating_add(origin_bytes(&v.origin))
                }))
                .saturating_add(raw.proxy_host.as_ref().map_or(0, |v| {
                    v.value.capacity().saturating_add(origin_bytes(&v.origin))
                }))
                .saturating_add(raw.cache_control.as_ref().map_or(0, |v| {
                    v.value.capacity().saturating_add(origin_bytes(&v.origin))
                }));
            p.reserve(
                std::mem::size_of::<crate::config::model::RawRoute>()
                    .saturating_add(payload)
                    .saturating_add(raw_extra),
                origin,
            )?;
        }
        Action::Respond {
            text, content_type, ..
        } => p.reserve(text.capacity() + content_type.capacity(), origin)?,
        Action::Branch(bs) => {
            p.reserve(
                bs.capacity()
                    .saturating_sub(bs.len())
                    .saturating_mul(std::mem::size_of::<super::model::Branch>()),
                origin,
            )?;
            for b in bs {
                p.reserve(
                    std::mem::size_of::<super::model::Branch>()
                        .saturating_add(origin_bytes(&b.origin)),
                    &b.origin,
                )?;
                if let Some(m) = &b.matcher {
                    visit_matcher(p, m)?;
                }
                visit_sequence(p, &b.actions, &b.origin)?;
            }
        }
    }
    Ok(())
}
fn visit_sequence(p: &mut PreparedPolicy, actions: &Vec<Action>, origin: &Origin) -> Result<()> {
    p.reserve(
        actions
            .capacity()
            .saturating_mul(std::mem::size_of::<Action>()),
        origin,
    )?;
    for action in actions {
        visit_action(p, action, origin)?;
    }
    Ok(())
}
fn has_filesystem(actions: &[Action]) -> bool {
    actions.iter().any(|action| match action {
        Action::TryFiles { .. } => true,
        Action::Branch(branches) => branches
            .iter()
            .any(|branch| has_filesystem(&branch.actions)),
        _ => false,
    })
}
pub fn compile(config: &ValidatedConfig) -> Result<PreparedPolicy> {
    let mut p = PreparedPolicy {
        regexes: Vec::new(),
        indices: BTreeMap::new(),
        roots: BTreeMap::new(),
        filesystem_sites: vec![false; config.sites.len()],
        bytes: 0,
    };
    if let Some(site) = config.sites.first() {
        p.reserve(p.filesystem_sites.capacity(), &site.origin)?;
    }
    let mut sites = 0;
    for (site_index, site) in config.sites.iter().enumerate() {
        if let Some(chain) = &site.chain {
            p.filesystem_sites[site_index] = chain
                .rules
                .iter()
                .chain(chain.errors.iter().flatten())
                .any(|rule| has_filesystem(&rule.actions));
            sites += 1;
            if sites > 128 {
                return Err(error("POLICY_LIMIT", &chain.origin));
            }
            p.reserve(
                std::mem::size_of::<super::model::Chain>()
                    .saturating_add(origin_bytes(&chain.origin))
                    .saturating_add(
                        chain
                            .rules
                            .capacity()
                            .saturating_sub(chain.rules.len())
                            .saturating_mul(std::mem::size_of::<super::model::Rule>()),
                    ),
                &chain.origin,
            )?;
            p.reserve(
                site.routes
                    .capacity()
                    .saturating_mul(std::mem::size_of::<crate::config::model::Route>()),
                &chain.origin,
            )?;
            for route in &site.routes {
                let params = &route.parameters;
                let refs = [
                    &params.connect_timeout,
                    &params.read_timeout,
                    &params.request_timeout,
                    &params.idle_timeout,
                ]
                .into_iter()
                .flatten()
                .map(|v| origin_bytes(&v.origin))
                .sum::<usize>()
                .saturating_add(
                    params
                        .websocket
                        .as_ref()
                        .map_or(0, |v| origin_bytes(&v.origin)),
                );
                let action_bytes = match &route.action {
                    crate::config::model::Action::Proxy(upstream) => upstream.host.capacity(),
                    crate::config::model::Action::Static {
                        root,
                        index,
                        cache_control,
                        ..
                    } => root
                        .value
                        .as_os_str()
                        .len()
                        .saturating_add(origin_bytes(&root.origin))
                        .saturating_add(index.capacity())
                        .saturating_add(cache_control.as_ref().map_or(0, String::capacity)),
                };
                p.reserve(
                    route
                        .pattern
                        .value
                        .capacity()
                        .saturating_add(origin_bytes(&route.pattern.origin))
                        .saturating_add(origin_bytes(&route.action_origin))
                        .saturating_add(route.proxy_host.as_ref().map_or(0, String::capacity))
                        .saturating_add(action_bytes)
                        .saturating_add(refs),
                    &chain.origin,
                )?;
            }
            if let Some(errors) = &chain.errors {
                p.reserve(
                    errors
                        .capacity()
                        .saturating_sub(errors.len())
                        .saturating_mul(std::mem::size_of::<super::model::Rule>()),
                    &chain.origin,
                )?;
            }
            for rule in chain.rules.iter().chain(chain.errors.iter().flatten()) {
                p.reserve(
                    std::mem::size_of::<super::model::Rule>()
                        + rule.id.as_ref().map_or(0, String::capacity)
                        + origin_bytes(&rule.origin),
                    &rule.origin,
                )?;
                if let Some(m) = &rule.matcher {
                    visit_matcher(&mut p, m)?;
                }
                visit_sequence(&mut p, &rule.actions, &rule.origin)?;
            }
        }
    }
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regex_build_admission_uses_final_candidate_size() -> Result<()> {
        let origin = Origin::file(std::path::Path::new("policy.kdl"));
        let mut policy = PreparedPolicy {
            regexes: Vec::new(),
            indices: BTreeMap::new(),
            roots: BTreeMap::new(),
            filesystem_sites: Vec::new(),
            bytes: 0,
        };
        policy.reserve(8 * MIB - 256 * 1024, &origin)?;
        for index in 0..5 {
            let mut origin = origin.clone();
            origin.offset = index;
            compile_regex(&mut policy, "^/x$", &origin, true)?;
        }
        assert_eq!(policy.regexes.len(), 5);
        assert!(policy.bytes < 8 * MIB);
        Ok(())
    }
}
