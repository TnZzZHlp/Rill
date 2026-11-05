use crate::{
    http_policy::{
        execute::{Cursor, suspended_action},
        model::Action,
    },
    runtime::state::RuntimeSnapshot,
};
use argon2::{
    Argon2,
    password_hash::{PasswordHash, PasswordVerifier},
};
use base64::Engine;
use http::{HeaderMap, header::AUTHORIZATION};
use std::{
    sync::{Arc, OnceLock},
    time::Duration,
};
use tokio::sync::Semaphore;

static CAPACITY: OnceLock<Arc<Semaphore>> = OnceLock::new();
const DUMMY: &str = "$2b$10$EGdrhbKUv8Oc9vGiXX0HQOxSg445d458Muh7DAHskb6QbtCvdxcie";

pub enum Result {
    Success,
    Invalid(Option<String>),
    Capacity,
    Timeout,
}

fn single<'a>(headers: &'a HeaderMap, name: &http::HeaderName) -> Option<&'a [u8]> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    Some(value.as_bytes())
}

pub async fn authorize(
    snapshot: Arc<RuntimeSnapshot>,
    site: usize,
    cursor: &Cursor,
    headers: &HeaderMap,
) -> Result {
    let Some(chain) = snapshot
        .configuration
        .sites
        .get(site)
        .and_then(|s| s.chain.as_ref())
    else {
        return Result::Capacity;
    };
    let Some(action) = suspended_action(chain, cursor) else {
        return Result::Capacity;
    };
    match action {
        Action::StaticToken(token) => {
            let value = single(headers, &token.header);
            let secret = snapshot
                .policy
                .tokens
                .get(&(token.origin.source.path.clone(), token.origin.offset));
            if let (Some(value), Some(secret)) = (value, secret)
                && !value.is_empty()
                && value.len() <= 4096
                && openssl::memcmp::eq(&openssl::sha::sha256(value), &secret.0)
            {
                return Result::Success;
            }
            Result::Invalid(None)
        }
        Action::BasicAuth(basic) => {
            let challenge = Some(basic.challenge.clone());
            let Some(input) = single(headers, &AUTHORIZATION).filter(|v| v.len() <= 4096) else {
                return Result::Invalid(challenge);
            };
            let Some(space) = input.iter().position(|b| *b == b' ') else {
                return Result::Invalid(challenge);
            };
            if !input[..space].eq_ignore_ascii_case(b"basic") {
                return Result::Invalid(challenge);
            }
            let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(&input[space + 1..])
            else {
                return Result::Invalid(challenge);
            };
            let Some(colon) = decoded.iter().position(|&b| b == b':') else {
                return Result::Invalid(challenge);
            };
            let Ok(username) = std::str::from_utf8(&decoded[..colon]) else {
                return Result::Invalid(challenge);
            };
            let hash = basic
                .users
                .iter()
                .find(|u| u.name == username)
                .map(|u| u.hash.0.clone());
            let known = hash.is_some();
            let mut hash = hash.unwrap_or_else(|| DUMMY.to_owned());
            let mut password = decoded[colon + 1..].to_vec();
            let too_long = hash.starts_with("$2b$") && password.len() > 72;
            if too_long {
                let cost = hash.get(4..6).unwrap_or("10");
                hash = format!("$2b${cost}${}", &DUMMY[7..]);
                password = b"dummy".to_vec();
            }
            let permit = match CAPACITY
                .get_or_init(|| Arc::new(Semaphore::new(4)))
                .clone()
                .try_acquire_owned()
            {
                Ok(p) => p,
                Err(_) => return Result::Capacity,
            };
            let task = tokio::task::spawn_blocking(move || {
                let _snapshot = snapshot;
                let _permit = permit;
                if hash.starts_with("$2b$") {
                    bcrypt::verify(&password, &hash).unwrap_or(false)
                } else {
                    PasswordHash::new(&hash).ok().is_some_and(|parsed| {
                        Argon2::default()
                            .verify_password(&password, &parsed)
                            .is_ok()
                    })
                }
            });
            match tokio::time::timeout(Duration::from_secs(2), task).await {
                Ok(Ok(true)) if known && !too_long => Result::Success,
                Ok(Ok(_)) => Result::Invalid(challenge),
                Ok(Err(_)) => Result::Capacity,
                Err(_) => Result::Timeout,
            }
        }
        _ => Result::Capacity,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::prepare::build,
        runtime::state::{HttpPreparer, RuntimeState},
    };

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_authorize_keeps_running_worker_permits()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let path = std::env::temp_dir().join(format!(
            "rill-auth-cancel-{}-{}.kdl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        let hash = "$2b$14$EGdrhbKUv8Oc9vGiXX0HQOxSg445d458Muh7DAHskb6QbtCvdxcie";
        std::fs::write(
            &path,
            format!(
                "server {{ http \"127.0.0.1:8080\"; }}\nsite \"example.test\" {{ chain {{ rule {{ basic-auth realm=\"private\" {{ user \"alice\" hash=\"{hash}\"; }}; respond 200; }}; }}; }}"
            ),
        )?;
        let state = RuntimeState::new(build(&path, &HttpPreparer)?)?;
        std::fs::remove_file(&path)?;
        let cursor = Cursor {
            rule: 0,
            branches: Vec::new(),
            next: 1,
            captures: [None; 9],
            error_status: None,
        };
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            http::HeaderValue::from_static("Basic YWxpY2U6YmFk"),
        );
        let mut tasks = Vec::new();
        for _ in 0..4 {
            let snapshot = state.snapshot();
            let cursor = cursor.clone();
            let headers = headers.clone();
            tasks.push(tokio::spawn(async move {
                authorize(snapshot, 0, &cursor, &headers).await
            }));
        }
        let capacity = CAPACITY.get_or_init(|| Arc::new(Semaphore::new(4))).clone();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        while capacity.available_permits() != 0 {
            if tokio::time::Instant::now() >= deadline {
                return Err("workers did not acquire all permits".into());
            }
            tokio::task::yield_now().await;
        }
        for task in &tasks {
            task.abort();
        }
        for task in tasks {
            assert!(task.await.is_err());
        }
        assert_eq!(capacity.available_permits(), 0);
        assert!(matches!(
            authorize(state.snapshot(), 0, &cursor, &headers).await,
            Result::Capacity
        ));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while capacity.available_permits() != 4 {
            if tokio::time::Instant::now() >= deadline {
                return Err("workers failed to release permits".into());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok(())
    }
}
