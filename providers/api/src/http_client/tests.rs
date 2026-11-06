use super::*;
use std::io::{Read, Write};
use std::net::TcpListener;

/// Serve one response per accepted connection.
pub(crate) fn serve(responses: Vec<String>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    std::thread::spawn(move || {
        for response in responses {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 65536];
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(response.as_bytes());
        }
    });
    url
}

fn ok(body: &str) -> String {
    format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
}

#[test]
fn sends_downloads_and_bounds_bodies() {
    let url = serve(vec![ok("hello"), ok("hello"), ok("abcdef")]);
    let r = send(&HttpRequest::get(&url, Duration::from_secs(5), 100), None).unwrap();
    assert_eq!((r.status, r.body.as_slice()), (200, &b"hello"[..]));
    let too_small = send(&HttpRequest::get(&url, Duration::from_secs(5), 3), None).unwrap_err();
    assert_eq!(too_small, HttpError::ResponseTooLarge);
    let dir = std::env::temp_dir().join(format!("shishi-api-dl-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let dest = dir.join("f");
    let mut seen = 0;
    assert_eq!(download(&HttpRequest::get(&url, Duration::from_secs(5), 100), &dest, &mut |n| seen = n).unwrap(), 200);
    assert_eq!((std::fs::read(&dest).unwrap(), seen), (b"abcdef".to_vec(), 6));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn only_https_or_test_loopback_and_cancel_is_honored() {
    let err = send(&HttpRequest::get("http://example.com/", Duration::from_secs(1), 1), None).unwrap_err();
    assert_eq!(err, HttpError::InvalidUrl);
    let cancel = AtomicBool::new(true);
    let url = serve(vec![ok("x")]);
    let mut request = HttpRequest::get(&url, Duration::from_secs(1), 10);
    request.cancel = Some(&cancel);
    assert_eq!(send(&request, None).unwrap_err(), HttpError::Cancelled);
}

#[test]
fn winhttp_proxy_lists_and_bypass_patterns() {
    let url = reqwest::Url::parse("https://api.example.com/v1").unwrap();
    assert_eq!(proxy_for(&url, "http=a:1;https=b:2", ""), Some("http://b:2".into()));
    assert_eq!(proxy_for(&url, "c:3", "*.example.com"), None);
    assert_eq!(proxy_for(&url, "c:3", "<local>"), Some("http://c:3".into()));
    assert!(wildcard("a*c*", "abcde") && !wildcard("a*x", "abc"));
}
