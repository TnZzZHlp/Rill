use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use super::source::{Located, Origin};

#[derive(Clone, Debug, Default)]
pub struct Parameters {
    pub connect_timeout: Option<Located<Duration>>,
    pub read_timeout: Option<Located<Duration>>,
    pub request_timeout: Option<Located<Duration>>,
    pub idle_timeout: Option<Located<Duration>>,
    pub websocket: Option<Located<bool>>,
}

#[derive(Clone, Debug)]
pub struct Profile {
    pub name: Located<String>,
    pub uses: Vec<Located<String>>,
    pub parameters: Parameters,
}

#[derive(Clone, Debug)]
pub enum RawAction {
    Proxy(Located<String>),
    Root(Located<PathBuf>),
}

#[derive(Clone, Debug)]
pub struct RawRoute {
    pub pattern: Located<String>,
    pub uses: Vec<Located<String>>,
    pub parameters: Parameters,
    pub action: RawAction,
    pub spa: Option<Located<bool>>,
    pub cache_control: Option<Located<String>>,
    pub precompressed: Option<Located<bool>>,
    pub browse: Option<Located<bool>>,
    pub error_pages: BTreeMap<u16, Located<PathBuf>>,
}

#[derive(Clone, Debug)]
pub struct RawSite {
    pub domains: Vec<Located<String>>,
    pub tls: Option<Located<String>>,
    pub uses: Vec<Located<String>>,
    pub parameters: Parameters,
    pub routes: Vec<RawRoute>,
    pub items: Vec<RouteItem>,
    pub origin: Origin,
}

#[derive(Clone, Debug)]
pub enum RouteItem {
    Route(Box<RawRoute>),
    Snippet(Located<String>),
}

#[derive(Clone, Debug)]
pub struct Snippet {
    pub name: Located<String>,
    pub items: Vec<RouteItem>,
}

#[derive(Clone, Debug)]
pub struct Certificate {
    pub name: Located<String>,
    pub cert: Located<PathBuf>,
    pub key: Located<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObservabilityPolicy {
    pub access_log: bool,
    pub access_sample_per_mille: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObservabilityConfig {
    pub policy: ObservabilityPolicy,
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            policy: ObservabilityPolicy {
                access_log: false,
                access_sample_per_mille: 1000,
            },
        }
    }
}

#[derive(Clone, Debug)]
pub struct Server {
    pub origin: Origin,
    pub http: Vec<Located<SocketAddr>>,
    pub https: Vec<Located<SocketAddr>>,
    pub watch: bool,
    pub static_cache_bytes: usize,
    pub observability: ObservabilityConfig,
    pub redirect_https: bool,
    pub redirect_port: Option<u16>,
    pub control_socket: PathBuf,
    pub log_level: String,
    pub trusted_proxy: Vec<Located<Network>>,
    pub upstream_allow: Vec<Located<Network>>,
}

#[derive(Clone, Debug)]
pub struct Network {
    pub address: IpAddr,
    pub prefix: u8,
}

impl Network {
    pub fn contains(&self, address: IpAddr) -> bool {
        match (self.address, address) {
            (IpAddr::V4(network), IpAddr::V4(address)) => {
                let Some(shift) = 32_u8.checked_sub(self.prefix) else {
                    return false;
                };
                let mask = u32::MAX.checked_shl(u32::from(shift)).unwrap_or(0);
                u32::from(network) & mask == u32::from(address) & mask
            }
            (IpAddr::V6(network), IpAddr::V6(address)) => {
                let Some(shift) = 128_u8.checked_sub(self.prefix) else {
                    return false;
                };
                let mask = u128::MAX.checked_shl(u32::from(shift)).unwrap_or(0);
                u128::from(network) & mask == u128::from(address) & mask
            }
            _ => false,
        }
    }
}

#[derive(Debug)]
pub struct ParsedConfig {
    pub entry: PathBuf,
    pub server: Option<Server>,
    pub profiles: BTreeMap<String, Profile>,
    pub snippets: BTreeMap<String, Snippet>,
    pub certificates: BTreeMap<String, Certificate>,
    pub sites: Vec<RawSite>,
}

#[derive(Debug)]
pub struct NormalizedConfig {
    pub entry: PathBuf,
    pub server: Server,
    pub certificates: BTreeMap<String, Certificate>,
    pub sites: Vec<RawSite>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Upstream {
    pub tls: bool,
    pub host: String,
    pub port: u16,
}

#[derive(Debug)]
pub enum Action {
    Proxy(Upstream),
    Static {
        root: Located<PathBuf>,
        spa: bool,
        cache_control: Option<String>,
        precompressed: bool,
        browse: bool,
        error_pages: BTreeMap<u16, Located<PathBuf>>,
    },
}

#[derive(Debug)]
pub struct Route {
    pub pattern: Located<String>,
    pub action_origin: Origin,
    pub action: Action,
    pub parameters: Parameters,
}

#[derive(Debug)]
pub struct Site {
    pub domains: Vec<Located<String>>,
    pub tls: Option<Located<String>>,
    pub routes: Vec<Route>,
    pub origin: Origin,
}

#[derive(Debug)]
pub struct ValidatedConfig {
    pub entry: PathBuf,
    pub server: Server,
    pub certificates: BTreeMap<String, Certificate>,
    pub sites: Vec<Site>,
}
