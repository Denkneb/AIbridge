mod common;
use bridge_mcp::{McpError, McpServer, http::HttpServer};
use common::{Fixture, initialize};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};
struct Running {
    fixture: Fixture,
    port: u16,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<bridge_mcp::Result<()>>>,
}
impl Running {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let fixture = Fixture::new(&format!(
            "mcp_url=\"http://127.0.0.1:{port}/mcp\"\nmcp_token_file=\"mcp.token\""
        ));
        write_token(&fixture, "fixture-token");
        let server = HttpServer::bind(fixture.project.clone(), fixture.layout.clone()).unwrap();
        assert!(server.address().unwrap().ip().is_loopback());
        let stop = Arc::new(AtomicBool::new(false));
        let child_stop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || server.run_until(&child_stop));
        Self {
            fixture,
            port,
            stop,
            handle: Some(handle),
        }
    }
    fn raw(&self, request: &[u8]) -> (u16, String, Vec<u8>) {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream.write_all(request).unwrap();
        stream.shutdown(Shutdown::Write).unwrap();
        // HTTP framing ends at Content-Length, not TCP EOF: overload rejection
        // can reset a connection with unread request bytes after a full response.
        let mut reader = BufReader::new(stream);
        let mut headers = String::new();
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).unwrap() > 0);
            if line == "\r\n" {
                break;
            }
            headers.push_str(&line);
        }
        let status = headers.split_whitespace().nth(1).unwrap().parse().unwrap();
        let length: usize = headers
            .lines()
            .find_map(|line| line.strip_prefix("Content-Length: "))
            .unwrap()
            .parse()
            .unwrap();
        let mut response = vec![0; length];
        reader.read_exact(&mut response).unwrap();
        (status, headers, response)
    }
    fn request(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> (u16, String, Vec<u8>) {
        let mut request = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n",
            self.port
        );
        for (key, value) in headers {
            request.push_str(&format!("{key}: {value}\r\n"));
        }
        request.push_str("\r\n");
        request.push_str(body);
        self.raw(request.as_bytes())
    }
    fn post(&self, body: &Value, token: &str) -> (u16, String, Vec<u8>) {
        let body = body.to_string();
        self.request(
            "POST",
            "/mcp",
            &[
                ("Authorization", &format!("Bearer {token}")),
                ("Accept", "application/json, text/event-stream"),
                ("Content-Type", "application/json"),
                ("Content-Length", &body.len().to_string()),
                ("MCP-Protocol-Version", "2025-06-18"),
            ],
            &body,
        )
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.handle.take().unwrap().join().unwrap().unwrap();
    }
}
fn write_token(f: &Fixture, value: &str) {
    fs::write(f.root.join("mcp.token"), value).unwrap();
    fs::set_permissions(f.root.join("mcp.token"), fs::Permissions::from_mode(0o600)).unwrap();
}
#[test]
fn authenticated_sessionless_http_initialize_project_info_notifications_and_get_contract() {
    let server = Running::new();
    let (status, headers, body) = server.post(&initialize(), "fixture-token");
    assert_eq!(status, 200);
    assert!(headers.contains("Content-Type: application/json"));
    assert!(!headers.to_ascii_lowercase().contains("mcp-session-id"));
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap()["result"]["protocolVersion"],
        "2025-06-18"
    );
    let call =
        json!({"jsonrpc":"2.0","id":"call","method":"tools/call","params":{"name":"project_info"}});
    let (status, _, body) = server.post(&call, "fixture-token");
    assert_eq!(status, 200);
    let message: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(message["result"]["structuredContent"]["project_id"], "proj");
    let (status, _, body) = server.post(
        &json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        "fixture-token",
    );
    assert_eq!(status, 202);
    assert!(body.is_empty());
    assert_eq!(
        server
            .request(
                "GET",
                "/mcp",
                &[("Authorization", "Bearer fixture-token")],
                ""
            )
            .0,
        405
    );
    assert_eq!(server.request("GET", "/mcp", &[], "").0, 401);
    assert_eq!(
        server
            .request(
                "POST",
                "/other",
                &[("Authorization", "Bearer fixture-token")],
                ""
            )
            .0,
        404
    );
}
#[test]
fn auth_rereads_private_token_for_rotation_revocation_and_missing_or_wrong_bearer() {
    let server = Running::new();
    let request = json!({"jsonrpc":"2.0","id":2,"method":"ping"});
    for token in ["", "wrong-token", "fixture-tokeN"] {
        let (status, headers, body) = server.post(&request, token);
        assert_eq!(status, 401);
        assert!(headers.contains("WWW-Authenticate: Bearer"));
        assert!(body.is_empty());
    }
    write_token(&server.fixture, "rotated-token");
    assert_eq!(server.post(&request, "fixture-token").0, 401);
    assert_eq!(server.post(&request, "rotated-token").0, 200);
    fs::set_permissions(
        server.fixture.root.join("mcp.token"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    assert_eq!(server.post(&request, "rotated-token").0, 401);
    fs::remove_file(server.fixture.root.join("mcp.token")).unwrap();
    assert_eq!(server.post(&request, "rotated-token").0, 401);
}
#[test]
fn origin_host_content_type_accept_and_protocol_headers_fail_closed() {
    let server = Running::new();
    let body = initialize().to_string();
    let length = body.len().to_string();
    for (extra, expected) in [
        (vec![("Origin", "https://evil.example")], 403),
        (vec![("Origin", "null")], 403),
        (vec![("MCP-Protocol-Version", "unknown-version")], 400),
        (vec![("Expect", "100-continue")], 417),
    ] {
        let mut headers = vec![
            ("Authorization", "Bearer fixture-token"),
            ("Accept", "application/json,text/event-stream"),
            ("Content-Type", "application/json"),
            ("Content-Length", &length),
        ];
        headers.extend(extra);
        assert_eq!(server.request("POST", "/mcp", &headers, &body).0, expected);
    }
    for (accept, content_type, expected) in [
        ("application/json", "application/json", 406),
        (
            "application/json;q=0,text/event-stream",
            "application/json",
            406,
        ),
        ("application/json,text/event-stream", "text/plain", 415),
    ] {
        assert_eq!(
            server
                .request(
                    "POST",
                    "/mcp",
                    &[
                        ("Authorization", "Bearer fixture-token"),
                        ("Accept", accept),
                        ("Content-Type", content_type),
                        ("Content-Length", &body.len().to_string())
                    ],
                    &body
                )
                .0,
            expected
        );
    }
    let host_request = format!(
        "POST /mcp HTTP/1.1\r\nHost: evil.example:{}\r\nAuthorization: Bearer fixture-token\r\n\r\n",
        server.port
    );
    assert_eq!(server.raw(host_request.as_bytes()).0, 403);
    let allowed_origin = format!("http://localhost:{}", server.port);
    assert_eq!(
        server
            .request(
                "POST",
                "/mcp",
                &[
                    ("Authorization", "Bearer fixture-token"),
                    ("Origin", &allowed_origin),
                    ("Accept", "application/json,text/event-stream"),
                    ("Content-Type", "application/json"),
                    ("Content-Length", &body.len().to_string())
                ],
                &body
            )
            .0,
        200
    );
}
#[test]
fn chunked_json_works_and_ambiguous_framing_malformed_trailers_and_oversize_are_refused() {
    let server = Running::new();
    let body = initialize().to_string();
    let chunked = format!("{:x}\r\n{}\r\n0\r\n\r\n", body.len(), body);
    let common = [
        ("Authorization", "Bearer fixture-token"),
        ("Accept", "application/json,text/event-stream"),
        ("Content-Type", "application/json"),
    ];
    let mut headers = common.to_vec();
    headers.push(("Transfer-Encoding", "chunked"));
    assert_eq!(server.request("POST", "/mcp", &headers, &chunked).0, 200);
    headers.push(("Content-Length", "1"));
    assert_eq!(server.request("POST", "/mcp", &headers, &chunked).0, 400);
    headers.pop();
    assert_eq!(server.request("POST", "/mcp", &headers, "z\r\n").0, 400);
    assert_eq!(
        server.request("POST", "/mcp", &headers, "100001\r\n").0,
        413
    );
    assert_eq!(
        server
            .request(
                "POST",
                "/mcp",
                &headers,
                "0\r\nAuthorization: injected\r\n\r\n"
            )
            .0,
        400
    );
    let mut headers = common.to_vec();
    headers.push(("Content-Length", "1048577"));
    assert_eq!(server.request("POST", "/mcp", &headers, "").0, 413);
    headers.pop();
    headers.push(("Content-Length", "1"));
    headers.push(("Content-Length", "2"));
    assert_eq!(server.request("POST", "/mcp", &headers, "").0, 400);
    headers.pop();
    assert_eq!(server.request("POST", "/mcp", &headers, "").0, 400);
    assert_eq!(server.request("POST", "/mcp", &common, "").0, 411);
    let huge = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer fixture-token\r\nX-Large: {}\r\n\r\n",
        server.port,
        "x".repeat(8192)
    );
    assert_eq!(server.raw(huge.as_bytes()).0, 431);
}
#[test]
fn malformed_json_is_redacted_and_delegated_tools_are_unavailable_without_task_creation() {
    let server = Running::new();
    let body = "private-token invalid-json";
    let (status, _, response) = server.request(
        "POST",
        "/mcp",
        &[
            ("Authorization", "Bearer fixture-token"),
            ("Accept", "application/json,text/event-stream"),
            ("Content-Type", "application/json"),
            ("Content-Length", &body.len().to_string()),
        ],
        body,
    );
    assert_eq!(status, 400);
    assert!(!String::from_utf8_lossy(&response).contains("private-token"));
    let (status,_,body)=server.post(&json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"submit_task","arguments":{"task":"private-task"}}}),"fixture-token");
    assert_eq!(status, 200);
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap()["error"]["code"],
        -32602
    );
    assert!(
        server
            .fixture
            .layout
            .open()
            .unwrap()
            .active_set(server.fixture.project.id())
            .unwrap()
            .tasks
            .is_empty()
    );
}
#[test]
fn http_and_stdio_share_mcp_lock_and_port_or_token_preflight_does_not_create_state() {
    let server = Running::new();
    assert!(matches!(
        McpServer::open(
            server.fixture.project.clone(),
            server.fixture.layout.clone()
        ),
        Err(McpError::Busy)
    ));
    let reserved = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reserved.local_addr().unwrap().port();
    let f = Fixture::new(&format!(
        "mcp_url=\"http://127.0.0.1:{port}/mcp\"\nmcp_token_file=\"mcp.token\""
    ));
    assert!(matches!(
        HttpServer::bind(f.project.clone(), f.layout.clone()),
        Err(McpError::Credentials)
    ));
    assert!(!f.layout.state_root().exists());
    write_token(&f, "fixture-token");
    assert!(matches!(
        HttpServer::bind(f.project.clone(), f.layout.clone()),
        Err(McpError::Endpoint)
    ));
    assert!(!f.layout.state_root().exists());
}
#[test]
fn concurrent_authenticated_clients_receive_their_own_responses() {
    let server = Running::new();
    std::thread::scope(|scope| {
        for id in 0..8 {
            let server = &server;
            scope.spawn(move||{let(status,_,body)=server.post(&json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"project_info"}}),"fixture-token");assert_eq!(status,200);assert_eq!(serde_json::from_slice::<Value>(&body).unwrap()["id"],id);});
        }
    });
}

#[test]
fn connection_capacity_is_bounded_and_recovers_after_incomplete_clients_disconnect() {
    let server = Running::new();
    let mut held = Vec::new();
    for _ in 0..16 {
        let mut stream = TcpStream::connect(("127.0.0.1", server.port)).unwrap();
        stream.write_all(b"POST /mcp HTTP/1.1\r\n").unwrap();
        held.push(stream);
    }
    let ping = json!({"jsonrpc":"2.0","id":1,"method":"ping"});
    assert_eq!(server.post(&ping, "fixture-token").0, 503);
    drop(held);
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        if server.post(&ping, "fixture-token").0 == 200 {
            break;
        }
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
}
#[test]
fn unfinished_request_hits_total_deadline_instead_of_holding_connection_indefinitely() {
    let server = Running::new();
    let mut stream = TcpStream::connect(("127.0.0.1", server.port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    stream.write_all(b"POST /mcp HTTP/1.1\r\n").unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(
        response.starts_with("HTTP/1.1 408 Request Timeout\r\n"),
        "{response:?}"
    );
}
