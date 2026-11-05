use tonic::transport::{Identity, Server, ServerTlsConfig};

use crate::{Result, service::Probe};

pub async fn run(args: &[String]) -> Result {
    if args.len() != 4 {
        return Err("backend CERT KEY".into());
    }
    let identity = Identity::from_pem(std::fs::read(&args[0])?, std::fs::read(&args[1])?);
    let mut service = Probe::default();
    service.backend_id = "one";
    let mut second = Probe::default();
    second.backend_id = "two";
    let second_identity = Identity::from_pem(std::fs::read(&args[2])?, std::fs::read(&args[3])?);
    let second_tls = Server::builder()
        .tls_config(ServerTlsConfig::new().identity(second_identity))?
        .add_service(second.clone())
        .serve_with_shutdown("0.0.0.0:50054".parse()?, shutdown());
    let other = Server::builder()
        .add_service(second)
        .serve_with_shutdown("0.0.0.0:50053".parse()?, shutdown());
    let plain = Server::builder()
        .add_service(service.clone())
        .serve_with_shutdown("0.0.0.0:50051".parse()?, shutdown());
    let tls = Server::builder()
        .tls_config(ServerTlsConfig::new().identity(identity))?
        .add_service(service)
        .serve_with_shutdown("0.0.0.0:50052".parse()?, shutdown());
    println!("backend ready: h2c=50051 tls-h2=50052");
    tokio::try_join!(plain, tls, other, second_tls)?;
    Ok(())
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
}
