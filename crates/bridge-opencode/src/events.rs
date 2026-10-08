//! Bounded SSE wakeups. Events never prove task completion or carry model text
//! across the adapter boundary; the observer still verifies persisted history.
use crate::{
    HttpRequest, OpenCodeClient, ResponseReader, TransportError, map_io, map_status, parse_head,
    parse_hex, remaining, write_all_until,
};
use std::{
    io::Read,
    net::{Ipv4Addr, Shutdown, SocketAddr, SocketAddrV4, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
const IO_SLICE: Duration = Duration::from_millis(200);
const FRAME_LIMIT: usize = 256 * 1024;

/// A coalesced notification for one session; dropping it closes and joins its reader.
pub struct SessionEvents {
    notices: mpsc::Receiver<()>,
    cancel: Arc<AtomicBool>,
    socket: Arc<Mutex<Option<TcpStream>>>,
    reader: Option<JoinHandle<()>>,
}
impl SessionEvents {
    /// Waits at most `timeout`; false also covers unavailable event transport.
    pub fn wait(&self, timeout: Duration) -> bool {
        match self.notices.recv_timeout(timeout) {
            Ok(()) => true,
            Err(mpsc::RecvTimeoutError::Timeout) => false,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                thread::sleep(timeout);
                false
            }
        }
    }
}
impl Drop for SessionEvents {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        if let Ok(socket) = self.socket.lock()
            && let Some(socket) = socket.as_ref()
        {
            let _ = socket.shutdown(Shutdown::Both);
        }
        if let Some(reader) = self.reader.take() {
            reader.thread().unpark();
            let _ = reader.join();
        }
    }
}
impl OpenCodeClient {
    /// Subscribes to scoped `/event` without changing the session. Unsupported
    /// servers and disconnected streams retain ordinary polling as fallback.
    pub fn subscribe_session(&self, session: &str) -> Result<SessionEvents, TransportError> {
        crate::encode_session_segment(session).ok_or(TransportError::InvalidRequest)?;
        let request = HttpRequest::get("/event")
            .with_query("directory", self.workspace.to_string_lossy().into_owned());
        let target = request.target()?;
        let bytes = self
            .transport
            .encode_with_accept(&request, &target, "text/event-stream");
        let address = SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::LOCALHOST,
            self.transport.endpoint.port(),
        ));
        let setup_timeout = self.timeout().min(IO_SLICE);
        let session = session.to_owned();
        let cancel = Arc::new(AtomicBool::new(false));
        let socket = Arc::new(Mutex::new(None));
        let (sender, notices) = mpsc::sync_channel(1);
        let (cancel_reader, shared) = (cancel.clone(), socket.clone());
        let reader = thread::Builder::new()
            .name("opencode-events".into())
            .spawn(move || {
                while !cancel_reader.load(Ordering::Acquire) {
                    let _ = stream_events(
                        address,
                        &bytes,
                        setup_timeout,
                        &session,
                        &sender,
                        &cancel_reader,
                        &shared,
                    );
                    if let Ok(mut socket) = shared.lock() {
                        *socket = None;
                    }
                    if !cancel_reader.load(Ordering::Acquire) {
                        thread::park_timeout(Duration::from_secs(1));
                    }
                }
            })
            .map_err(|_| TransportError::Unavailable)?;
        Ok(SessionEvents {
            notices,
            cancel,
            socket,
            reader: Some(reader),
        })
    }
}
fn stream_events(
    address: SocketAddr,
    request: &[u8],
    timeout: Duration,
    session: &str,
    sender: &mpsc::SyncSender<()>,
    cancel: &AtomicBool,
    shared: &Mutex<Option<TcpStream>>,
) -> Result<(), TransportError> {
    if timeout.is_zero() {
        return Err(TransportError::Timeout);
    }
    let deadline = Instant::now() + timeout;
    let mut stream = TcpStream::connect_timeout(&address, remaining(deadline)?).map_err(map_io)?;
    {
        let mut socket = shared.lock().map_err(|_| TransportError::Unavailable)?;
        if cancel.load(Ordering::Acquire) {
            return Ok(());
        }
        *socket = Some(stream.try_clone().map_err(map_io)?);
    }
    write_all_until(
        request,
        || remaining(deadline),
        |budget, bytes| {
            use std::io::Write;
            stream.set_write_timeout(Some(budget))?;
            stream.write(bytes)
        },
    )?;
    let mut reader = ResponseReader::new(&stream, deadline);
    let head = reader.read_head()?;
    let parsed = parse_head(&head)?;
    if parsed.status != 200 {
        return Err(map_status(parsed.status));
    }
    if !head.split(|b| *b == b'\n').any(|line| {
        line.iter()
            .position(|b| *b == b':')
            .map(|index| (&line[..index], &line[index + 1..]))
            .is_some_and(|(name, value)| {
                name.trim_ascii().eq_ignore_ascii_case(b"content-type")
                    && value
                        .split(|b| *b == b';')
                        .next()
                        .unwrap_or_default()
                        .trim_ascii()
                        .eq_ignore_ascii_case(b"text/event-stream")
            })
    }) {
        return Err(TransportError::Protocol);
    }
    let initial = std::mem::take(&mut reader.buffer);
    drop(reader);
    stream.set_read_timeout(Some(IO_SLICE)).map_err(map_io)?;
    let mut body = Body::new(parsed.chunked);
    let mut frames = Frames::default();
    body.feed(&initial, |bytes| frames.feed(bytes, session, sender))?;
    let mut bytes = [0; 8192];
    while !cancel.load(Ordering::Acquire) && !body.done {
        match stream.read(&mut bytes) {
            Ok(0) => return Ok(()),
            Ok(n) => body.feed(&bytes[..n], |bytes| frames.feed(bytes, session, sender))?,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::Interrupted
                ) => {}
            Err(e) => return Err(map_io(e)),
        }
    }
    Ok(())
}
// Incremental HTTP framing retains partial chunk headers/data across timeouts.
struct Body {
    chunked: bool,
    buffer: Vec<u8>,
    left: Option<usize>,
    ending: bool,
    done: bool,
}
impl Body {
    fn new(chunked: bool) -> Self {
        Self {
            chunked,
            buffer: vec![],
            left: None,
            ending: false,
            done: false,
        }
    }
    fn feed(
        &mut self,
        bytes: &[u8],
        mut consume: impl FnMut(&[u8]) -> Result<(), TransportError>,
    ) -> Result<(), TransportError> {
        if !self.chunked {
            return consume(bytes);
        }
        self.buffer.extend_from_slice(bytes);
        loop {
            if self.done {
                return Ok(());
            }
            if self.ending {
                if self.buffer.len() < 2 {
                    return Ok(());
                }
                if &self.buffer[..2] != b"\r\n" {
                    return Err(TransportError::Protocol);
                }
                self.buffer.drain(..2);
                self.ending = false;
            }
            if let Some(left) = self.left {
                let count = left.min(self.buffer.len());
                consume(&self.buffer[..count])?;
                self.buffer.drain(..count);
                if left == count {
                    self.left = None;
                    self.ending = true;
                } else {
                    self.left = Some(left - count);
                    return Ok(());
                }
            } else {
                let Some(end) = crate::find_subslice(&self.buffer, b"\r\n") else {
                    if self.buffer.len() > 8192 {
                        return Err(TransportError::Protocol);
                    }
                    return Ok(());
                };
                let size = parse_hex(
                    self.buffer[..end]
                        .split(|b| *b == b';')
                        .next()
                        .unwrap_or_default()
                        .trim_ascii(),
                )?;
                self.buffer.drain(..end + 2);
                if size == 0 {
                    self.done = true;
                    return Ok(());
                }
                self.left = Some(size);
            }
        }
    }
}
#[derive(Default)]
struct Frames {
    line: Vec<u8>,
    data: Vec<u8>,
}
impl Frames {
    fn feed(
        &mut self,
        bytes: &[u8],
        session: &str,
        sender: &mpsc::SyncSender<()>,
    ) -> Result<(), TransportError> {
        for &byte in bytes {
            if byte != b'\n' {
                self.line.push(byte);
                if self.line.len() + self.data.len() > FRAME_LIMIT {
                    return Err(TransportError::Protocol);
                }
                continue;
            }
            let line = self.line.strip_suffix(b"\r").unwrap_or(&self.line);
            if line.is_empty() {
                if relevant(&self.data, session) {
                    let _ = sender.try_send(());
                }
                self.data.clear();
            } else if let Some(value) = line.strip_prefix(b"data:") {
                if !self.data.is_empty() {
                    self.data.push(b'\n');
                }
                self.data
                    .extend_from_slice(value.strip_prefix(b" ").unwrap_or(value));
                if self.data.len() > FRAME_LIMIT {
                    return Err(TransportError::Protocol);
                }
            }
            self.line.clear();
        }
        Ok(())
    }
}
fn relevant(bytes: &[u8], session: &str) -> bool {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return false;
    };
    let properties = &value["properties"];
    let id = properties["sessionID"]
        .as_str()
        .or_else(|| properties["info"]["sessionID"].as_str());
    id == Some(session)
        && matches!(
            value["type"].as_str(),
            Some(
                "session.idle"
                    | "session.error"
                    | "session.status"
                    | "message.updated"
                    | "permission.asked"
                    | "permission.replied"
                    | "question.asked"
                    | "question.replied"
                    | "question.rejected"
            )
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn test_client(port: u16) -> (std::path::PathBuf, OpenCodeClient) {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!(
            "bridge-events-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("ws")).unwrap();
        let password = root.join("password");
        std::fs::write(&password, "event-test-password").unwrap();
        std::fs::set_permissions(&password, std::fs::Permissions::from_mode(0o600)).unwrap();
        let config = root.join("projects.toml");
        std::fs::write(&config,format!("[projects.proj]\nworkspace=\"ws\"\nopencode_url=\"http://127.0.0.1:{port}\"\npassword_file=\"password\"\nmax_rounds=3\n")).unwrap();
        let config = bridge_config::load_config(&config).unwrap();
        let client =
            OpenCodeClient::from_project(config.project("proj").unwrap(), Duration::from_secs(2))
                .unwrap();
        (root, client)
    }
    fn read_request(stream: &mut TcpStream) {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut byte = [0];
        while !bytes.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).unwrap();
            bytes.push(byte[0]);
        }
        let request = String::from_utf8(bytes).unwrap();
        assert!(request.starts_with("GET /event?directory="));
        assert!(request.contains("authorization: Basic "));
        assert!(request.contains("accept: text/event-stream"));
    }
    #[test]
    fn subscription_reconnects_after_eof_and_drop_closes_silent_stream() {
        use std::io::Write;
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let (root, client) = test_client(listener.local_addr().unwrap().port());
        let server = thread::spawn(move || {
            let (mut first, _) = listener.accept().unwrap();
            read_request(&mut first);
            first
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n: ping\n\n")
                .unwrap();
            drop(first);
            let (mut second, _) = listener.accept().unwrap();
            read_request(&mut second);
            second.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: Text/Event-Stream; charset=utf-8\r\nTransfer-Encoding: chunked\r\n\r\n").unwrap();
            let event =
                b"data: {\"type\":\"session.idle\",\"properties\":{\"sessionID\":\"own\"}}\n\n";
            let encoded = format!(
                "{:x}\r\n{}\r\n",
                event.len(),
                std::str::from_utf8(event).unwrap()
            );
            for chunk in encoded.as_bytes().chunks(7) {
                second.write_all(chunk).unwrap();
            }
            let mut byte = [0];
            assert_eq!(second.read(&mut byte).unwrap(), 0);
        });
        let events = client.subscribe_session("own").unwrap();
        assert!(events.wait(Duration::from_secs(3)));
        let start = Instant::now();
        drop(events);
        assert!(start.elapsed() < Duration::from_millis(500));
        server.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn drop_interrupts_stalled_headers_and_invalid_session_starts_no_reader() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let (root, client) = test_client(listener.local_addr().unwrap().port());
        assert!(client.subscribe_session("").is_err());
        let (ready, connected) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_request(&mut stream);
            ready.send(()).unwrap();
            let mut byte = [0];
            assert_eq!(stream.read(&mut byte).unwrap(), 0);
        });
        let events = client.subscribe_session("own").unwrap();
        connected.recv_timeout(Duration::from_secs(2)).unwrap();
        let start = Instant::now();
        drop(events);
        assert!(start.elapsed() < Duration::from_millis(500));
        server.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn fragmented_chunked_multiline_sse_filters_sessions_and_coalesces_duplicates() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let mut frames = Frames::default();
        let mut body = Body::new(true);
        let messages = concat!(
            ": heartbeat\r\n\r\n",
            "data: {\"type\":\"session.idle\",\"properties\":{\"sessionID\":\"other\"}}\n\n",
            "data: {\"type\":\"message.updated\",\n",
            "data: \"properties\":{\"info\":{\"sessionID\":\"own\"}}}\r\n\r\n",
            "data: {\"type\":\"session.idle\",\"properties\":{\"sessionID\":\"own\"}}\n\n"
        );
        let encoded = format!(
            "{:x};extension=ok\r\n{messages}\r\n0\r\n\r\n",
            messages.len()
        );
        for bytes in encoded.as_bytes().chunks(3) {
            body.feed(bytes, |bytes| frames.feed(bytes, "own", &sender))
                .unwrap();
        }
        assert!(body.done);
        assert_eq!(receiver.try_iter().count(), 1);
    }
    #[test]
    fn malformed_or_unscoped_events_never_wake_and_oversized_frames_fail_bounded() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let mut frames = Frames::default();
        frames
            .feed(
                b"data: invalid\n\ndata: {\"type\":\"session.idle\"}\n\n",
                "own",
                &sender,
            )
            .unwrap();
        assert!(receiver.try_recv().is_err());
        assert!(
            frames
                .feed(&vec![b'x'; FRAME_LIMIT + 1], "own", &sender)
                .is_err()
        );
        assert!(frames.line.len() <= FRAME_LIMIT + 1);
        let mut body = Body::new(true);
        assert!(body.feed(b"broken\r\n", |_| Ok(())).is_err());
    }
}
