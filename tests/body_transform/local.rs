use super::http::read_body;
use crate::support::*;
use std::io::{Read, Write};

#[test]
fn branch_reentry_empty_body_and_bodyless_statuses_use_reached_filter_instances() -> TestResult {
    let address = vacant()?;
    let config = format!(
        r#"server {{ http "{address}"; }}
site "app.test" {{ chain {{
 rule {{ body-replace mime="text/html" {{ literal "old" "base"; }}; continue; }}
 rule {{ match {{ path-glob "/start"; }}; body-replace mime="text/html" {{ literal "base" "again"; }}; rewrite "/final"; reenter; }}
 rule {{ match {{ path-glob "/branch"; }}; branch {{
  when {{ match {{ header-equal "x-pick" "yes"; }}; body-replace mime="text/html" {{ literal "base" "picked"; }}; respond 200 text="old" content-type="text/html"; }}
  otherwise {{ body-replace mime="text/html" {{ literal "base" "other"; }}; respond 200 text="old" content-type="text/html"; }}
 }} }}
 rule {{ match {{ path-glob "/empty"; }}; body-replace mime="text/html" {{ literal "" "X"; }}; respond 200 content-type="text/html"; }}
 rule {{ match {{ path-glob "/204"; }}; respond 204 content-type="text/html"; }}
 rule {{ match {{ path-glob "/304"; }}; respond 304 content-type="text/html"; }}
 rule {{ respond 200 text="old" content-type="text/html"; }}
}} }}"#
    );
    let _proxy = Proxy::start(address, &config)?;
    for (path, pick, expected) in [
        ("/start", "no", &b"again"[..]),
        ("/branch", "yes", &b"picked"[..]),
        ("/branch", "no", &b"other"[..]),
        ("/empty", "no", &b"X"[..]),
    ] {
        let mut stream = connect(address)?;
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: app.test\r\nConnection: close\r\nx-pick: {pick}\r\n\r\n"
        )?;
        let (headers, body) = read_body(&mut stream)?;
        assert!(headers.starts_with("HTTP/1.1 200 "), "{headers}");
        assert_eq!(body, expected);
    }
    for (method, path) in [("GET", "/204"), ("GET", "/304"), ("HEAD", "/empty")] {
        let mut stream = connect(address)?;
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: app.test\r\nConnection: close\r\n\r\n"
        )?;
        let headers = head(&mut stream)?.to_ascii_lowercase();
        assert!(
            !headers.contains("content-length:") && !headers.contains("transfer-encoding:"),
            "{headers}"
        );
        let mut body = Vec::new();
        stream.read_to_end(&mut body)?;
        assert!(body.is_empty());
    }
    Ok(())
}
