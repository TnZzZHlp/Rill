use super::support::{Fixture, Result};
use std::fs;
use std::io;
use std::net::UdpSocket;
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

struct Reload(Option<Child>);

impl Reload {
    fn spawn(f: &Fixture) -> Result<Self> {
        Ok(Self(Some(
            Command::new(env!("CARGO_BIN_EXE_rill"))
                .args(["reload", "--socket"])
                .arg(&f.socket)
                .args(["--timeout", "45s"])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?,
        )))
    }

    fn output(mut self) -> Result<Output> {
        Ok(self
            .0
            .take()
            .ok_or("missing reload child")?
            .wait_with_output()?)
    }
}

impl Drop for Reload {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn dns_answer(query: &[u8]) -> Result<Vec<u8>> {
    if query.len() < 17 || query[4..6] != [0, 1] {
        return Err("invalid DNS question".into());
    }
    let mut offset = 12;
    let mut labels = Vec::new();
    loop {
        let length = *query.get(offset).ok_or("truncated DNS name")? as usize;
        offset += 1;
        if length == 0 {
            break;
        }
        if length > 63 || offset + length > query.len() {
            return Err("invalid DNS label".into());
        }
        labels.push(std::str::from_utf8(&query[offset..offset + length])?);
        offset += length;
    }
    if labels.join(".") != "slow-reload.test" || offset + 4 > query.len() {
        return Err("unexpected DNS question".into());
    }
    let question_end = offset + 4;
    let qtype = &query[offset..offset + 2];
    if query[offset + 2..question_end] != [0, 1] {
        return Err("unexpected DNS class".into());
    }
    let address: &[u8] = match qtype {
        [0, 1] => &[127, 0, 0, 1],
        [0, 28] => &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
        _ => return Err("unexpected DNS type".into()),
    };
    let mut answer = vec![query[0], query[1], 0x81, 0x80, 0, 1, 0, 1, 0, 0, 0, 0];
    answer.extend_from_slice(&query[12..question_end]);
    answer.extend_from_slice(&[0xc0, 0x0c]);
    answer.extend_from_slice(qtype);
    answer.extend_from_slice(&[0, 1, 0, 0, 0, 60, 0, address.len() as u8]);
    answer.extend_from_slice(address);
    Ok(answer)
}

fn assert_stable(f: &Fixture) -> Result {
    let mut stream = f.connect()?;
    let (status, headers, body) = Fixture::request(&mut stream, "/file")?;
    assert_eq!(status, 200);
    assert!(
        headers
            .to_ascii_lowercase()
            .contains("cache-control: max-age=10")
    );
    assert_eq!(body, b"stable");
    Ok(())
}

fn assert_busy(output: Output) {
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("BUSY"));
}

#[test]
#[ignore = "requires isolated resolver at 127.0.0.1:53"]
fn timed_out_prepare_keeps_admission_and_never_publishes_late() -> Result {
    let mut f = Fixture::new()?;
    let root = f.dir.join("root");
    fs::create_dir(&root)?;
    fs::write(root.join("file"), "stable")?;
    let valid = f.config(&root, "max-age=10");
    fs::write(&f.entry, &valid)?;
    f.start()?;
    assert_stable(&f)?;

    let socket = UdpSocket::bind("127.0.0.1:53")?;
    socket.set_read_timeout(Some(Duration::from_secs(5)))?;
    let (ready_tx, ready_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let dns = thread::spawn(move || -> Result {
        let mut buf = [0; 4096];
        let (length, peer) = socket.recv_from(&mut buf)?;
        let mut pending = vec![(dns_answer(&buf[..length])?, peer)];
        ready_tx.send(())?;
        socket.set_read_timeout(Some(Duration::from_millis(100)))?;
        let deadline = Instant::now() + Duration::from_secs(40);
        loop {
            match release_rx.try_recv() {
                Ok(()) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err("DNS release disconnected".into());
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
            if Instant::now() >= deadline {
                return Err("DNS release timed out".into());
            }
            match socket.recv_from(&mut buf) {
                Ok((length, peer)) => pending.push((dns_answer(&buf[..length])?, peer)),
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) => {}
                Err(e) => return Err(e.into()),
            }
        }
        for (answer, peer) in pending {
            socket.send_to(&answer, peer)?;
        }
        Ok(())
    });

    fs::write(
        &f.entry,
        format!(
            "server {{ http \"{}\"; control-socket \"{}\"; }}\nsite \"example.test\" {{ proxy \"http://slow-reload.test:3000\"; }}\n",
            f.address,
            f.socket.display()
        ),
    )?;
    let start = Instant::now();
    let first = Reload::spawn(&f)?;
    ready_rx.recv_timeout(Duration::from_secs(10))?;
    let mut others = Vec::new();
    for _ in 0..7 {
        others.push(Reload::spawn(&f)?);
    }
    let output = first.output()?;
    assert!(start.elapsed() > Duration::from_secs(5));
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("PREPARE_TIMEOUT"));
    assert_stable(&f)?;
    assert_busy(Reload::spawn(&f)?.output()?);
    fs::write(&f.entry, &valid)?;
    release_tx.send(())?;
    dns.join().map_err(|_| "DNS thread panicked")??;

    let mut successes = 0;
    let mut busy = 0;
    for child in others {
        let output = child.output()?;
        if output.status.success() {
            successes += 1;
            assert!(
                String::from_utf8_lossy(&output.stdout)
                    .split_whitespace()
                    .any(|field| field == "generation=2")
            );
        } else {
            assert_busy(output);
            busy += 1;
        }
    }
    assert_eq!((successes, busy), (1, 6));
    assert_stable(&f)?;
    assert_eq!(f.reload()?.generation, Some(3));
    Ok(())
}
