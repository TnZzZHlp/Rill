use std::sync::Arc;

use nix::unistd::geteuid;
use tokio::net::UnixStream;
use tokio::sync::watch;
use tokio::task::JoinSet;

use super::protocol::{self, Response};
use super::socket::ControlSocket;
use crate::observability::logging::Level;
use crate::runtime::reload::ReloadCoordinator;
use crate::runtime::state::RuntimeState;

fn error(id: String, code: &str) -> Response {
    Response {
        version: 1,
        id,
        status: "error".into(),
        generation: None,
        code: Some(code.into()),
        errors: Vec::new(),
        truncated: false,
    }
}

async fn respond(stream: &mut UnixStream, mut response: Response) {
    if let Ok(frame) = protocol::encode_response(&mut response) {
        let _ = protocol::write_frame(stream, &frame).await;
    }
}

fn authorized(peer: u32, service: u32) -> bool {
    peer == 0 || peer == service
}

async fn session(
    mut stream: UnixStream,
    coordinator: Arc<ReloadCoordinator>,
    state: Arc<RuntimeState>,
) {
    if !stream
        .peer_cred()
        .is_ok_and(|cred| authorized(cred.uid(), geteuid().as_raw()))
    {
        respond(&mut stream, error("invalid".into(), "UNAUTHORIZED")).await;
        return;
    }
    let request = match protocol::read_frame(&mut stream)
        .await
        .and_then(|frame| protocol::decode_request(&frame))
    {
        Ok(request) => request,
        Err(_) => {
            respond(&mut stream, error("invalid".into(), "PROTOCOL")).await;
            return;
        }
    };
    let reload = coordinator.reload();
    tokio::pin!(reload);
    let result = loop {
        tokio::select! {
            biased;
            ready = stream.readable() => {
                if ready.is_err() { return; }
                let mut byte = [0u8; 1];
                match stream.try_read(&mut byte) {
                    Ok(0) => return,
                    Ok(_) => { respond(&mut stream, error(request.id, "PROTOCOL")).await; return; }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                    Err(_) => return,
                }
            }
            result = &mut reload => break result,
        }
    };
    let response = match result {
        Ok(generation) => {
            state.snapshot().logger.event(
                Level::Info,
                "reload_succeeded",
                &[("id", &request.id), ("generation", &generation.to_string())],
            );
            Response {
                version: 1,
                id: request.id,
                status: "ok".into(),
                generation: Some(generation),
                code: None,
                errors: Vec::new(),
                truncated: false,
            }
        }
        Err(err) => {
            let response = Response::from_config_error(request.id, &err);
            state.snapshot().logger.event(
                Level::Warn,
                "reload_failed",
                &[
                    ("id", &response.id),
                    ("code", response.code.as_deref().unwrap_or("INTERNAL")),
                ],
            );
            response
        }
    };
    respond(&mut stream, response).await;
}

pub async fn serve(
    socket: ControlSocket,
    coordinator: Arc<ReloadCoordinator>,
    state: Arc<RuntimeState>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), &'static str> {
    let mut sessions = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => break,
            result = sessions.join_next(), if !sessions.is_empty() => {
                if result.is_some_and(|r| r.is_err()) { return Err("control session failed"); }
            }
            accepted = socket.listener.accept(), if sessions.len() < 32 => {
                let (stream, _) = accepted.map_err(|_| "control accept failed")?;
                sessions.spawn(session(stream, coordinator.clone(), state.clone()));
            }
        }
    }
    sessions.abort_all();
    while let Some(result) = sessions.join_next().await {
        if result.is_err_and(|e| !e.is_cancelled()) {
            return Err("control session failed");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_service_uid_and_root_are_authorized() {
        assert!(super::authorized(0, 1001));
        assert!(super::authorized(1001, 1001));
        assert!(!super::authorized(1002, 1001));
    }
}
