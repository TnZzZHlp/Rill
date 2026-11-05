use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use openssl::x509::X509;
use pingora::proxy::{ProxyHttp, Session, http_proxy_service};
use pingora::server::{Server, configuration::ServerConf};
use pingora::upstreams::peer::HttpPeer;

use crate::Result;

struct GrpcProxy(HttpPeer);

#[async_trait]
impl ProxyHttp for GrpcProxy {
    type CTX = ();
    fn new_ctx(&self) {}
    async fn upstream_peer(&self, _: &mut Session, _: &mut ()) -> pingora::Result<Box<HttpPeer>> {
        Ok(Box::new(self.0.clone()))
    }

    async fn logging(&self, _: &mut Session, error: Option<&pingora::Error>, _: &mut ()) {
        if let Some(error) = error {
            eprintln!("proxy failure: {error}");
        }
    }
}

pub fn run(args: &[String]) -> Result {
    if args.len() != 5 {
        return Err("proxy LISTEN UPSTREAM h2c|tls-h2 CA NAME".into());
    }
    let listen: SocketAddr = args[0].parse()?;
    let upstream: SocketAddr = args[1].parse()?;
    if !listen.ip().is_loopback() || !upstream.ip().is_loopback() {
        return Err("acceptance proxy only allows loopback".into());
    }
    let tls = match args[2].as_str() {
        "h2c" => false,
        "tls-h2" => true,
        _ => return Err("invalid protocol".into()),
    };
    let mut peer = HttpPeer::new(upstream, tls, args[4].clone());
    peer.options.set_http_version(2, 2);
    peer.options.max_h2_streams = 16;
    peer.options.connection_timeout = Some(Duration::from_secs(3));
    peer.options.read_timeout = Some(Duration::from_secs(5));
    peer.options.write_timeout = Some(Duration::from_secs(5));
    if tls {
        peer.options.ca = Some(Arc::new(
            X509::stack_from_pem(&std::fs::read(&args[3])?)?.into_boxed_slice(),
        ));
        peer.options.verify_cert = true;
        peer.options.verify_hostname = true;
    }
    let configuration = ServerConf {
        threads: 2,
        grace_period_seconds: Some(0),
        graceful_shutdown_timeout_seconds: Some(2),
        ..ServerConf::default()
    };
    let mut server = Server::new_with_opt_and_conf(None, configuration);
    let mut service = http_proxy_service(&server.configuration, GrpcProxy(peer));
    let mut options = pingora::apps::HttpServerOptions::default();
    options.h2c = true;
    service
        .app_logic_mut()
        .ok_or("missing proxy")?
        .server_options = Some(options);
    service.add_tcp(&listen.to_string());
    server.add_service(service);
    println!("proxy ready: {listen} -> {upstream} {}", args[2]);
    server.run_forever();
}
