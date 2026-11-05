use tonic::transport::{Identity, Server, ServerTlsConfig};

use crate::{Result, service::Probe};

pub async fn run(args: &[String]) -> Result {
    if args.len() != 2 {
        return Err("backend CERT KEY".into());
    }
    let identity = Identity::from_pem(std::fs::read(&args[0])?, std::fs::read(&args[1])?);
    let service = Probe::default();
    let plain = Server::builder()
        .add_service(service.clone())
        .serve_with_shutdown("0.0.0.0:50051".parse()?, shutdown());
    let tls = Server::builder()
        .tls_config(ServerTlsConfig::new().identity(identity))?
        .add_service(service)
        .serve_with_shutdown("0.0.0.0:50052".parse()?, shutdown());
    println!("backend ready: h2c=50051 tls-h2=50052");
    tokio::try_join!(plain, tls)?;
    Ok(())
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
}
