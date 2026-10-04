//! Sessionless Streamable HTTP using JSON responses, without SSE streams.
//! Loopback-only, authenticated, bounded HTTP/1.1 messages and connections.
use crate::{
    McpError, McpServer, Result,
    protocol::{Protocol, SUPPORTED_VERSIONS},
    stdio::MAX_MESSAGE_BYTES,
};
use bridge_config::ProjectEntry;
use bridge_storage::RustStateLayout;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::{BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
const MAX_HEADER_BYTES: usize = 32 * 1024;
const MAX_LINE_BYTES: usize = 8 * 1024;
const MAX_CONNECTIONS: usize = 16;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

pub struct HttpServer {
    listener: TcpListener,
    server: Arc<McpServer>,
}
impl HttpServer {
    /// Preflights credentials and atomically binds the configured loopback port.
    /// # Errors
    /// Missing HTTP config/token, occupied port or foreign/busy state fail closed.
    pub fn bind(project: ProjectEntry, layout: RustStateLayout) -> Result<Self> {
        Self::bind_server(project, layout, None)
    }
    /// Bind the delegated surface with a trusted worker launcher.
    pub fn bind_with_workers(
        project: ProjectEntry,
        layout: RustStateLayout,
        spawner: crate::WorkerSpawner,
        registry: Vec<ProjectEntry>,
    ) -> Result<Self> {
        Self::bind_server(project, layout, Some((spawner, registry)))
    }
    fn bind_server(
        project: ProjectEntry,
        layout: RustStateLayout,
        workers: Option<(crate::WorkerSpawner, Vec<ProjectEntry>)>,
    ) -> Result<Self> {
        let port = project.mcp_endpoint().ok_or(McpError::Endpoint)?.port();
        token(&project).ok_or(McpError::Credentials)?;
        let listener = TcpListener::bind(("127.0.0.1", port)).map_err(|_| McpError::Endpoint)?;
        let mut server = McpServer::open(project, layout)?;
        if let Some((spawner, registry)) = workers {
            server = server.with_workers(spawner, registry)?;
        }
        let server = Arc::new(server);
        listener.set_nonblocking(true).map_err(|_| McpError::Io)?;
        Ok(Self { listener, server })
    }
    pub fn address(&self) -> Result<SocketAddr> {
        self.listener.local_addr().map_err(|_| McpError::Io)
    }
    /// Foreground loop; process signals are handled by the invoking runtime.
    /// # Errors
    /// Reports listener/thread failures with fixed safe labels.
    pub fn run(self) -> Result<()> {
        self.run_until(&AtomicBool::new(false))
    }
    /// Trusted embedding/test stop flag. Outstanding requests drain before return.
    /// # Errors
    /// Reports listener/thread failures with fixed safe labels.
    pub fn run_until(self, stop: &AtomicBool) -> Result<()> {
        let port = self.address()?.port();
        let mut handles: Vec<std::thread::JoinHandle<()>> = Vec::new();
        let result = (|| {
            while !stop.load(Ordering::Acquire) {
                handles.retain(|handle| !handle.is_finished());
                match self.listener.accept() {
                    Ok((mut stream, _)) => {
                        if handles.len() >= MAX_CONNECTIONS {
                            let _ = send(&mut stream, 503, b"", false);
                            continue;
                        }
                        let server = Arc::clone(&self.server);
                        handles.push(
                            std::thread::Builder::new()
                                .name("bridge-mcp-http".into())
                                .spawn(move || {
                                    let _ = connection(stream, &server, port);
                                })
                                .map_err(|_| McpError::Io)?,
                        );
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10))
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => (),
                    Err(_) => return Err(McpError::Io),
                }
            }
            Ok(())
        })();
        for handle in handles {
            let _ = handle.join();
        }
        result
    }
}
fn token(project: &ProjectEntry) -> Option<bridge_config::Secret> {
    let secret = project.read_mcp_token().ok()??;
    if secret.expose_secret().len() > MAX_LINE_BYTES / 2
        || !secret.expose_secret().bytes().all(|b| b.is_ascii_graphic())
    {
        return None;
    }
    Some(secret)
}
fn authorized(project: &ProjectEntry, header: Option<&String>) -> bool {
    let Some(expected) = token(project) else {
        return false;
    };
    let Some((scheme, provided)) = header.and_then(|h| h.split_once(' ')) else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("Bearer") || provided.is_empty() {
        return false;
    }
    // Hash to equal-length digests and accumulate every byte without early exit.
    // black_box prevents replacing the loop with a prefix-short-circuit compare.
    let expected = Sha256::digest(expected.expose_secret().as_bytes());
    let provided = Sha256::digest(provided.as_bytes());
    let difference = expected
        .iter()
        .zip(provided.iter())
        .fold(0u8, |acc, (a, b)| acc | std::hint::black_box(a ^ b));
    std::hint::black_box(difference) == 0
}
struct Input {
    reader: BufReader<TcpStream>,
    deadline: Instant,
}
impl Input {
    fn new(stream: TcpStream) -> Self {
        Self {
            reader: BufReader::new(stream),
            deadline: Instant::now() + REQUEST_TIMEOUT,
        }
    }
    fn read(&mut self, buffer: &mut [u8]) -> std::result::Result<usize, u16> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(408);
        }
        if self.reader.buffer().is_empty() {
            self.reader
                .get_ref()
                .set_read_timeout(Some(remaining))
                .map_err(|_| 500u16)?;
        }
        match self.reader.read(buffer) {
            Ok(n) => Ok(n),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                Err(408)
            }
            Err(_) => Err(400),
        }
    }
    fn line(&mut self) -> std::result::Result<Vec<u8>, u16> {
        let mut line = Vec::new();
        loop {
            let mut byte = [0u8; 1];
            if self.read(&mut byte)? == 0 {
                return Err(400);
            }
            line.push(byte[0]);
            if line.len() > MAX_LINE_BYTES {
                return Err(431);
            }
            if byte[0] == b'\n' {
                break;
            }
        }
        if !line.ends_with(b"\r\n") {
            return Err(400);
        }
        line.truncate(line.len() - 2);
        Ok(line)
    }
    fn bytes(&mut self, size: usize) -> std::result::Result<Vec<u8>, u16> {
        let mut body = vec![0; size];
        let mut offset = 0;
        while offset < size {
            let count = self.read(&mut body[offset..])?;
            if count == 0 {
                return Err(400);
            }
            offset += count;
        }
        Ok(body)
    }
}
struct Request {
    method: String,
    headers: BTreeMap<String, String>,
    length: Option<usize>,
    chunked: bool,
    path: String,
}
fn headers(input: &mut Input) -> std::result::Result<Request, u16> {
    let line = input.line()?;
    let mut total = line.len() + 2;
    let line = std::str::from_utf8(&line).map_err(|_| 400u16)?;
    let parts: Vec<_> = line.split(' ').collect();
    if parts.len() != 3
        || parts[2] != "HTTP/1.1"
        || parts[0].is_empty()
        || !parts[0].bytes().all(header_token)
    {
        return Err(400);
    }
    let method = parts[0].to_owned();
    let path = parts[1].to_owned();
    let mut headers = BTreeMap::new();
    loop {
        let line = input.line()?;
        total += line.len() + 2;
        if total > MAX_HEADER_BYTES {
            return Err(431);
        }
        if line.is_empty() {
            break;
        }
        let line = std::str::from_utf8(&line).map_err(|_| 400u16)?;
        let (name, value) = line.split_once(':').ok_or(400u16)?;
        if name.is_empty()
            || !name.bytes().all(header_token)
            || value.bytes().any(|b| b < 0x20 && b != b'\t' || b == 0x7f)
        {
            return Err(400);
        }
        let name = name.to_ascii_lowercase();
        // Reject ambiguous duplicate headers rather than merge auth/framing data.
        if headers
            .insert(name, value.trim_matches([' ', '\t']).to_owned())
            .is_some()
        {
            return Err(400);
        }
    }
    let length = if let Some(length) = headers.get("content-length") {
        if length.is_empty() || !length.bytes().all(|b| b.is_ascii_digit()) {
            return Err(400);
        }
        Some(length.parse::<usize>().map_err(|_| 413u16)?)
    } else {
        None
    };
    if length.is_some_and(|n| n > MAX_MESSAGE_BYTES) {
        return Err(413);
    }
    let chunked = if let Some(encoding) = headers.get("transfer-encoding") {
        if length.is_some() || !encoding.eq_ignore_ascii_case("chunked") {
            return Err(400);
        }
        true
    } else {
        false
    };
    Ok(Request {
        method,
        headers,
        length,
        chunked,
        path,
    })
}
fn header_token(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}
fn body(input: &mut Input, request: &Request) -> std::result::Result<Vec<u8>, u16> {
    if !request.chunked {
        return input.bytes(request.length.ok_or(411u16)?);
    }
    let mut result = Vec::new();
    let mut overhead = 0;
    loop {
        let line = input.line()?;
        overhead += line.len() + 2;
        if overhead > MAX_HEADER_BYTES {
            return Err(431);
        }
        let size = std::str::from_utf8(&line)
            .map_err(|_| 400u16)?
            .split(';')
            .next()
            .ok_or(400u16)?;
        if size.is_empty() || !size.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(400);
        }
        let size = usize::from_str_radix(size, 16).map_err(|_| 413u16)?;
        if size > MAX_MESSAGE_BYTES - result.len() {
            return Err(413);
        }
        if size == 0 {
            // Trailer fields may alter framing/auth semantics; this profile refuses them.
            if !input.line()?.is_empty() {
                return Err(400);
            }
            return Ok(result);
        }
        result.extend(input.bytes(size)?);
        if input.bytes(2)? != b"\r\n" {
            return Err(400);
        }
    }
}
fn acceptable(headers: &BTreeMap<String, String>) -> bool {
    let Some(accept) = headers.get("accept") else {
        return false;
    };
    ["application/json", "text/event-stream"]
        .iter()
        .all(|wanted| {
            accept.split(',').any(|part| {
                let mut parts = part.split(';');
                parts
                    .next()
                    .is_some_and(|media| media.trim().eq_ignore_ascii_case(wanted))
                    && !parts.any(|p| {
                        p.trim()
                            .strip_prefix("q=")
                            .is_some_and(|q| q.parse::<f32>().map_or(true, |q| q <= 0.0))
                    })
            })
        })
}
fn connection(stream: TcpStream, server: &McpServer, port: u16) -> Result<()> {
    stream.set_nonblocking(false).map_err(|_| McpError::Io)?;
    let mut input = Input::new(stream);
    let mut handle = || -> std::result::Result<(u16, Vec<u8>, bool), u16> {
        let request = headers(&mut input)?;
        if !authorized(&server.project, request.headers.get("authorization")) {
            return Ok((401, Vec::new(), true));
        }
        let hosts = [format!("127.0.0.1:{port}"), format!("localhost:{port}")];
        if !request
            .headers
            .get("host")
            .is_some_and(|host| hosts.iter().any(|h| h.eq_ignore_ascii_case(host)))
        {
            return Err(403);
        }
        if request.headers.get("origin").is_some_and(|origin| {
            !hosts
                .iter()
                .any(|host| origin.eq_ignore_ascii_case(&format!("http://{host}")))
        }) {
            return Err(403);
        }
        if request.path != "/mcp" {
            return Err(404);
        }
        if request.method != "POST" {
            return Err(405);
        }
        if request
            .headers
            .get("mcp-protocol-version")
            .is_some_and(|v| !SUPPORTED_VERSIONS[1..].contains(&v.as_str()))
        {
            return Err(400);
        }
        if !acceptable(&request.headers) {
            return Err(406);
        }
        if !request.headers.get("content-type").is_some_and(|v| {
            v.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("application/json")
        }) {
            return Err(415);
        }
        if request.headers.contains_key("expect") {
            return Err(417);
        }
        let bytes = body(&mut input, &request)?;
        let response = Protocol::stateless_http().handle_bytes(server, &bytes);
        match response {
            Some(response) => Ok((
                if matches!(response["error"]["code"].as_i64(), Some(-32700 | -32600)) {
                    400
                } else {
                    200
                },
                serde_json::to_vec(&response).map_err(|_| 500u16)?,
                false,
            )),
            None => Ok((202, Vec::new(), false)),
        }
    };
    let (status, body, challenge) = handle().unwrap_or_else(|status| (status, Vec::new(), false));
    send(input.reader.get_mut(), status, &body, challenge)
}
fn send(stream: &mut TcpStream, status: u16, body: &[u8], challenge: bool) -> Result<()> {
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|_| McpError::Io)?;
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        408 => "Request Timeout",
        411 => "Length Required",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        417 => "Expectation Failed",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    };
    let extra = if challenge {
        "WWW-Authenticate: Bearer\r\n"
    } else if status == 405 {
        "Allow: POST\r\n"
    } else {
        ""
    };
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n{extra}\r\n",
        body.len()
    );
    stream
        .write_all(header.as_bytes())
        .and_then(|()| stream.write_all(body))
        .map_err(|_| McpError::Io)
}
