use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use pingora::apps::ServerApp;
use pingora::protocols::l4::listener::Listener;
use pingora::proxy::HttpProxy;
use pingora::server::configuration::ServerConf;
use socket2::{Domain, Protocol, Socket, Type};
use std::net::SocketAddr;
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinSet;
use tokio::time::timeout;

use super::state::{HttpPreparer, RuntimeState};
use crate::config::prepare::build;
use crate::observability::logging::{Level, Logger};
use crate::proxy::Handler;
use crate::proxy::app::{App, Transport};

fn bind(address: SocketAddr) -> std::io::Result<Listener> {
    let socket = Socket::new(
        Domain::for_address(address),
        Type::STREAM,
        Some(Protocol::TCP),
    )?;
    socket.set_reuse_address(true)?;
    if address.is_ipv6() {
        socket.set_only_v6(true)?;
    }
    socket.set_nonblocking(true)?;
    socket.bind(&address.into())?;
    socket.listen(1024)?;
    Ok(Listener::from(tokio::net::TcpListener::from_std(
        socket.into(),
    )?))
}

async fn endpoint_loop(
    endpoint: Listener,
    app: Arc<Transport>,
    mut shutdown: watch::Receiver<bool>,
    capacity: Arc<Semaphore>,
    tls: Option<Arc<crate::tls::resolver::Resolver>>,
) -> Result<(), &'static str> {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => break,
            completed = connections.join_next(), if !connections.is_empty() => {
                if completed.is_some_and(|result| result.is_err()) { return Err("connection task failed"); }
            }
            permit = capacity.clone().acquire_owned() => {
                let permit = permit.map_err(|_| "connection capacity closed")?;
                let stream = tokio::select! {
                    biased;
                    _ = shutdown.changed() => break,
                    stream = endpoint.accept() => stream.map_err(|_| "listener accept failed")?,
                };
                let app = app.clone();
                let shutdown = shutdown.clone();
                let tls = tls.clone();
                connections.spawn(async move {
                    let _permit = permit;
                    let stream: pingora::protocols::Stream = if let Some(tls) = tls {
                        match timeout(Duration::from_secs(10), tls.accept(stream)).await {
                            Ok(Ok(stream)) => stream,
                            _ => {
                                app.app.logger.event(Level::Warn, "tls_handshake_failed", &[]);
                                return;
                            }
                        }
                    } else { Box::new(stream) };
                    app.process_new(stream, &shutdown).await;
                });
            }
        }
    }
    drop(endpoint);
    while let Some(result) = connections.join_next().await {
        if result.is_err() {
            return Err("connection task failed during drain");
        }
    }
    Ok(())
}

pub fn run(path: &Path) -> u8 {
    let prepared = match build(path, &HttpPreparer) {
        Ok(prepared) => prepared,
        Err(error) => {
            Logger::new("info").event(
                Level::Error,
                "configuration_failed",
                &[("diagnostic", &error.to_string())],
            );
            return 1;
        }
    };
    let state = RuntimeState::new(prepared);
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(std::thread::available_parallelism().map_or(2, |count| count.get().min(8)))
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => {
            state
                .snapshot()
                .logger
                .event(Level::Error, "runtime_failed", &[]);
            return 1;
        }
    };
    runtime.block_on(serve(state))
}

pub async fn serve(state: Arc<RuntimeState>) -> u8 {
    let snapshot = state.snapshot();
    let logger = snapshot.logger.clone();
    let mut terminate =
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(signal) => signal,
            Err(_) => {
                logger.event(Level::Error, "signal_setup_failed", &[]);
                return 1;
            }
        };
    let mut interrupt =
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()) {
            Ok(signal) => signal,
            Err(_) => {
                logger.event(Level::Error, "signal_setup_failed", &[]);
                return 1;
            }
        };
    let mut listeners = Vec::new();
    logger.event(
        Level::Info,
        "starting",
        &[("generation", &snapshot.generation.to_string())],
    );
    for (addresses, secure) in [
        (&snapshot.configuration.server.http, false),
        (&snapshot.configuration.server.https, true),
    ] {
        for address in addresses {
            match bind(address.value) {
                Ok(listener) => listeners.push((listener, secure)),
                Err(_) => {
                    logger.event(
                        Level::Error,
                        "listen_failed",
                        &[("address", &address.value.to_string())],
                    );
                    return 1;
                }
            }
        }
    }
    let resolver = match crate::tls::resolver::Resolver::new(state.clone()) {
        Ok(resolver) => resolver,
        Err(_) => {
            logger.event(Level::Error, "tls_context_failed", &[]);
            return 1;
        }
    };
    let configuration = Arc::new(ServerConf {
        threads: 1,
        max_retries: 8,
        ..ServerConf::default()
    });
    let mut proxy = HttpProxy::new(Handler { state }, configuration);
    let mut options = pingora::apps::HttpServerOptions::default();
    options.h2_idle_timeout = Some(Duration::from_secs(60));
    proxy.server_options = Some(options);
    proxy.handle_init_modules();
    let app = Arc::new(Transport {
        app: Arc::new(App {
            proxy: Arc::new(proxy),
            logger: logger.clone(),
        }),
    });
    let capacity = Arc::new(Semaphore::new(1024));
    let (shutdown, receiver) = watch::channel(false);
    let mut workers = JoinSet::new();
    for (endpoint, secure) in listeners {
        workers.spawn(endpoint_loop(
            endpoint,
            app.clone(),
            receiver.clone(),
            capacity.clone(),
            secure.then(|| resolver.clone()),
        ));
    }
    logger.event(
        Level::Info,
        "started",
        &[(
            "listeners",
            &(snapshot.configuration.server.http.len() + snapshot.configuration.server.https.len())
                .to_string(),
        )],
    );
    let mut status = 0;
    tokio::select! {
        _ = terminate.recv() => {}
        _ = interrupt.recv() => {}
        result = workers.join_next() => {
            let _ = result;
            logger.event(Level::Error, "listener_failed", &[]);
            status = 1;
        }
    }
    logger.event(Level::Info, "shutdown_started", &[]);
    let _ = shutdown.send(true);
    app.cleanup().await;
    let drained = timeout(Duration::from_secs(30), async {
        while let Some(result) = workers.join_next().await {
            if !matches!(result, Ok(Ok(()))) {
                status = 1;
            }
        }
    })
    .await;
    if drained.is_err() {
        workers.abort_all();
        while workers.join_next().await.is_some() {}
        logger.event(Level::Warn, "shutdown_deadline", &[]);
        status = 1;
    }
    logger.event(Level::Info, "stopped", &[("status", &status.to_string())]);
    status
}
