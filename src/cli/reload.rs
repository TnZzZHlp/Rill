use std::path::Path;
use std::time::Duration;

pub fn parse_timeout(value: &str) -> Result<Duration, String> {
    let split = value
        .find(|c: char| !c.is_ascii_digit())
        .ok_or("timeout requires ms, s, m or h")?;
    let number: u64 = value[..split].parse().map_err(|_| "invalid timeout")?;
    let factor = match &value[split..] {
        "ms" => 1,
        "s" => 1000,
        "m" => 60_000,
        "h" => 3_600_000,
        _ => return Err("timeout requires ms, s, m or h".into()),
    };
    let millis = number
        .checked_mul(factor)
        .filter(|n| *n > 0)
        .ok_or("invalid timeout")?;
    let duration = Duration::from_millis(millis);
    std::time::Instant::now()
        .checked_add(duration)
        .ok_or("timeout too large")?;
    Ok(duration)
}

pub fn run(path: &Path, duration: Duration) -> u8 {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => {
            eprintln!("reload communication failed; no automatic retry, result may be unknown");
            return 3;
        }
    };
    match runtime.block_on(crate::control::client::reload(path, duration)) {
        Ok(response) if response.status == "ok" => {
            if let Some(generation) = response.generation {
                println!("id={} generation={generation}", response.id);
                0
            } else {
                3
            }
        }
        Ok(response) => {
            eprintln!(
                "reload failed: {} id={}",
                response.code.as_deref().unwrap_or("INTERNAL"),
                response.id
            );
            for error in response.errors {
                eprintln!(
                    "{}:{}:{}: {}: {}",
                    error.file, error.line, error.column, error.item, error.reason
                );
            }
            if response.truncated {
                eprintln!("diagnostics truncated");
            }
            1
        }
        Err(_) => {
            eprintln!("reload communication failed; no automatic retry, result may be unknown");
            3
        }
    }
}
