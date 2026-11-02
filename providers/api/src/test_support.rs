//! Shared loopback HTTP fixtures; never compiled into the product.
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

pub(crate) const TEST_ACCEPT_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) struct TestRequest {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}

impl TestRequest {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

pub(crate) struct TestResponse {
    pub(crate) status: u16,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
    pub(crate) include_content_length: bool,
}

impl TestResponse {
    pub(crate) fn json(body: &[u8]) -> Self {
        Self {
            headers: vec![("Content-Type".into(), "application/json".into())],
            ..Self::ok(body)
        }
    }

    pub(crate) fn ok(body: &[u8]) -> Self {
        Self::status(200, body)
    }

    pub(crate) fn status(status: u16, body: &[u8]) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.to_vec(),
            include_content_length: true,
        }
    }

    pub(crate) fn redirect(location: &str) -> Self {
        Self {
            status: 302,
            headers: vec![("Location".to_string(), location.to_string())],
            body: Vec::new(),
            include_content_length: true,
        }
    }

    pub(crate) fn without_content_length(mut self) -> Self {
        self.include_content_length = false;
        self
    }
}

pub(crate) struct TestServer {
    pub(crate) addr: SocketAddr,
    requests: Arc<Mutex<Vec<TestRequest>>>,
    handle: thread::JoinHandle<()>,
}

impl TestServer {
    pub(crate) fn start<F>(expected_requests: usize, handler: F) -> Self
    where
        F: Fn(&TestRequest, usize) -> TestResponse + Send + 'static,
    {
        Self::start_with(expected_requests, move |stream, request, index| {
            let response = handler(request, index);
            write_test_response(stream, &response);
        })
    }

    pub(crate) fn start_with<F>(expected_requests: usize, handler: F) -> Self
    where
        F: Fn(&mut TcpStream, &TestRequest, usize) + Send + 'static,
    {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind test server");
        let addr = listener.local_addr().expect("test server address");
        listener
            .set_nonblocking(true)
            .expect("nonblocking test listener");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let handle = thread::spawn(move || {
            let deadline = Instant::now() + TEST_ACCEPT_TIMEOUT;
            let mut served = 0;
            while served < expected_requests && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).expect("blocking test stream");
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                        let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                        let request = read_test_request(&mut stream);
                        handler(&mut stream, &request, served);
                        captured.lock().expect("requests mutex").push(request);
                        served += 1;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            addr,
            requests,
            handle,
        }
    }

    pub(crate) fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    pub(crate) fn join(self) -> Vec<TestRequest> {
        self.handle.join().expect("join test server");
        let requests = match Arc::try_unwrap(self.requests) {
            Ok(requests) => requests,
            Err(_) => panic!("test server requests still shared"),
        };
        requests.into_inner().expect("requests mutex")
    }
}

pub(crate) fn read_test_request(stream: &mut TcpStream) -> TestRequest {
    let mut data = Vec::new();
    let mut chunk = [0u8; 1024];
    let header_end = loop {
        let read = stream.read(&mut chunk).expect("read test request");
        if read == 0 {
            panic!("connection closed before request headers");
        }
        data.extend_from_slice(&chunk[..read]);
        if let Some(position) = data.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
        if data.len() > 64 * 1024 {
            panic!("request headers too large");
        }
    };

    let head = std::str::from_utf8(&data[..header_end - 4]).expect("ASCII request headers");
    let mut lines = head.split("\r\n");
    let request_line = lines.next().expect("request line");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().expect("method").to_string();
    let path = parts.next().expect("path").to_string();
    let mut headers = Vec::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    let content_length = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = data[header_end..].to_vec();
    while body.len() < content_length {
        let read = stream.read(&mut chunk).expect("read test body");
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(content_length);
    TestRequest {
        method,
        path,
        headers,
        body,
    }
}

pub(crate) fn write_test_response(stream: &mut TcpStream, response: &TestResponse) {
    let mut head = format!("HTTP/1.1 {} Test\r\nConnection: close\r\n", response.status);
    if response.include_content_length {
        head.push_str(&format!("Content-Length: {}\r\n", response.body.len()));
    }
    for (name, value) in &response.headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&response.body);
    let _ = stream.flush();
}
