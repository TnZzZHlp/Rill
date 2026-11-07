use crate::support::{Proxy, TestResult, connect, response, vacant};
use std::io::Write;

#[test]
fn header_prefix_and_boolean_matchers_route_real_requests() -> TestResult {
    let address = vacant()?;
    let config = format!(
        r#"server {{ http "{address}"; }}
site "app.test" {{ chain {{
 rule {{ match {{ all {{ any {{ header-prefix "content-type" "application/grpc"; header-equal "x-choice" "yes"; }}; not {{ method "DELETE"; }}; }}; }}; respond 201 text="selected"; }};
 rule {{ respond 404; }};
}}; }}"#
    );
    let mut proxy = Proxy::start(address, &config)?;
    for (method, headers, expected) in [
        ("POST", "Content-Type: application/grpc+proto\r\n", 201),
        ("GET", "X-Choice: yes\r\n", 201),
        ("POST", "Content-Type: application/json\r\n", 404),
        ("DELETE", "Content-Type: application/grpc\r\n", 404),
    ] {
        let mut stream = connect(address)?;
        write!(
            stream,
            "{method} / HTTP/1.1\r\nHost: app.test\r\n{headers}Connection: close\r\n\r\n"
        )?;
        assert_eq!(response(&mut stream)?.0, expected);
    }
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);
    Ok(())
}
