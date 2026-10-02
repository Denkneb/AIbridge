//! Minimal OpenCode HTTP transport, Basic authentication (task 6.1),
//! health/workspace identity probing (task 6.2), OpenAPI compatibility (task
//! 6.3), session create/list/get (task 6.4), session message list/parsing (task
//! 6.5), async prompt delivery (task 6.6), permissions list/reply (task 6.7)
//! and questions/blockers (task 6.8).
//!
//! This crate implements the first steps of the OpenCode adapter stream: a
//! small, reusable HTTP/1.1 transport that talks to an already validated
//! OpenCode loopback endpoint and attaches the Basic Authorization credential
//! the OpenCode server expects, plus the typed health and workspace identity
//! checks the reference `opencode_client.py` performs before any session is
//! touched, plus the OpenAPI compatibility check (6.3) that validates the
//! installed `/doc` document against the mandatory operations and schemas, plus
//! the typed session operations (6.4), the typed session message list and
//! message/parts parsing (6.5), the typed async prompt delivery (6.6), the
//! typed permission list/reply operations (6.7) and the typed question list
//! plus the minimal session-scoped permission/question blocker detection (6.8).
//! It deliberately stops before the worker/MCP/runtime wiring (task 7.1+).
//!
//! # Endpoint and auth material
//!
//! The transport consumes the typed, validated values produced by
//! [`bridge_config`] instead of re-parsing configuration:
//!
//! - [`bridge_config::Endpoint`] is always `http://127.0.0.1:<port>` with an
//!   explicit port and no path, query or credentials;
//! - [`bridge_config::Secret`] is the single, non-empty password line read from
//!   the project `password_file` (task 2.8).
//!
//! [`BasicAuth`] wraps that [`bridge_config::Secret`] and renders the exact
//! header the reference `opencode_client.py` sends through `httpx`: the
//! constant user name `opencode` and the password, joined by `:` and encoded
//! with standard Base64. [`BasicAuth::from_project`] wires
//! [`bridge_config::ProjectEntry::read_password`] to a typed
//! [`TransportError::InvalidAuth`] when the password file is missing, unsafe or
//! unreadable, so configuration validation is reused and never duplicated.
//! The OpenCode contract always requires a password (`password_file` is a
//! required project key), so the transport has no unauthenticated mode: every
//! request carries the Basic credential.
//!
//! # Health and workspace identity
//!
//! [`OpenCodeClient`] binds the transport to one canonical workspace and
//! reproduces the reference probes exactly:
//!
//! - [`OpenCodeClient::health`] sends `GET /global/health?directory=<workspace>`
//!   (the reference client scopes the health request with the `directory`
//!   query) and treats the endpoint as healthy only when the JSON object has a
//!   literal boolean `true` under `healthy`, exactly like the reference
//!   `health().get("healthy") is True` check; the optional `version` string is
//!   carried through as a typed value.
//! - [`OpenCodeClient::verify_workspace`] sends `GET /path` **without** the
//!   `directory` query. The reference `get_server_path` documents that the
//!   scoped `/path` merely echoes the caller's own workspace, so only the
//!   server's own root can prove identity. The reported `directory` string is
//!   resolved with the reference's non-strict, symlink-aware `Path.resolve()`
//!   semantics (a missing component is appended while the walk continues, so a
//!   following `..` is folded against the symlink-resolved prefix) and compared
//!   to the configured canonical workspace; a missing field, a non-string
//!   value, a non-object body, a non-matching resolved path, an embedded NUL, a
//!   symlink loop or any non-`NotFound` filesystem failure fails closed.
//!
//! Both probes return typed errors ([`HealthError`], [`IdentityError`]) that
//! preserve the [`TransportError`] categories (timeout, unavailable, HTTP 401,
//! other non-success, protocol) and never render a workspace path or response
//! content.
//!
//! # OpenAPI compatibility
//!
//! [`OpenCodeClient::check_compatibility`] sends `GET /doc` **scoped** with the
//! workspace `directory` query (the reference `get_doc` uses `scoped=True`) and
//! runs the pure [`openapi_problems`] checker over the decoded document. The
//! checker reproduces the reference `opencode_client.py` semantics exactly:
//!
//! - mandatory routes and methods ([`CompatibilityProblem::MissingPath`],
//!   [`CompatibilityProblem::MissingOperation`]) with path parameter names
//!   normalized away, so `/session/{id}` and `/session/{sessionID}` both satisfy
//!   the `/session/{}` contract;
//! - mandatory `Path`/`Session`/`AssistantMessage` schema properties and the
//!   nested `AssistantMessage.time.completed` property;
//! - the JSON `prompt_async` body (`messageID`, `parts`, and `model` only when
//!   [`OpenCodeClient::require_prompt_model`] is set);
//! - the permission-reply body, its `reply` field and the `once`/`always`/
//!   `reject` enum.
//!
//! Two path spellings that normalize to the same template are resolved
//! first-wins in the document's JSON insertion order, matching the reference
//! `setdefault` (the `serde_json` `preserve_order` feature keeps that order).
//! Local `$ref` chains are resolved with cycle protection; an unresolvable
//! reference (non-string, external, cyclic or missing token) becomes an empty
//! node, so sibling properties can never prove a mandatory structure, and a
//! non-object `properties` fails closed instead of silently passing. The checker
//! never panics on a malformed document: invalid JSON is [`DocError::Malformed`],
//! a non-object document or malformed nested containers fail closed with a
//! compatibility problem. It is deliberately not a general-purpose OpenAPI
//! validator.
//!
//! [`OpenCodeClient::from_project`] applies the project's `opencode_model`
//! setting unambiguously: a project that selects a model sets
//! [`OpenCodeClient::require_prompt_model`] from
//! [`bridge_config::ProjectEntry::opencode_model`], so callers never guess
//! whether `model` is required. This is a check for the *presence* of the
//! `model` field in the API body, never a check that a specific provider/model
//! exists on the server.
//!
//! # Sessions
//!
//! [`OpenCodeClient::list_sessions`], [`OpenCodeClient::create_session`] and
//! [`OpenCodeClient::get_session`] reproduce the three session operations of the
//! reference `opencode_client.py`, all scoped with the workspace `directory`
//! query and authenticated through the shared transport:
//!
//! - `GET /session` returns the session list (reference `list_sessions`);
//! - `POST /session` with the compact JSON body `{"title": <title>}` creates a
//!   session (reference `create_session`);
//! - `GET /session/<id>` fetches one session (reference `get_session`).
//!
//! The session id is percent-encoded as a single RFC 3986 path segment instead
//! of being interpolated raw into the path. For the alphanumeric `ses...` ids
//! the server actually issues this is a byte-for-byte no-op, but it makes the
//! request path provably safe if an id ever contains `/`, `?`, `#`, a space or
//! any other byte: such an id can no longer change the request target or inject
//! a query. An empty id (and the bare `.`/`..` dot-segments, which a server
//! could resolve to a different route) is rejected as
//! [`SessionError::InvalidSessionId`] before a request is sent. This is a
//! deliberate deviation from the reference, which interpolates the id verbatim;
//! the shared transport's stricter `2xx`-only success policy is another
//! deliberate deviation, documented under "Prompts".
//!
//! The typed [`Session`] exposes the `id`, `title` and `directory` string
//! fields the reference reads from a session object, each as an `Option`. A
//! response that is not valid JSON, or whose top level is not an object
//! (create/get) or an array (list), is [`SessionError::Malformed`]; a list
//! element that is not a JSON object is skipped, exactly like the reference
//! `isinstance(session, dict)` filter, so a stray scalar cannot hide the real
//! sessions.
//!
//! # Messages
//!
//! [`OpenCodeClient::list_messages`] reproduces the reference `list_messages`
//! (`GET /session/<id>/message`, scoped with the workspace `directory` and
//! authenticated through the shared transport) and parses the returned array
//! into typed [`Message`] values. Each message preserves the `info`/`parts`
//! shape the reference worker consumers read:
//!
//! - [`MessageInfo`] carries identity (`id`/`role`/`parentID`/`sessionID`), the
//!   assistant lifecycle fields (`time.completed`, `finish`, `error`), the
//!   provider/model identity and the normalized token/cost accounting;
//! - [`MessagePart`] carries the text content (`type`, `text`, `ignored`) and
//!   the tool lifecycle (`tool`, `state.status`, `state.error`,
//!   `metadata.providerExecuted`, `state.metadata.interrupted`).
//!
//! The parsing is permissive exactly like the reference `message.get(...)` /
//! `part.get(...)` accessors for scalar fields (a missing or wrongly typed one
//! becomes `None`/`false`/zero) and for an absent `info`/`parts` (the reference
//! defaults `{}`/`[]`). A present but wrongly typed lifecycle container or
//! message/part element is instead [`MessageError::Malformed`] (the reference
//! would call `.get` on it and raise), so a malformed structure can neither
//! panic nor be mistaken for a completed turn, and a damaged trailing element
//! cannot hide a later unfinished message. A top-level body that is not a JSON
//! array is also [`MessageError::Malformed`]. The id is percent-encoded as one
//! RFC 3986 path segment like the session operations, and an unusable id is
//! [`MessageError::InvalidSessionId`] before any request.
//!
//! # Prompts
//!
//! [`OpenCodeClient::send_prompt_async`] reproduces the reference
//! `send_prompt_async` (`POST /session/<id>/prompt_async`, scoped with the
//! workspace `directory` and authenticated through the shared transport). It
//! only *delivers* the prompt: the reference `_request` helper returns as soon
//! as the server accepts the request, so a successful call is deliberately not
//! an assertion that an assistant turn completed. The JSON body is the compact
//! UTF-8 object httpx serializes (`ensure_ascii=False`, `separators=(",", ":")`):
//! `{"messageID": <id>, "parts": [{"type": "text", "text": <text>}]}`, plus
//! `"model": {"providerID": ..., "modelID": ...}` inserted last only when the
//! client was built from a project that selects [`bridge_config::OpenCodeModel`]
//! ([`OpenCodeClient::from_project`]); a client without a model sends no `model`
//! field at all, exactly like the reference `config.opencode_model is None`
//! branch. The session id is percent-encoded as one RFC 3986 path segment by the
//! same shared code and percent-triplet validation the session/message
//! operations use, so an unusable id is [`PromptError::InvalidSessionId`] before
//! any request; the `messageID` travels inside the JSON body and is escaped by
//! the serializer like any other string. Any `2xx` response, including a
//! bodyless `204`, is success. The shared transport still reads and frames the
//! successful response body per HTTP framing (except a bodyless `204`) before it
//! returns the [`HttpResponse`], exactly like the reference `_request` (httpx
//! also reads the body), but `send_prompt_async` never interprets or parses that
//! body as JSON — hence there is no `Malformed` variant. Transport categories
//! (timeout, unavailable, HTTP 401, HTTP 404, other non-success, protocol) are
//! preserved as [`PromptError::Transport`]. The reference `_request` accepts any
//! status `< 400` (including `3xx`) while the shared transport only accepts
//! `2xx`; the transport policy is deliberately not weakened for prompt delivery,
//! so a `3xx` is surfaced as [`TransportError::HttpStatus`]. There is no
//! automatic retry: the delivery is not idempotent, and the reference does not
//! retry it. A transport error after the request was written leaves the delivery
//! outcome undefined, so it must not be read as a guarantee that nothing was
//! sent.
//!
//! # Redaction
//!
//! The password, the `Authorization` header, response bodies, request paths,
//! query strings, workspace paths and OS error details must never leak through
//! a [`fmt::Debug`] or [`fmt::Display`] representation. Every public type in
//! this crate therefore implements both traits explicitly and renders only
//! non-sensitive structure: [`BasicAuth`] and the request path/query/body are
//! redacted, [`HttpResponse`] exposes only its status, and
//! [`TransportError`] carries only a static label plus, for an unsuccessful
//! status, the numeric HTTP status code. [`OpenCodeClient`] redacts its
//! workspace, [`Health`] redacts the server-reported `version`, [`Session`]
//! redacts its id/title/directory, [`Message`]/[`MessageInfo`]/[`MessagePart`]
//! redact message content, and the
//! [`HealthError`]/[`IdentityError`]/[`SessionError`]/[`MessageError`]/
//! [`PromptError`] representations render only static labels plus the nested
//! [`TransportError`]. The prompt model stored on [`OpenCodeClient`] is redacted
//! like the workspace, and the prompt session id, message id, text and body
//! never appear in any `Debug`/`Display` output or error.
//!
//! # Transport behavior
//!
//! [`HttpTransport::request`] performs one request over a fresh
//! `Connection: close` `TcpStream` to the loopback endpoint. The whole request
//! is bounded by the caller-supplied timeout: connect, every partial write and
//! every socket read share a single deadline, and the remaining time is
//! recomputed before each blocking operation, so a slow-reading or stalled peer
//! can never extend the request. The result distinguishes a timeout, a
//! connection/transport failure, a rejected credential (HTTP 401), a missing
//! resource (HTTP 404), any other non-success status, a malformed request and a
//! malformed response. Only `2xx` responses are successful, and the response
//! body of an unsuccessful status is never read into memory. A successful
//! `204 No Content` is treated as bodyless regardless of framing headers, so an
//! open connection cannot make the transport wait for EOF; a `304 Not Modified`
//! is not a success status, so it is rejected as a non-success error before any
//! body handling and its body is never read.

use std::collections::{HashMap, VecDeque};
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use bridge_config::{Endpoint, OpenCodeModel, ProjectEntry, Secret};

/// The fixed Basic Auth user name of the OpenCode server contract.
///
/// The reference `credentials.py` defines `USERNAME = "opencode"`; the bridge
/// authenticates every request with this user name and the project password.
pub const OPENCODE_BASIC_USERNAME: &str = "opencode";

/// The default request timeout, matching the reference `OpenCodeClient` timeout
/// of 30 seconds.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// The exact `Accept` header the reference client sends.
const ACCEPT_HEADER: &str = "application/json";

/// The maximum accepted response head size, a fail-closed guard against a
/// peer that never terminates its header block.
const MAX_HEAD_BYTES: usize = 64 * 1024;

/// The maximum accepted response body size, a fail-closed guard against a
/// peer that never terminates its body.
const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

/// The HTTP request methods the OpenCode contract depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    /// An HTTP `GET` request.
    Get,
    /// An HTTP `POST` request.
    Post,
}

impl HttpMethod {
    /// Returns the uppercase method token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
        }
    }

    /// Returns the lowercase key an OpenAPI path item uses for this method.
    const fn spec_token(self) -> &'static str {
        match self {
            Self::Get => "get",
            Self::Post => "post",
        }
    }
}

impl fmt::Display for HttpMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A single request to send to the OpenCode endpoint.
///
/// The path is relative to the endpoint root and must start with `/` and
/// contain only RFC 3986 path characters; query pairs are encoded as
/// `application/x-www-form-urlencoded` (so the workspace `directory` value is
/// percent-encoded exactly like the reference client). A body, when present,
/// is sent as `application/json`.
///
/// The [`fmt::Debug`] representation never renders the path, the query or the
/// body, because a query or body can carry a workspace path or prompt text.
#[derive(Clone)]
pub struct HttpRequest {
    method: HttpMethod,
    path: String,
    query: Vec<(String, String)>,
    body: Option<Vec<u8>>,
}

impl HttpRequest {
    /// Builds a `GET` request for `path`.
    #[must_use]
    pub fn get(path: impl Into<String>) -> Self {
        Self {
            method: HttpMethod::Get,
            path: path.into(),
            query: Vec::new(),
            body: None,
        }
    }

    /// Builds a `POST` request for `path` with a JSON `body`.
    #[must_use]
    pub fn post(path: impl Into<String>, body: Vec<u8>) -> Self {
        Self {
            method: HttpMethod::Post,
            path: path.into(),
            query: Vec::new(),
            body: Some(body),
        }
    }

    /// Appends one query pair, preserving insertion order.
    #[must_use]
    pub fn with_query(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.query.push((name.into(), value.into()));
        self
    }

    /// Returns the request method.
    #[must_use]
    pub const fn method(&self) -> HttpMethod {
        self.method
    }

    /// Returns the request body, if any.
    #[must_use]
    pub fn body(&self) -> Option<&[u8]> {
        self.body.as_deref()
    }

    /// Builds the request target (`path` plus an encoded query, if any).
    fn target(&self) -> Result<String, TransportError> {
        validate_path(&self.path)?;
        if self.query.is_empty() {
            return Ok(self.path.clone());
        }
        let mut serializer = url::form_urlencoded::Serializer::new(String::new());
        for (name, value) in &self.query {
            serializer.append_pair(name, value);
        }
        let mut target = self.path.clone();
        target.push('?');
        target.push_str(&serializer.finish());
        Ok(target)
    }
}

impl fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpRequest")
            .field("method", &self.method)
            .field("path", &"[redacted]")
            .field("query_pairs", &self.query.len())
            .field("body", &self.body.as_ref().map(|_| "[redacted]"))
            .finish()
    }
}

/// A received OpenCode HTTP response.
///
/// Only successful (`2xx`) responses reach the caller; the raw status is
/// available through [`HttpResponse::status`] and the decoded body (with any
/// `Transfer-Encoding: chunked` framing removed) through
/// [`HttpResponse::body`]. The [`fmt::Debug`] representation never renders the
/// body.
pub struct HttpResponse {
    status: u16,
    body: Vec<u8>,
}

impl HttpResponse {
    /// Returns the HTTP status code.
    #[must_use]
    pub const fn status(&self) -> u16 {
        self.status
    }

    /// Returns the decoded body bytes.
    #[must_use]
    pub fn body(&self) -> &[u8] {
        &self.body
    }
}

impl fmt::Debug for HttpResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpResponse")
            .field("status", &self.status)
            .field("body_bytes", &self.body.len())
            .field("body", &"[redacted]")
            .finish()
    }
}

/// A typed transport failure.
///
/// The variants separate the failure categories a caller has to react to,
/// mirroring the reference `OpenCodeError` hierarchy:
///
/// - [`TransportError::Timeout`] — the request exceeded its deadline;
/// - [`TransportError::Unavailable`] — the connection or socket failed;
/// - [`TransportError::InvalidAuth`] — the project credential could not be
///   read (reference `OpenCodeAuthError` setup side);
/// - [`TransportError::Unauthorized`] — the server rejected the credential
///   with HTTP 401 (reference `OpenCodeAuthError`);
/// - [`TransportError::NotFound`] — HTTP 404 (reference `OpenCodeNotFound`);
/// - [`TransportError::HttpStatus`] — any other non-success status (reference
///   `OpenCodeHTTPError`);
/// - [`TransportError::InvalidRequest`] — the request itself was malformed and
///   was never sent;
/// - [`TransportError::Protocol`] — the response was not valid HTTP.
///
/// Neither [`fmt::Display`] nor [`fmt::Debug`] renders the endpoint, request
/// path, query, headers, body or OS details; only the static label and the
/// numeric status code of [`TransportError::HttpStatus`] are shown.
#[derive(Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TransportError {
    /// The request exceeded the configured timeout.
    Timeout,
    /// The connection or socket operation failed.
    Unavailable,
    /// The project credential could not be read.
    InvalidAuth,
    /// The server rejected the credential with HTTP 401.
    Unauthorized,
    /// The server reported HTTP 404.
    NotFound,
    /// The server returned any other non-success status.
    HttpStatus(u16),
    /// The request was malformed and was never sent.
    InvalidRequest,
    /// The response was not valid HTTP.
    Protocol,
}

impl TransportError {
    /// Returns the numeric HTTP status when this error came from a response
    /// that was not mapped to a dedicated variant.
    #[must_use]
    pub const fn status(self) -> Option<u16> {
        match self {
            Self::HttpStatus(status) => Some(status),
            _ => None,
        }
    }
}

impl fmt::Debug for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout => f.write_str("Timeout"),
            Self::Unavailable => f.write_str("Unavailable"),
            Self::InvalidAuth => f.write_str("InvalidAuth"),
            Self::Unauthorized => f.write_str("Unauthorized"),
            Self::NotFound => f.write_str("NotFound"),
            Self::HttpStatus(status) => write!(f, "HttpStatus({status})"),
            Self::InvalidRequest => f.write_str("InvalidRequest"),
            Self::Protocol => f.write_str("Protocol"),
        }
    }
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout => f.write_str("OpenCode request timed out"),
            Self::Unavailable => f.write_str("OpenCode endpoint is unreachable"),
            Self::InvalidAuth => f.write_str("OpenCode auth material could not be read"),
            Self::Unauthorized => f.write_str("OpenCode rejected the credentials"),
            Self::NotFound => f.write_str("OpenCode reported not found"),
            Self::HttpStatus(status) => write!(f, "OpenCode returned HTTP status {status}"),
            Self::InvalidRequest => f.write_str("OpenCode request is malformed"),
            Self::Protocol => f.write_str("OpenCode response violated HTTP"),
        }
    }
}

impl Error for TransportError {}

/// An explicit Basic Auth credential for the OpenCode endpoint.
///
/// The credential is built from the validated [`bridge_config::Secret`], so it
/// can never be empty. The [`fmt::Debug`] and [`fmt::Display`] representations
/// always render a redacted marker and never the password or the encoded
/// header.
#[derive(Clone)]
pub struct BasicAuth {
    secret: Secret,
}

impl BasicAuth {
    /// Builds an OpenCode Basic Auth credential from a validated secret.
    #[must_use]
    pub fn new(secret: Secret) -> Self {
        Self { secret }
    }

    /// Reads the credential from a validated project entry.
    ///
    /// This reuses [`bridge_config::ProjectEntry::read_password`] and maps any
    /// read failure (missing, symlinked, unsafe, non-UTF-8 or multi-line
    /// password file) to [`TransportError::InvalidAuth`]; the underlying
    /// message, the path and the file contents are never exposed.
    ///
    /// # Errors
    ///
    /// Returns [`TransportError::InvalidAuth`] when the password file cannot be
    /// read or validated.
    pub fn from_project(project: &ProjectEntry) -> Result<Self, TransportError> {
        let secret = project
            .read_password()
            .map_err(|_| TransportError::InvalidAuth)?;
        Ok(Self { secret })
    }

    /// Builds the `Authorization` header value.
    ///
    /// The encoding matches the reference `httpx` Basic auth exactly: the
    /// UTF-8 bytes of `opencode:<password>` encoded with standard Base64.
    fn header_value(&self) -> String {
        let mut credentials = String::with_capacity(
            OPENCODE_BASIC_USERNAME.len() + 1 + self.secret.expose_secret().len(),
        );
        credentials.push_str(OPENCODE_BASIC_USERNAME);
        credentials.push(':');
        credentials.push_str(self.secret.expose_secret());
        format!("Basic {}", base64_encode(credentials.as_bytes()))
    }
}

impl fmt::Debug for BasicAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BasicAuth([redacted])")
    }
}

impl fmt::Display for BasicAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

/// A reusable HTTP/1.1 transport bound to one validated OpenCode endpoint.
///
/// The transport owns the validated [`bridge_config::Endpoint`], the typed
/// [`BasicAuth`] credential and the request timeout. Every call to
/// [`HttpTransport::request`] opens a fresh `Connection: close` loopback
/// connection and authenticates it; the encrypted credentials never appear in
/// the [`fmt::Debug`] output.
pub struct HttpTransport {
    endpoint: Endpoint,
    auth: BasicAuth,
    timeout: Duration,
}

impl HttpTransport {
    /// Binds a transport to a validated endpoint, credential and timeout.
    ///
    /// A zero timeout is accepted here but every request then fails closed with
    /// [`TransportError::Timeout`], because the deadline is already exceeded.
    #[must_use]
    pub fn new(endpoint: Endpoint, auth: BasicAuth, timeout: Duration) -> Self {
        Self {
            endpoint,
            auth,
            timeout,
        }
    }

    /// Returns the configured request timeout.
    #[must_use]
    pub const fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Sends one request and returns the decoded successful response.
    ///
    /// The whole exchange (connect, every partial write and every read) is
    /// bounded by a single deadline derived from the configured timeout, and the
    /// remaining time is recomputed before each blocking operation, so a peer
    /// that accepts the connection, stalls or trickles bytes cannot extend the
    /// request. A `2xx` response is only returned while the deadline still
    /// holds.
    ///
    /// # Errors
    ///
    /// Returns the typed [`TransportError`] for a timeout, a connection or
    /// socket failure, a malformed request, a malformed response, HTTP 401,
    /// HTTP 404 or any other non-success status. The body of an unsuccessful
    /// response is never read or exposed.
    pub fn request(&self, request: &HttpRequest) -> Result<HttpResponse, TransportError> {
        if self.timeout.is_zero() {
            return Err(TransportError::Timeout);
        }
        let target = request.target()?;
        let deadline = Instant::now() + self.timeout;
        let address = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, self.endpoint.port()));
        let mut stream =
            TcpStream::connect_timeout(&address, remaining(deadline)?).map_err(map_io)?;
        let bytes = self.encode(request, &target);
        write_all_until(
            &bytes,
            || remaining(deadline),
            |budget, chunk| {
                stream.set_write_timeout(Some(budget))?;
                stream.write(chunk)
            },
        )?;

        let mut reader = ResponseReader::new(&stream, deadline);
        let head = reader.read_head()?;
        let parsed = parse_head(&head)?;
        if !(200..=299).contains(&parsed.status) {
            return Err(map_status(parsed.status));
        }
        let body = if status_has_no_body(parsed.status) {
            Vec::new()
        } else {
            reader.read_body(&parsed)?
        };
        remaining(deadline)?;
        Ok(HttpResponse {
            status: parsed.status,
            body,
        })
    }

    /// Serializes the request head and body into the bytes sent on the wire.
    fn encode(&self, request: &HttpRequest, target: &str) -> Vec<u8> {
        let mut head = String::new();
        head.push_str(request.method.as_str());
        head.push(' ');
        head.push_str(target);
        head.push_str(" HTTP/1.1\r\n");
        head.push_str("host: ");
        head.push_str(self.endpoint.host());
        head.push(':');
        head.push_str(&self.endpoint.port().to_string());
        head.push_str("\r\n");
        head.push_str("authorization: ");
        head.push_str(&self.auth.header_value());
        head.push_str("\r\n");
        head.push_str("accept: ");
        head.push_str(ACCEPT_HEADER);
        head.push_str("\r\n");
        head.push_str("connection: close\r\n");
        if let Some(body) = &request.body {
            head.push_str("content-type: application/json\r\n");
            head.push_str("content-length: ");
            head.push_str(&body.len().to_string());
            head.push_str("\r\n");
        }
        head.push_str("\r\n");
        let mut bytes = head.into_bytes();
        if let Some(body) = &request.body {
            bytes.extend_from_slice(body);
        }
        bytes
    }
}

impl fmt::Debug for HttpTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpTransport")
            .field("endpoint", &self.endpoint)
            .field("auth", &self.auth)
            .field("timeout", &self.timeout)
            .finish()
    }
}

/// The reference health endpoint, scoped by the caller's `directory` query.
const HEALTH_PATH: &str = "/global/health";

/// The reference server-root endpoint, deliberately queried without a
/// `directory` context so it cannot echo the caller's own workspace.
const PATH_PATH: &str = "/path";

/// The reference OpenAPI document endpoint, queried scoped with the workspace
/// `directory` (reference `get_doc` calls `_json("GET", "/doc")` with the
/// default `scoped=True`).
const DOC_PATH: &str = "/doc";

/// The reference session collection endpoint (task 6.4). `GET` lists sessions,
/// `POST` creates one; both are scoped with the workspace `directory` query.
const SESSION_PATH: &str = "/session";

/// The mandatory OpenCode operations, with path parameter names normalized away
/// so a `{id}`/`{sessionID}` spelling difference does not matter. A path that
/// exists with the wrong method is just as incompatible as a missing path.
const REQUIRED_OPERATIONS: &[(&str, &[HttpMethod])] = &[
    ("/global/health", &[HttpMethod::Get]),
    ("/path", &[HttpMethod::Get]),
    ("/session", &[HttpMethod::Get, HttpMethod::Post]),
    ("/session/{}", &[HttpMethod::Get]),
    ("/session/{}/message", &[HttpMethod::Get]),
    ("/session/{}/prompt_async", &[HttpMethod::Post]),
    ("/session/status", &[HttpMethod::Get]),
    ("/permission", &[HttpMethod::Get]),
    ("/permission/{}/reply", &[HttpMethod::Post]),
    ("/question", &[HttpMethod::Get]),
];

/// The reference permission-collection endpoint (task 6.7), scoped with the
/// workspace `directory` query.
const PERMISSION_PATH: &str = "/permission";

/// The reference question-collection endpoint (task 6.8), scoped with the
/// workspace `directory` query (reference `list_questions`).
const QUESTION_PATH: &str = "/question";

/// The reference `worker.py::_pending_questions` truncation of the first
/// question's text (`text[:300]`, counted in Unicode code points like Python).
const QUESTION_BLOCKER_TEXT_LIMIT: usize = 300;

/// The normalized path whose JSON request body carries the prompt parts.
const PROMPT_ASYNC_PATH: &str = "/session/{}/prompt_async";

/// The normalized path of the installed v1 permission-reply operation.
const PERMISSION_REPLY_PATH: &str = "/permission/{}/reply";

/// The permission replies the reference bridge depends on.
const PERMISSION_REPLIES: [&str; 3] = ["once", "always", "reject"];

/// Schema properties the adapter reads directly.
const REQUIRED_SCHEMA_PROPERTIES: &[(&str, &[&str])] = &[
    ("Path", &["directory"]),
    ("Session", &["directory"]),
    ("AssistantMessage", &["parentID", "time", "finish"]),
];

/// A fail-closed empty JSON object used when a `$ref` cannot be resolved (a
/// missing token, a non-string `$ref`, an external reference or a cycle). It is
/// also the reference `_resolve_ref`'s fresh `{}` for a missing token, so a
/// broken reference never proves a mandatory structure.
static EMPTY_OBJECT: LazyLock<serde_json::Value> =
    LazyLock::new(|| serde_json::Value::Object(serde_json::Map::new()));

/// A typed OpenCode health result (task 6.2).
///
/// The endpoint is healthy only when the response JSON object carries a literal
/// boolean `true` under `healthy`, mirroring the reference
/// `health().get("healthy") is True`. Every other value (missing field, a
/// string, a number, `false`) yields `healthy == false`, so a malformed or
/// incomplete health payload can never be mistaken for a ready server.
///
/// The [`fmt::Debug`]/[`fmt::Display`] representations render the semantic
/// `healthy` flag but never the server-reported `version`; the version is
/// available only through the explicit [`Health::version`] accessor.
#[derive(Clone, PartialEq, Eq)]
pub struct Health {
    healthy: bool,
    version: Option<String>,
}

impl Health {
    /// Returns `true` only when the server reported a literal boolean `true`.
    #[must_use]
    pub const fn healthy(&self) -> bool {
        self.healthy
    }

    /// Returns the optional server-reported version string.
    #[must_use]
    pub fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }
}

impl fmt::Debug for Health {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Health")
            .field("healthy", &self.healthy)
            .field("version", &self.version.as_ref().map(|_| "[redacted]"))
            .finish()
    }
}

impl fmt::Display for Health {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "OpenCode health: healthy={}", self.healthy)
    }
}

/// A typed health-probe failure.
///
/// [`HealthError::Transport`] preserves the underlying [`TransportError`]
/// (timeout, unavailable, HTTP 401, other non-success, protocol);
/// [`HealthError::Malformed`] means the successful response body was not a JSON
/// object. Neither [`fmt::Debug`] nor [`fmt::Display`] renders the endpoint,
/// the query, the credential or the response body.
#[derive(Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum HealthError {
    /// The transport itself failed.
    Transport(TransportError),
    /// The response body was not a JSON object.
    Malformed,
}

impl From<TransportError> for HealthError {
    fn from(error: TransportError) -> Self {
        Self::Transport(error)
    }
}

impl fmt::Debug for HealthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(error) => write!(f, "Transport({error:?})"),
            Self::Malformed => f.write_str("Malformed"),
        }
    }
}

impl fmt::Display for HealthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(error) => write!(f, "OpenCode health request failed: {error}"),
            Self::Malformed => f.write_str("OpenCode health response is not a JSON object"),
        }
    }
}

impl Error for HealthError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Transport(error) => Some(error),
            Self::Malformed => None,
        }
    }
}

/// A typed workspace-identity failure.
///
/// Identity fails closed on every condition that would make the server root
/// unprovable:
///
/// - [`IdentityError::Transport`] — the `/path` request itself failed;
/// - [`IdentityError::Malformed`] — the body was not a JSON object or
///   `directory` was not a string;
/// - [`IdentityError::MissingDirectory`] — the JSON object omitted `directory`;
/// - [`IdentityError::Mismatch`] — the resolved server root is not the
///   configured workspace, or the reported path cannot be resolved at all (an
///   embedded NUL, a symlink loop or a non-`NotFound` filesystem failure), so
///   identity fails closed.
///
/// Neither [`fmt::Debug`] nor [`fmt::Display`] renders the reported path, the
/// configured workspace, the query, the credential or the response body.
#[derive(Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum IdentityError {
    /// The transport itself failed.
    Transport(TransportError),
    /// The response body was not a JSON object, or `directory` was not a
    /// string.
    Malformed,
    /// The `/path` response omitted the `directory` field.
    MissingDirectory,
    /// The resolved server root does not match the configured workspace.
    Mismatch,
}

impl From<TransportError> for IdentityError {
    fn from(error: TransportError) -> Self {
        Self::Transport(error)
    }
}

impl fmt::Debug for IdentityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(error) => write!(f, "Transport({error:?})"),
            Self::Malformed => f.write_str("Malformed"),
            Self::MissingDirectory => f.write_str("MissingDirectory"),
            Self::Mismatch => f.write_str("Mismatch"),
        }
    }
}

impl fmt::Display for IdentityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(error) => write!(f, "OpenCode /path request failed: {error}"),
            Self::Malformed => f.write_str("OpenCode /path response is malformed"),
            Self::MissingDirectory => f.write_str("OpenCode /path did not return a directory"),
            Self::Mismatch => {
                f.write_str("OpenCode server root does not match the configured workspace")
            }
        }
    }
}

impl Error for IdentityError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Transport(error) => Some(error),
            Self::Malformed | Self::MissingDirectory | Self::Mismatch => None,
        }
    }
}

/// One incompatibility found in an OpenCode `/doc` document (task 6.3).
///
/// Each variant is a stable category. The only strings a variant carries are
/// mandatory contract names (required path, schema and property names) taken
/// from this crate's own constants, never an arbitrary path, schema name,
/// `$ref` or document value, so a [`fmt::Debug`]/[`fmt::Display`]
/// representation cannot leak document content.
///
/// [`fmt::Display`] renders the same stable text the reference
/// `openapi_problems` returns (for example `missing path /session/{}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CompatibilityProblem {
    /// The document was not a JSON object.
    DocumentNotObject,
    /// The `openapi` version field was missing or falsy.
    MissingOpenapiVersion,
    /// `paths` was missing, not an object or empty.
    MissingPaths,
    /// A mandatory path was absent (or its item was not an object).
    MissingPath(&'static str),
    /// A mandatory method was absent from a present path item.
    MissingOperation {
        /// The missing HTTP method.
        method: HttpMethod,
        /// The normalized mandatory path.
        path: &'static str,
    },
    /// A mandatory component schema was absent or not an object.
    MissingSchema(&'static str),
    /// A mandatory property was absent from a component schema.
    MissingSchemaProperty {
        /// The component schema name.
        schema: &'static str,
        /// The missing property name.
        property: &'static str,
    },
    /// `AssistantMessage.time` did not declare the `completed` property.
    MissingAssistantTimeCompleted,
    /// The `prompt_async` operation did not declare a JSON request body schema.
    PromptAsyncMissingBodySchema,
    /// The `prompt_async` body schema did not declare `messageID`.
    PromptAsyncBodyMissingMessageId,
    /// The `prompt_async` body schema did not declare `parts`.
    PromptAsyncBodyMissingParts,
    /// The `prompt_async` body schema did not declare `model` while the project
    /// requires it.
    PromptAsyncBodyMissingModel,
    /// The permission-reply operation did not declare a JSON request body
    /// schema.
    PermissionReplyMissingBodySchema,
    /// The permission-reply body schema did not declare a `reply` field.
    PermissionReplyBodyMissingReply,
    /// The permission-reply `reply` field had no list-valued enum.
    PermissionReplyMissingReplyEnum,
    /// The permission-reply enum did not contain a mandatory reply value.
    PermissionReplyEnumMissingValue(&'static str),
}

impl fmt::Display for CompatibilityProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DocumentNotObject => f.write_str("document is not a JSON object"),
            Self::MissingOpenapiVersion => f.write_str("missing openapi version"),
            Self::MissingPaths => f.write_str("missing paths"),
            Self::MissingPath(path) => write!(f, "missing path {path}"),
            Self::MissingOperation { method, path } => {
                write!(f, "missing operation {method} {path}")
            }
            Self::MissingSchema(name) => write!(f, "missing schema {name}"),
            Self::MissingSchemaProperty { schema, property } => {
                write!(f, "schema {schema} missing property {property}")
            }
            Self::MissingAssistantTimeCompleted => {
                f.write_str("schema AssistantMessage.time missing property completed")
            }
            Self::PromptAsyncMissingBodySchema => {
                f.write_str("prompt_async missing JSON request body schema")
            }
            Self::PromptAsyncBodyMissingMessageId => {
                f.write_str("prompt_async body missing messageID")
            }
            Self::PromptAsyncBodyMissingParts => f.write_str("prompt_async body missing parts"),
            Self::PromptAsyncBodyMissingModel => f.write_str("prompt_async body missing model"),
            Self::PermissionReplyMissingBodySchema => {
                f.write_str("permission reply missing JSON request body schema")
            }
            Self::PermissionReplyBodyMissingReply => {
                f.write_str("permission reply body missing reply")
            }
            Self::PermissionReplyMissingReplyEnum => {
                f.write_str("permission reply missing reply enum")
            }
            Self::PermissionReplyEnumMissingValue(value) => {
                write!(f, "permission reply enum missing {value}")
            }
        }
    }
}

/// The typed result of checking an OpenCode `/doc` document (task 6.3).
///
/// A document is compatible only when [`DocCompatibility::problems`] is empty;
/// every problem is a stable [`CompatibilityProblem`] that never carries
/// document content. The [`fmt::Debug`]/[`fmt::Display`] representations render
/// only the compatible flag and the problem count, never the problems or the
/// document.
#[derive(Clone)]
pub struct DocCompatibility {
    problems: Vec<CompatibilityProblem>,
}

impl DocCompatibility {
    /// Returns `true` when no incompatibility was found.
    #[must_use]
    pub fn is_compatible(&self) -> bool {
        self.problems.is_empty()
    }

    /// Returns the incompatibilities in the reference `openapi_problems` order.
    #[must_use]
    pub fn problems(&self) -> &[CompatibilityProblem] {
        &self.problems
    }
}

impl fmt::Debug for DocCompatibility {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DocCompatibility")
            .field("compatible", &self.is_compatible())
            .field("problem_count", &self.problems.len())
            .finish()
    }
}

impl fmt::Display for DocCompatibility {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.problems.is_empty() {
            f.write_str("OpenCode API document is compatible")
        } else {
            write!(
                f,
                "OpenCode API document has {} compatibility problem(s)",
                self.problems.len()
            )
        }
    }
}

/// A typed `/doc` request failure (task 6.3).
///
/// [`DocError::Transport`] preserves the underlying [`TransportError`] (timeout,
/// unavailable, HTTP 401, other non-success, protocol);
/// [`DocError::Malformed`] means the successful response body was not valid
/// JSON. A valid JSON body that is not an object is not an error: it yields
/// [`CompatibilityProblem::DocumentNotObject`] so the check fails closed without
/// conflating it with a transport problem. Neither [`fmt::Debug`] nor
/// [`fmt::Display`] renders the endpoint, query, credential or response body.
#[derive(Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DocError {
    /// The transport itself failed.
    Transport(TransportError),
    /// The response body was not valid JSON.
    Malformed,
}

impl From<TransportError> for DocError {
    fn from(error: TransportError) -> Self {
        Self::Transport(error)
    }
}

impl fmt::Debug for DocError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(error) => write!(f, "Transport({error:?})"),
            Self::Malformed => f.write_str("Malformed"),
        }
    }
}

impl fmt::Display for DocError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(error) => write!(f, "OpenCode /doc request failed: {error}"),
            Self::Malformed => f.write_str("OpenCode /doc response is not JSON"),
        }
    }
}

impl Error for DocError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Transport(error) => Some(error),
            Self::Malformed => None,
        }
    }
}

/// Returns the incompatibilities of an OpenCode `/doc` document, reproducing
/// the reference `opencode_client.py::openapi_problems` semantics exactly.
///
/// An empty result means the installed API still exposes every path, schema and
/// property the adapter depends on. `require_prompt_model` additionally requires
/// the prompt body schema to declare a `model` property; it must be set only for
/// projects whose config selects `opencode_model`, so projects without the
/// setting stay backward compatible.
///
/// The function is total: it never panics, even on a malformed document. Invalid
/// nested containers that make the reference raise are treated as absent and
/// therefore fail closed with a compatibility problem. Two spellings that
/// normalize to the same path template are resolved first-wins in the document's
/// JSON insertion order (the `preserve_order` feature keeps that order), exactly
/// like the reference `setdefault`, so a conflicting pair can never turn a
/// missing operation into a false compatibility.
///
/// Local `$ref` chains are resolved with cycle protection. An unresolvable
/// reference — a non-string `$ref`, an external reference, a cycle or a missing
/// token — yields an empty unresolved node, so sibling properties can never
/// prove a mandatory structure. This is deliberately not a general-purpose
/// OpenAPI validator.
#[must_use]
pub fn openapi_problems(
    doc: &serde_json::Value,
    require_prompt_model: bool,
) -> Vec<CompatibilityProblem> {
    let mut problems = Vec::new();
    if !doc.is_object() {
        problems.push(CompatibilityProblem::DocumentNotObject);
        return problems;
    }
    if !member(doc, "openapi").is_some_and(is_truthy) {
        problems.push(CompatibilityProblem::MissingOpenapiVersion);
    }

    let paths = match member(doc, "paths").and_then(serde_json::Value::as_object) {
        Some(paths) if !paths.is_empty() => paths,
        _ => {
            problems.push(CompatibilityProblem::MissingPaths);
            return problems;
        }
    };
    // The reference `openapi_problems` normalizes paths with `setdefault`, so
    // the first path spelling in the *original JSON insertion order* wins when
    // two spellings normalize to the same template. `serde_json`'s
    // `preserve_order` feature (enabled in `Cargo.toml`) keeps that order, so a
    // conflict is resolved exactly like the reference instead of by a sorted
    // key that could pick a different spelling and report false compatibility.
    let mut normalized: HashMap<String, &serde_json::Value> = HashMap::new();
    for (path, spec) in paths {
        normalized.entry(normalize_path(path)).or_insert(spec);
    }
    for (required, methods) in REQUIRED_OPERATIONS {
        let Some(spec) = normalized
            .get(*required)
            .and_then(|value| value.as_object())
        else {
            problems.push(CompatibilityProblem::MissingPath(required));
            continue;
        };
        for method in *methods {
            if !spec.contains_key(method.spec_token()) {
                problems.push(CompatibilityProblem::MissingOperation {
                    method: *method,
                    path: required,
                });
            }
        }
    }

    let schemas = member(doc, "components")
        .and_then(|components| member(components, "schemas"))
        .and_then(serde_json::Value::as_object);
    for (name, required_props) in REQUIRED_SCHEMA_PROPERTIES {
        let schema = resolve_ref(doc, schemas.and_then(|map| map.get(*name)));
        let Some(schema_object) = schema.and_then(serde_json::Value::as_object) else {
            problems.push(CompatibilityProblem::MissingSchema(name));
            continue;
        };
        let properties = schema_object
            .get("properties")
            .and_then(serde_json::Value::as_object);
        for property in *required_props {
            if !properties.is_some_and(|map| map.contains_key(*property)) {
                problems.push(CompatibilityProblem::MissingSchemaProperty {
                    schema: name,
                    property,
                });
            }
        }
    }

    if let Some(assistant) = resolve_ref(doc, schemas.and_then(|map| map.get("AssistantMessage")))
        .and_then(serde_json::Value::as_object)
    {
        let time = assistant
            .get("properties")
            .and_then(serde_json::Value::as_object)
            .and_then(|properties| properties.get("time"));
        // A `time` schema proves `completed` only when its `properties` is an
        // object that actually contains the field. A missing `time` schema, a
        // `properties` that is absent, or a `properties` of the wrong JSON type
        // (array, null, string, number) all fail closed, so a single damaged
        // nested field can never be mistaken for a compatible document.
        let time_properties = resolve_ref(doc, time)
            .and_then(serde_json::Value::as_object)
            .and_then(|time_object| time_object.get("properties"))
            .and_then(serde_json::Value::as_object);
        if !time_properties.is_some_and(|properties| properties.contains_key("completed")) {
            problems.push(CompatibilityProblem::MissingAssistantTimeCompleted);
        }
    }

    if let Some(prompt_async) = normalized
        .get(PROMPT_ASYNC_PATH)
        .and_then(|value| value.as_object())
    {
        let body_schema = prompt_async
            .get("post")
            .and_then(serde_json::Value::as_object)
            .and_then(json_request_body_schema);
        match resolve_ref(doc, body_schema).and_then(serde_json::Value::as_object) {
            None => problems.push(CompatibilityProblem::PromptAsyncMissingBodySchema),
            Some(body_object) => {
                let properties = body_object
                    .get("properties")
                    .and_then(serde_json::Value::as_object);
                let has = |name: &str| properties.is_some_and(|map| map.contains_key(name));
                if !has("messageID") {
                    problems.push(CompatibilityProblem::PromptAsyncBodyMissingMessageId);
                }
                if !has("parts") {
                    problems.push(CompatibilityProblem::PromptAsyncBodyMissingParts);
                }
                if require_prompt_model && !has("model") {
                    problems.push(CompatibilityProblem::PromptAsyncBodyMissingModel);
                }
            }
        }
    }

    if let Some(permission_reply) = normalized
        .get(PERMISSION_REPLY_PATH)
        .and_then(|value| value.as_object())
        && let Some(post) = permission_reply.get("post")
    {
        let reply_body = post.as_object().and_then(json_request_body_schema);
        match resolve_ref(doc, reply_body).and_then(serde_json::Value::as_object) {
            None => problems.push(CompatibilityProblem::PermissionReplyMissingBodySchema),
            Some(body_object) => {
                let reply = body_object
                    .get("properties")
                    .and_then(serde_json::Value::as_object)
                    .and_then(|properties| properties.get("reply"));
                match reply.and_then(serde_json::Value::as_object) {
                    None => problems.push(CompatibilityProblem::PermissionReplyBodyMissingReply),
                    Some(reply_object) => match reply_object.get("enum") {
                        Some(enum_value) => match enum_value.as_array() {
                            Some(values) => {
                                for value in PERMISSION_REPLIES {
                                    if !values.iter().any(|item| item.as_str() == Some(value)) {
                                        problems.push(
                                            CompatibilityProblem::PermissionReplyEnumMissingValue(
                                                value,
                                            ),
                                        );
                                    }
                                }
                            }
                            None => {
                                problems.push(CompatibilityProblem::PermissionReplyMissingReplyEnum)
                            }
                        },
                        None => {
                            problems.push(CompatibilityProblem::PermissionReplyMissingReplyEnum)
                        }
                    },
                }
            }
        }
    }

    problems
}

/// Follows the `requestBody.content["application/json"].schema` chain of an
/// OpenAPI operation, treating any wrong nested type as absent.
fn json_request_body_schema(
    operation: &serde_json::Map<String, serde_json::Value>,
) -> Option<&serde_json::Value> {
    operation
        .get("requestBody")
        .and_then(serde_json::Value::as_object)
        .and_then(|body| body.get("content"))
        .and_then(serde_json::Value::as_object)
        .and_then(|content| content.get("application/json"))
        .and_then(serde_json::Value::as_object)
        .and_then(|media| media.get("schema"))
}

/// Returns the value of `key` when `value` is a JSON object.
fn member<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a serde_json::Value> {
    value.as_object().and_then(|map| map.get(key))
}

/// Python truthiness for a JSON value (`if not doc.get("openapi")`).
fn is_truthy(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => false,
        serde_json::Value::Bool(flag) => *flag,
        serde_json::Value::Number(number) => number.as_f64().is_none_or(|value| value != 0.0),
        serde_json::Value::String(text) => !text.is_empty(),
        serde_json::Value::Array(items) => !items.is_empty(),
        serde_json::Value::Object(map) => !map.is_empty(),
    }
}

/// Normalizes an OpenAPI path, replacing every `{parameter}` (with at least one
/// character) by `{}`, exactly like the reference `_PATH_PARAM_RE`.
fn normalize_path(path: &str) -> String {
    let mut normalized = String::with_capacity(path.len());
    let mut chars = path.chars();
    while let Some(ch) = chars.next() {
        if ch != '{' {
            normalized.push(ch);
            continue;
        }
        let mut inner = String::new();
        let mut closed = false;
        for next in chars.by_ref() {
            if next == '}' {
                closed = true;
                break;
            }
            inner.push(next);
        }
        if closed {
            normalized.push_str("{}");
        } else {
            normalized.push('{');
            normalized.push_str(&inner);
        }
    }
    normalized
}

/// Resolves a local `$ref` chain against `doc`, mirroring the reference
/// `_resolve_ref` for valid local chains.
///
/// A `$ref` is followed only when it is a string starting with `#/` and has not
/// been seen before. When the chain is valid the resolved node is returned, so
/// inline schemas and local chains keep proving structure.
///
/// An unresolvable reference — a non-string `$ref`, an external reference, a
/// cycle or a missing token — returns the fail-closed [`EMPTY_OBJECT`] instead
/// of the node that carried the `$ref`. That node's sibling properties (for
/// example a `properties.directory` next to a self-`$ref`) therefore can never
/// prove a mandatory structure, as the task requires. The input `None` stays
/// `None`.
fn resolve_ref<'a>(
    doc: &'a serde_json::Value,
    schema: Option<&'a serde_json::Value>,
) -> Option<&'a serde_json::Value> {
    let mut current = schema?;
    let mut seen: Vec<&str> = Vec::new();
    loop {
        let Some(object) = current.as_object() else {
            return Some(current);
        };
        let Some(reference_value) = object.get("$ref") else {
            return Some(current);
        };
        let Some(reference) = reference_value.as_str() else {
            // A non-string `$ref` cannot be followed; siblings must not prove
            // the structure.
            return Some(&EMPTY_OBJECT);
        };
        if !reference.starts_with("#/") || seen.contains(&reference) {
            // An external or cyclic reference cannot be followed; siblings must
            // not prove the structure.
            return Some(&EMPTY_OBJECT);
        }
        seen.push(reference);
        let mut node: &serde_json::Value = doc;
        for token in reference[2..].split('/') {
            match node.as_object().and_then(|map| map.get(token)) {
                Some(next) => node = next,
                // A missing token is an unresolved reference; the node that
                // carried the `$ref` may have misleading siblings.
                None => return Some(&EMPTY_OBJECT),
            }
        }
        current = node;
    }
}

/// A typed OpenCode adapter bound to one canonical workspace (tasks 6.2–6.4).
///
/// The client owns an [`HttpTransport`] (6.1), the configured canonical
/// workspace path and the project's prompt-model requirement. It exposes the
/// two probes the reference client performs before touching a session
/// ([`OpenCodeClient::health`] and [`OpenCodeClient::verify_workspace`]), the
/// OpenAPI compatibility check ([`OpenCodeClient::check_compatibility`]) and the
/// typed session operations ([`OpenCodeClient::list_sessions`],
/// [`OpenCodeClient::create_session`], [`OpenCodeClient::get_session`]). The
/// message/permission/prompt APIs (6.5+) are intentionally not part of this type
/// yet.
///
/// The [`fmt::Debug`] representation never renders the workspace path.
pub struct OpenCodeClient {
    transport: HttpTransport,
    workspace: PathBuf,
    require_prompt_model: bool,
    prompt_model: Option<OpenCodeModel>,
}

impl OpenCodeClient {
    /// Binds an existing transport to a canonical workspace path.
    ///
    /// The caller is responsible for passing the canonical workspace (as
    /// produced by [`bridge_config::ProjectEntry::workspace`]); a
    /// non-canonical path can only make identity checks fail closed. The
    /// prompt-model requirement defaults to `false` and no prompt `model` is
    /// sent; use [`OpenCodeClient::with_require_prompt_model`] to set the
    /// requirement explicitly or [`OpenCodeClient::from_project`] to inherit the
    /// project's [`bridge_config::OpenCodeModel`].
    #[must_use]
    pub fn new(transport: HttpTransport, workspace: PathBuf) -> Self {
        Self {
            transport,
            workspace,
            require_prompt_model: false,
            prompt_model: None,
        }
    }

    /// Builds a client from a validated project entry.
    ///
    /// The endpoint, credential, canonical workspace, the prompt-model
    /// requirement and the optional prompt model are read from the already
    /// validated [`bridge_config::ProjectEntry`]; configuration validation is
    /// never duplicated. [`OpenCodeClient::require_prompt_model`] is derived
    /// from [`bridge_config::ProjectEntry::opencode_model`] so a project that
    /// selects a model always requires the `model` field without the caller
    /// guessing, and the same validated [`bridge_config::OpenCodeModel`] is
    /// carried into [`OpenCodeClient::send_prompt_async`] so the wire body uses
    /// the configured `providerID`/`modelID`.
    ///
    /// # Errors
    ///
    /// Returns [`TransportError::InvalidAuth`] when the project password file
    /// cannot be read or validated.
    pub fn from_project(project: &ProjectEntry, timeout: Duration) -> Result<Self, TransportError> {
        let endpoint = *project.opencode_endpoint();
        let auth = BasicAuth::from_project(project)?;
        let transport = HttpTransport::new(endpoint, auth, timeout);
        Ok(Self {
            transport,
            workspace: project.workspace().to_path_buf(),
            require_prompt_model: project.opencode_model().is_some(),
            prompt_model: project.opencode_model().cloned(),
        })
    }

    /// Returns the canonical workspace this client is bound to.
    #[must_use]
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// Returns the configured request timeout.
    #[must_use]
    pub const fn timeout(&self) -> Duration {
        self.transport.timeout()
    }

    /// Returns whether the prompt body must declare a `model` property.
    #[must_use]
    pub const fn require_prompt_model(&self) -> bool {
        self.require_prompt_model
    }

    /// Sets whether the prompt body must declare a `model` property.
    ///
    /// This is applied unambiguously by [`OpenCodeClient::check_compatibility`];
    /// a client built with [`OpenCodeClient::from_project`] already derives it
    /// from the project's `opencode_model`.
    #[must_use]
    pub const fn with_require_prompt_model(mut self, require_prompt_model: bool) -> Self {
        self.require_prompt_model = require_prompt_model;
        self
    }

    /// Probes `GET /global/health` scoped with the workspace `directory`.
    ///
    /// # Errors
    ///
    /// Returns [`HealthError::Transport`] for a timeout, unavailable endpoint,
    /// rejected credential or other HTTP failure, and
    /// [`HealthError::Malformed`] when the successful body is not a JSON
    /// object. The response body of an unsuccessful status is never read.
    pub fn health(&self) -> Result<Health, HealthError> {
        let request = HttpRequest::get(HEALTH_PATH)
            .with_query("directory", self.workspace.to_string_lossy().into_owned());
        let response = self.transport.request(&request)?;
        parse_health(response.body())
    }

    /// Proves that the server root (`GET /path` without a `directory` context)
    /// resolves to the configured workspace.
    ///
    /// The unscoped request is essential: the reference documents that a scoped
    /// `/path` echoes the caller's own workspace, so it cannot prove which
    /// project the endpoint actually serves.
    ///
    /// # Errors
    ///
    /// Returns [`IdentityError::Transport`] for a timeout, unavailable
    /// endpoint, rejected credential or other HTTP failure,
    /// [`IdentityError::Malformed`] for a non-object body or a non-string
    /// `directory`, [`IdentityError::MissingDirectory`] when the field is
    /// absent, and [`IdentityError::Mismatch`] when the resolved server root is
    /// not the configured workspace.
    pub fn verify_workspace(&self) -> Result<(), IdentityError> {
        let response = self.transport.request(&HttpRequest::get(PATH_PATH))?;
        let reported = parse_server_directory(response.body())?;
        if reported_matches_workspace(&reported, &self.workspace) {
            Ok(())
        } else {
            Err(IdentityError::Mismatch)
        }
    }

    /// Fetches `GET /doc` scoped with the workspace `directory` and checks it
    /// against the mandatory OpenCode contract.
    ///
    /// The `directory` query is essential and matches the reference `get_doc`
    /// (`scoped=True`): the server resolves paths against the project workspace.
    /// The check applies [`OpenCodeClient::require_prompt_model`] unambiguously,
    /// so a project built with [`OpenCodeClient::from_project`] never has to
    /// guess whether `model` is required.
    ///
    /// # Errors
    ///
    /// Returns [`DocError::Transport`] for a timeout, unavailable endpoint,
    /// rejected credential or other HTTP failure, and [`DocError::Malformed`]
    /// when the successful body is not valid JSON. A valid JSON body that is not
    /// an object is not an error: it yields an incompatible
    /// [`DocCompatibility`] with [`CompatibilityProblem::DocumentNotObject`]. The
    /// response body of an unsuccessful status is never read.
    pub fn check_compatibility(&self) -> Result<DocCompatibility, DocError> {
        let request = HttpRequest::get(DOC_PATH)
            .with_query("directory", self.workspace.to_string_lossy().into_owned());
        let response = self.transport.request(&request)?;
        let value: serde_json::Value =
            serde_json::from_slice(response.body()).map_err(|_| DocError::Malformed)?;
        Ok(DocCompatibility {
            problems: openapi_problems(&value, self.require_prompt_model),
        })
    }

    /// Lists the sessions of the workspace with `GET /session` scoped with the
    /// workspace `directory` (reference `list_sessions`).
    ///
    /// The reference returns the raw JSON array; this method maps every object
    /// element to a typed [`Session`] and skips non-object elements exactly like
    /// the reference `isinstance(session, dict)` filter.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::Transport`] for a timeout, unavailable endpoint,
    /// rejected credential or other HTTP failure, and [`SessionError::Malformed`]
    /// when the successful body is not valid JSON or is not a JSON array. The
    /// response body of an unsuccessful status is never read.
    pub fn list_sessions(&self) -> Result<Vec<Session>, SessionError> {
        let request = HttpRequest::get(SESSION_PATH)
            .with_query("directory", self.workspace.to_string_lossy().into_owned());
        let response = self.transport.request(&request)?;
        parse_session_list(response.body())
    }

    /// Creates a session with `POST /session` scoped with the workspace
    /// `directory` and the compact JSON body `{"title": <title>}` (reference
    /// `create_session`).
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::Transport`] for a timeout, unavailable endpoint,
    /// rejected credential or other HTTP failure, and [`SessionError::Malformed`]
    /// when the successful body is not valid JSON or is not a JSON object. The
    /// response body of an unsuccessful status is never read.
    pub fn create_session(&self, title: &str) -> Result<Session, SessionError> {
        let body = serde_json::json!({ "title": title })
            .to_string()
            .into_bytes();
        let request = HttpRequest::post(SESSION_PATH, body)
            .with_query("directory", self.workspace.to_string_lossy().into_owned());
        let response = self.transport.request(&request)?;
        parse_session(response.body())
    }

    /// Fetches one session with `GET /session/<id>` scoped with the workspace
    /// `directory` (reference `get_session`).
    ///
    /// The id is percent-encoded as a single path segment, so a hostile or
    /// malformed id cannot alter the request target or inject a query. An empty
    /// id, or the bare `.`/`..` dot-segments, is rejected before any request is
    /// sent.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::InvalidSessionId`] when the id is empty or a bare
    /// dot-segment, [`SessionError::Transport`] for a timeout, unavailable
    /// endpoint, rejected credential, HTTP 404 or other HTTP failure, and
    /// [`SessionError::Malformed`] when the successful body is not valid JSON or
    /// is not a JSON object. The response body of an unsuccessful status is
    /// never read.
    pub fn get_session(&self, session_id: &str) -> Result<Session, SessionError> {
        let path = session_path(session_id)?;
        let request = HttpRequest::get(path)
            .with_query("directory", self.workspace.to_string_lossy().into_owned());
        let response = self.transport.request(&request)?;
        parse_session(response.body())
    }

    /// Lists the messages of a session with `GET /session/<id>/message` scoped
    /// with the workspace `directory` (reference `list_messages`).
    ///
    /// The id is percent-encoded as a single path segment exactly like
    /// [`OpenCodeClient::get_session`], and an empty id or a bare `.`/`..`
    /// dot-segment is rejected before any request is sent. The reference
    /// returns the raw JSON array; this method maps every object element to a
    /// typed [`Message`] and fails closed ([`MessageError::Malformed`]) on a
    /// non-object element, a non-object `info`, a non-array `parts`, a
    /// non-object part or a malformed lifecycle/`text` container, so a damaged
    /// structure can never be mistaken for a completed turn or hide a later
    /// unfinished one.
    ///
    /// # Errors
    ///
    /// Returns [`MessageError::InvalidSessionId`] when the id is empty or a bare
    /// dot-segment, [`MessageError::Transport`] for a timeout, unavailable
    /// endpoint, rejected credential, HTTP 404 or other HTTP failure, and
    /// [`MessageError::Malformed`] when the successful body is not valid JSON,
    /// is not a JSON array, or contains a wrongly typed message structure. The
    /// response body of an unsuccessful status is never read.
    pub fn list_messages(&self, session_id: &str) -> Result<Vec<Message>, MessageError> {
        let path = session_messages_path(session_id)?;
        let request = HttpRequest::get(path)
            .with_query("directory", self.workspace.to_string_lossy().into_owned());
        let response = self.transport.request(&request)?;
        parse_message_list(response.body())
    }

    /// Delivers a user prompt asynchronously with
    /// `POST /session/<id>/prompt_async`, scoped with the workspace `directory`
    /// and authenticated through the shared transport (reference
    /// `send_prompt_async`).
    ///
    /// The request body is the compact UTF-8 JSON object httpx serializes
    /// (`ensure_ascii=False`, `separators=(",", ":")`):
    /// `{"messageID": <message_id>, "parts": [{"type": "text", "text": <text>}]}`.
    /// When the client was built from a project that selects an
    /// [`bridge_config::OpenCodeModel`], the body additionally carries
    /// `"model": {"providerID": ..., "modelID": ...}` inserted last, exactly
    /// like the reference `config.opencode_model is not None` branch; a client
    /// without a model sends no `model` field at all. The session id is
    /// percent-encoded as one RFC 3986 path segment by the shared session code,
    /// and an empty id or a bare `.`/`..` dot-segment is rejected as
    /// [`PromptError::InvalidSessionId`] before any request is sent. The
    /// `message_id` and `text` travel inside the JSON body, so any UTF-8 content
    /// is escaped by the serializer rather than interpolated into the path.
    ///
    /// This method only *delivers* the prompt. The reference `_request` helper
    /// returns once the server accepts the request, so `Ok(())` means the
    /// endpoint accepted the POST — never that an assistant turn completed. Any
    /// `2xx` response, including a bodyless `204`, is success. The shared
    /// transport reads and frames the successful response body per HTTP framing
    /// (except a bodyless `204`) before returning, exactly like the reference
    /// `_request` (httpx also reads the body), but this method never interprets
    /// or parses that body as JSON. The reference `_request` accepts any status
    /// `< 400` including `3xx`, while the shared transport keeps its `2xx`-only
    /// policy and reports a `3xx` as [`TransportError::HttpStatus`]; this
    /// deliberate deviation is not weakened for prompt delivery. There is no
    /// automatic retry (the delivery is not idempotent and the reference does
    /// not retry it), and a transport failure after the request was written
    /// leaves the delivery outcome undefined rather than proving nothing was
    /// sent.
    ///
    /// # Errors
    ///
    /// Returns [`PromptError::InvalidSessionId`] when the id is empty or a bare
    /// dot-segment (no request is sent), and [`PromptError::Transport`] for a
    /// timeout, unavailable endpoint, rejected credential, HTTP 404 or any other
    /// non-success status or malformed response. The response body of an
    /// unsuccessful status is never read.
    pub fn send_prompt_async(
        &self,
        session_id: &str,
        message_id: &str,
        text: &str,
    ) -> Result<(), PromptError> {
        let path = session_prompt_path(session_id)?;
        let body = prompt_body(message_id, text, self.prompt_model.as_ref());
        let request = HttpRequest::post(path, body)
            .with_query("directory", self.workspace.to_string_lossy().into_owned());
        self.transport.request(&request)?;
        Ok(())
    }

    /// Lists the pending permissions with `GET /permission` scoped with the
    /// workspace `directory` (reference `list_permissions`).
    ///
    /// The reference returns the raw JSON array of the installed
    /// `PermissionRequest` shape; this method maps every object element to a
    /// typed [`Permission`] and preserves the reference fields the future
    /// permission consumers read (`id`, `sessionID`, `permission`, `patterns`,
    /// `metadata`, `always` and the optional `tool` envelope). Parsing is fail
    /// closed: the required identity (`id`, `sessionID`, `permission`) and
    /// `patterns` must be present with the right JSON type, so a missing,
    /// `null` or wrongly typed required field makes the whole operation
    /// [`PermissionError::Malformed`] rather than a silently skipped entry, and
    /// a malformed response can never look like an empty permission list or an
    /// approval.
    ///
    /// # Errors
    ///
    /// Returns [`PermissionError::Transport`] for a timeout, unavailable
    /// endpoint, rejected credential, HTTP 404 or other HTTP failure, and
    /// [`PermissionError::Malformed`] when the successful body is not valid
    /// JSON, is not a JSON array, or contains a non-object element or a
    /// malformed field. The response body of an unsuccessful status is never
    /// read.
    pub fn list_permissions(&self) -> Result<Vec<Permission>, PermissionError> {
        let request = HttpRequest::get(PERMISSION_PATH)
            .with_query("directory", self.workspace.to_string_lossy().into_owned());
        let response = self.transport.request(&request)?;
        parse_permission_list(response.body())
    }

    /// Answers one permission request with
    /// `POST /permission/<id>/reply`, scoped with the workspace `directory` and
    /// authenticated through the shared transport (reference
    /// `reply_permission`).
    ///
    /// The body is the compact UTF-8 JSON object the reference httpx client
    /// sends: `{"reply": <reply>}`, with `"message"` appended last only when a
    /// `message` is supplied. An absent message (`None`) omits the field
    /// entirely, while an empty message (`Some("")`) sends `"message": ""`;
    /// this preserves the reference `message is not None` distinction. The reply
    /// value is a [`PermissionReply`], so an unsupported reply cannot be sent.
    /// The request id is validated and percent-encoded as one RFC 3986 path
    /// segment by the shared session encoder, and an empty id or a bare
    /// `.`/`..` dot-segment is rejected as [`PermissionError::InvalidRequestId`]
    /// before any request is sent; this is a deliberate fail-closed hardening
    /// over the reference, which interpolates the id into the path verbatim.
    ///
    /// The reference `reply_permission` calls `_request` (not `_json`), so any
    /// `2xx`, including a bodyless `204`, is success and the response body is
    /// read and framed by the transport but never interpreted or parsed as JSON.
    /// The reference `_request` accepts any status `< 400` including `3xx`, while
    /// the shared transport keeps its `2xx`-only policy and reports a `3xx` as
    /// [`TransportError::HttpStatus`]; this deliberate deviation is not weakened
    /// here. There is no automatic retry (the reference does not retry), and a
    /// transport failure after the request was written leaves the reply outcome
    /// undefined rather than proving the reply was not delivered.
    ///
    /// # Errors
    ///
    /// Returns [`PermissionError::InvalidRequestId`] when the id is empty or a
    /// bare dot-segment (no request is sent), and [`PermissionError::Transport`]
    /// for a timeout, unavailable endpoint, rejected credential, HTTP 404 or any
    /// other non-success status or malformed response. The response body of an
    /// unsuccessful status is never read.
    pub fn reply_permission(
        &self,
        request_id: &str,
        reply: PermissionReply,
        message: Option<&str>,
    ) -> Result<(), PermissionError> {
        let path = permission_reply_path(request_id)?;
        let body = permission_reply_body(reply, message);
        let request = HttpRequest::post(path, body)
            .with_query("directory", self.workspace.to_string_lossy().into_owned());
        self.transport.request(&request)?;
        Ok(())
    }

    /// Lists the pending questions with `GET /question` scoped with the
    /// workspace `directory` (reference `list_questions`).
    ///
    /// The reference returns the raw JSON array of the installed
    /// `QuestionRequest` shape; this method maps every object element to a
    /// typed [`Question`] and preserves the real reference fields the future
    /// question consumers read (`id`, `sessionID`, `questions[]` with
    /// `question`/`header`/`options` and the optional `multiple`/`custom`, plus
    /// the optional `tool` envelope). Parsing is fail closed: the required
    /// identity (`id`, `sessionID`) and `questions` must be present with the
    /// right JSON type, and every nested `QuestionInfo`/`QuestionOption`
    /// required field must too, so a missing, `null` or wrongly typed required
    /// field makes the whole operation [`QuestionError::Malformed`] rather than
    /// a silently skipped entry. A damaged response can therefore never look
    /// like an empty question list, and a valid empty `questions: []` stays
    /// valid and distinguishable from the damaged case.
    ///
    /// # Errors
    ///
    /// Returns [`QuestionError::Transport`] for a timeout, unavailable
    /// endpoint, rejected credential, HTTP 404 or other HTTP failure, and
    /// [`QuestionError::Malformed`] when the successful body is not valid JSON,
    /// is not a JSON array, or contains a non-object element or a malformed
    /// field. The response body of an unsuccessful status is never read.
    pub fn list_questions(&self) -> Result<Vec<Question>, QuestionError> {
        let request = HttpRequest::get(QUESTION_PATH)
            .with_query("directory", self.workspace.to_string_lossy().into_owned());
        let response = self.transport.request(&request)?;
        parse_question_list(response.body())
    }
}

impl fmt::Debug for OpenCodeClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenCodeClient")
            .field("transport", &self.transport)
            .field("workspace", &"[redacted]")
            .field("require_prompt_model", &self.require_prompt_model)
            .field(
                "prompt_model",
                &self.prompt_model.as_ref().map(|_| "[redacted]"),
            )
            .finish()
    }
}

/// A typed OpenCode session object (task 6.4).
///
/// The three string fields the reference `opencode_client.py` reads from a
/// session object are exposed through [`Session::id`], [`Session::title`] and
/// [`Session::directory`], each as an `Option`. A field that is absent or not a
/// JSON string is simply `None`, mirroring the reference's permissive
/// `session.get(...)` access; the typed operation never invents an error the
/// reference would not produce.
///
/// The [`fmt::Debug`]/[`fmt::Display`] representations render only whether the
/// id is present, never the id, title, directory or any other session content.
#[derive(Clone, PartialEq, Eq)]
pub struct Session {
    id: Option<String>,
    title: Option<String>,
    directory: Option<String>,
}

impl Session {
    /// Returns the session id, when the server reported a JSON string.
    #[must_use]
    pub fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }

    /// Returns the session title, when the server reported a JSON string.
    #[must_use]
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// Returns the session `directory`, when the server reported a JSON string.
    #[must_use]
    pub fn directory(&self) -> Option<&str> {
        self.directory.as_deref()
    }
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("id", &self.id.as_ref().map(|_| "[redacted]"))
            .field("title", &self.title.as_ref().map(|_| "[redacted]"))
            .field("directory", &self.directory.as_ref().map(|_| "[redacted]"))
            .finish()
    }
}

impl fmt::Display for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "OpenCode session (id present: {})", self.id.is_some())
    }
}

/// A typed session-operation failure (task 6.4).
///
/// [`SessionError::Transport`] preserves the underlying [`TransportError`]
/// (timeout, unavailable, HTTP 401, HTTP 404, other non-success, protocol);
/// [`SessionError::Malformed`] means the successful response body was not valid
/// JSON or had the wrong top-level shape; [`SessionError::InvalidSessionId`]
/// means the caller-supplied id could not be used to form a safe path and no
/// request was sent. Neither [`fmt::Debug`] nor [`fmt::Display`] renders the
/// endpoint, the query, the session id, the credential or the response body.
#[derive(Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SessionError {
    /// The transport itself failed.
    Transport(TransportError),
    /// The response body was not valid JSON or had the wrong top-level shape.
    Malformed,
    /// The session id was empty or a bare dot-segment; no request was sent.
    InvalidSessionId,
}

impl From<TransportError> for SessionError {
    fn from(error: TransportError) -> Self {
        Self::Transport(error)
    }
}

impl fmt::Debug for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(error) => write!(f, "Transport({error:?})"),
            Self::Malformed => f.write_str("Malformed"),
            Self::InvalidSessionId => f.write_str("InvalidSessionId"),
        }
    }
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(error) => write!(f, "OpenCode session request failed: {error}"),
            Self::Malformed => f.write_str("OpenCode session response is malformed"),
            Self::InvalidSessionId => f.write_str("OpenCode session id is not usable"),
        }
    }
}

impl Error for SessionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Transport(error) => Some(error),
            Self::Malformed | Self::InvalidSessionId => None,
        }
    }
}

/// Validates a caller-supplied session id and percent-encodes it as one RFC 3986
/// path segment.
///
/// An empty id and the bare `.`/`..` dot-segments are rejected (`None`), because
/// they would let a server resolve a different route instead of one session.
/// Every other byte outside the unreserved set is `%XX`-encoded, so a hostile id
/// cannot change the request target or inject a query.
fn encode_session_segment(session_id: &str) -> Option<String> {
    if session_id.is_empty() || session_id == "." || session_id == ".." {
        return None;
    }
    Some(encode_path_segment(session_id))
}

/// Builds the scoped `GET`/`POST`-independent session-item path for `session_id`.
///
/// The id is percent-encoded as one RFC 3986 path segment, so only unreserved
/// bytes (`ALPHA`/`DIGIT`/`-`/`.`/`_`/`~`) survive verbatim. An empty id and the
/// bare `.`/`..` dot-segments are rejected fail closed, because they would let a
/// server resolve a different route instead of one session.
fn session_path(session_id: &str) -> Result<String, SessionError> {
    let segment = encode_session_segment(session_id).ok_or(SessionError::InvalidSessionId)?;
    Ok(format!("/session/{segment}"))
}

/// Builds the session message-collection path for `session_id`.
///
/// The id is validated and encoded exactly like [`session_path`], so the message
/// endpoint inherits the same fail-closed id handling.
fn session_messages_path(session_id: &str) -> Result<String, MessageError> {
    let segment = encode_session_segment(session_id).ok_or(MessageError::InvalidSessionId)?;
    Ok(format!("/session/{segment}/message"))
}

/// Builds the async prompt-delivery path for `session_id`.
///
/// The id is validated and encoded exactly like [`session_path`], so the prompt
/// endpoint inherits the same fail-closed id handling and percent-triplet
/// validation.
fn session_prompt_path(session_id: &str) -> Result<String, PromptError> {
    let segment = encode_session_segment(session_id).ok_or(PromptError::InvalidSessionId)?;
    Ok(format!("/session/{segment}/prompt_async"))
}

/// Builds the permission-reply path for `request_id`.
///
/// The request id is validated and percent-encoded as a single RFC 3986 path
/// segment by the same [`encode_session_segment`] helper the session endpoints
/// use, so only unreserved bytes survive verbatim and the shared transport
/// percent-triplet validation still applies. An empty id and the bare `.`/`..`
/// dot-segments are rejected fail closed as
/// [`PermissionError::InvalidRequestId`] before any request is sent, because
/// they would let a server resolve a different route instead of one permission.
/// This is a deliberate fail-closed hardening over the reference, which
/// interpolates the id into the path verbatim.
fn permission_reply_path(request_id: &str) -> Result<String, PermissionError> {
    let segment = encode_session_segment(request_id).ok_or(PermissionError::InvalidRequestId)?;
    Ok(format!("/permission/{segment}/reply"))
}

/// Serializes the compact UTF-8 JSON permission-reply body the reference httpx
/// client sends.
///
/// The object starts with `reply` and appends `message` only when one is
/// supplied, mirroring the reference `body = {"reply": reply}` /
/// `if message is not None: body["message"] = message`. An absent message omits
/// the field, while an empty message still emits `"message": ""`. `serde_json`
/// is built with the `preserve_order` feature, so the insertion order is kept
/// byte-for-byte.
fn permission_reply_body(reply: PermissionReply, message: Option<&str>) -> Vec<u8> {
    let mut body = serde_json::Map::new();
    body.insert(
        "reply".to_string(),
        serde_json::Value::String(reply.as_str().to_string()),
    );
    if let Some(message) = message {
        body.insert(
            "message".to_string(),
            serde_json::Value::String(message.to_string()),
        );
    }
    serde_json::Value::Object(body).to_string().into_bytes()
}

/// Serializes the compact UTF-8 JSON prompt body the reference httpx client
/// sends.
///
/// Field insertion order matches the reference dict (`messageID`, `parts`, then
/// the optional `model`), and `serde_json` is built with the `preserve_order`
/// feature, so the object preserves that order byte-for-byte. The optional model
/// is emitted only when the client carries a validated
/// [`bridge_config::OpenCodeModel`], mirroring the reference
/// `config.opencode_model is not None` branch; otherwise no `model` field is
/// present at all.
fn prompt_body(message_id: &str, text: &str, model: Option<&OpenCodeModel>) -> Vec<u8> {
    let mut body = serde_json::Map::new();
    body.insert(
        "messageID".to_string(),
        serde_json::Value::String(message_id.to_string()),
    );
    body.insert(
        "parts".to_string(),
        serde_json::json!([{ "type": "text", "text": text }]),
    );
    if let Some(model) = model {
        body.insert(
            "model".to_string(),
            serde_json::json!({
                "providerID": model.provider(),
                "modelID": model.model(),
            }),
        );
    }
    serde_json::Value::Object(body).to_string().into_bytes()
}

/// Percent-encodes `segment` as a single RFC 3986 path segment.
///
/// Every byte outside the unreserved set is `%XX`-encoded from its UTF-8 bytes,
/// so `/`, `?`, `#`, whitespace, `%` and non-ASCII bytes can never change the
/// request target or inject a query.
fn encode_path_segment(segment: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(segment.len());
    for &byte in segment.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push('%');
            encoded.push(HEX[usize::from(byte >> 4)] as char);
            encoded.push(HEX[usize::from(byte & 0x0f)] as char);
        }
    }
    encoded
}

/// Parses a session-object body (`create_session`/`get_session`).
fn parse_session(body: &[u8]) -> Result<Session, SessionError> {
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| SessionError::Malformed)?;
    session_from_value(&value).ok_or(SessionError::Malformed)
}

/// Parses a session-list body, skipping non-object elements like the reference
/// `isinstance(session, dict)` filter.
fn parse_session_list(body: &[u8]) -> Result<Vec<Session>, SessionError> {
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| SessionError::Malformed)?;
    let items = value.as_array().ok_or(SessionError::Malformed)?;
    Ok(items.iter().filter_map(session_from_value).collect())
}

/// Maps a JSON value to a [`Session`] when it is an object.
fn session_from_value(value: &serde_json::Value) -> Option<Session> {
    let object = value.as_object()?;
    let string = |name: &str| {
        object
            .get(name)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    Some(Session {
        id: string("id"),
        title: string("title"),
        directory: string("directory"),
    })
}

/// A typed OpenCode message from a session history (task 6.5).
///
/// The reference `list_messages` returns the raw JSON array and the worker
/// consumers read each element as `{"info": {...}, "parts": [...]}`. This type
/// preserves that shape: [`Message::info`] carries the parsed metadata and
/// [`Message::parts`] the parsed content parts. An absent `info`/`parts` keeps
/// the reference default (empty metadata object / empty part list), while a
/// present but wrongly typed one is [`MessageError::Malformed`] rather than a
/// defaulted entry, so a damaged message can never be mistaken for a completed
/// turn or hide a later unfinished one.
///
/// The [`fmt::Debug`]/[`fmt::Display`] representations render only presence and
/// lifecycle flags plus the part count, never the id, text, error or any other
/// message content.
#[derive(Clone)]
pub struct Message {
    info: MessageInfo,
    parts: Vec<MessagePart>,
}

/// Returns `true` for the characters Python `str.strip()` removes at the edges.
///
/// This is Rust [`char::is_whitespace`] (the Unicode `White_Space` property)
/// plus the four C0 separators U+001C..U+001F. Python's `str.isspace()`
/// classifies those separators as whitespace, while `White_Space` omits them,
/// so without this addition a leading/trailing separator would survive the
/// trim and the extracted text would diverge from the reference `_text_of`.
fn is_python_whitespace(character: char) -> bool {
    character.is_whitespace() || matches!(character, '\u{1c}'..='\u{1f}')
}

impl Message {
    /// Returns the parsed message metadata (`info`).
    #[must_use]
    pub const fn info(&self) -> &MessageInfo {
        &self.info
    }

    /// Returns the parsed content parts (`parts`).
    #[must_use]
    pub fn parts(&self) -> &[MessagePart] {
        &self.parts
    }

    /// Returns `true` when the message role is exactly `user`.
    #[must_use]
    pub fn is_user(&self) -> bool {
        self.info.is_user()
    }

    /// Returns `true` when the message role is exactly `assistant`.
    #[must_use]
    pub fn is_assistant(&self) -> bool {
        self.info.is_assistant()
    }

    /// Returns `true` when the assistant turn is completed, i.e. `info.time` is
    /// an object whose `completed` field is present and not null.
    #[must_use]
    pub const fn is_completed(&self) -> bool {
        self.info.is_completed()
    }

    /// Returns `true` when `info.error` is truthy, mirroring the reference
    /// `if info.get("error"):` check.
    #[must_use]
    pub fn has_error(&self) -> bool {
        self.info.has_error()
    }

    /// Joins the visible text parts, reproducing the reference `_text_of`.
    ///
    /// Every part whose `type` is exactly `text` and whose `ignored` flag is
    /// falsy contributes its `text` string; empty strings are dropped, the
    /// survivors are joined with `\n` and the result is trimmed with Python
    /// `str.strip()` whitespace. A `text` value that is present, not a JSON
    /// string and truthy is rejected as [`MessageError::Malformed`] during
    /// parsing (the reference would raise `TypeError` while joining), so a
    /// malformed part never reaches this method.
    #[must_use]
    pub fn text(&self) -> String {
        let mut joined = String::new();
        for part in &self.parts {
            if !part.is_text() || part.ignored {
                continue;
            }
            let Some(text) = part.text.as_deref() else {
                continue;
            };
            if text.is_empty() {
                continue;
            }
            if !joined.is_empty() {
                joined.push('\n');
            }
            joined.push_str(text);
        }
        joined.trim_matches(is_python_whitespace).to_owned()
    }

    /// Returns `true` when an unresolved tool part keeps the turn running,
    /// reproducing the reference `_has_tool_parts`.
    ///
    /// A `tool` part is pending unless it was provider-executed or is an orphaned
    /// interrupted error tool (`state.status == "error"` and
    /// `state.metadata.interrupted is true`), which are already resolved.
    #[must_use]
    pub fn has_pending_tool_parts(&self) -> bool {
        self.parts.iter().any(MessagePart::is_pending_tool)
    }
}

impl fmt::Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Message")
            .field("role_present", &self.info.role.is_some())
            .field("id_present", &self.info.id.is_some())
            .field("completed", &self.info.completed)
            .field("has_error", &self.info.has_error())
            .field("part_count", &self.parts.len())
            .finish()
    }
}

impl fmt::Display for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "OpenCode message (parts: {}, completed: {}, error: {})",
            self.parts.len(),
            self.info.completed,
            self.info.has_error()
        )
    }
}

/// Parsed metadata (`info`) of an OpenCode message (task 6.5).
///
/// Every accessor is permissive like the reference `info.get(...)`: a missing or
/// wrongly typed scalar field is `None`/`false`/zero rather than an error. The
/// type preserves the fields the reference worker consumers read: identity
/// (`id`/`role`/`parentID`/`sessionID`), assistant lifecycle (`time.completed`,
/// `finish`, `error`), provider/model identity and normalized token/cost
/// accounting. Parsing a present `info` that is not an object, or a present
/// truthy `time` that is not an object, is [`MessageError::Malformed`] instead
/// (the reference would raise).
///
/// [`fmt::Debug`]/[`fmt::Display`] render only presence and lifecycle flags,
/// never the id, provider/model, error value or accounting numbers.
#[derive(Clone)]
pub struct MessageInfo {
    id: Option<String>,
    role: Option<String>,
    parent_id: Option<String>,
    session_id: Option<String>,
    provider_id: Option<String>,
    model_id: Option<String>,
    usage: Usage,
    completed: bool,
    finish: Option<String>,
    error: Option<serde_json::Value>,
}

impl MessageInfo {
    /// Returns the message id, when the server reported a JSON string.
    #[must_use]
    pub fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }

    /// Returns the message role, when the server reported a JSON string.
    #[must_use]
    pub fn role(&self) -> Option<&str> {
        self.role.as_deref()
    }

    /// Returns the parent message id, when the server reported a JSON string.
    #[must_use]
    pub fn parent_id(&self) -> Option<&str> {
        self.parent_id.as_deref()
    }

    /// Returns the session id, when the server reported a JSON string.
    #[must_use]
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// Returns the provider id, when the server reported a JSON string.
    #[must_use]
    pub fn provider_id(&self) -> Option<&str> {
        self.provider_id.as_deref()
    }

    /// Returns the model id, when the server reported a JSON string.
    #[must_use]
    pub fn model_id(&self) -> Option<&str> {
        self.model_id.as_deref()
    }

    /// Returns the `(providerID, modelID)` pair when both are non-empty strings,
    /// reproducing the reference `usage.normalize_model`.
    #[must_use]
    pub fn model(&self) -> Option<(&str, &str)> {
        let provider = self.provider_id.as_deref()?;
        let model = self.model_id.as_deref()?;
        if provider.is_empty() || model.is_empty() {
            return None;
        }
        Some((provider, model))
    }

    /// Returns the normalized token/cost accounting of this message.
    #[must_use]
    pub const fn usage(&self) -> &Usage {
        &self.usage
    }

    /// Returns `true` when the role is exactly `user`.
    #[must_use]
    pub fn is_user(&self) -> bool {
        self.role.as_deref() == Some("user")
    }

    /// Returns `true` when the role is exactly `assistant`.
    #[must_use]
    pub fn is_assistant(&self) -> bool {
        self.role.as_deref() == Some("assistant")
    }

    /// Returns `true` when `time.completed` is present and not null, exactly like
    /// the reference `time_info.get("completed") is None` negation. A missing or
    /// non-object `time` is not completed.
    #[must_use]
    pub const fn is_completed(&self) -> bool {
        self.completed
    }

    /// Returns the `finish` value, when the server reported a JSON string.
    #[must_use]
    pub fn finish(&self) -> Option<&str> {
        self.finish.as_deref()
    }

    /// Returns `true` when the error field is truthy.
    #[must_use]
    pub fn has_error(&self) -> bool {
        self.error.is_some()
    }

    /// Returns the raw truthy error value, when the server reported one.
    ///
    /// The value is preserved verbatim (it may be a string, object or array) so a
    /// future worker consumer can render it exactly as the reference
    /// `str(error)[:300]` does. It is never rendered by this crate's
    /// [`fmt::Debug`]/[`fmt::Display`].
    #[must_use]
    pub fn error(&self) -> Option<&serde_json::Value> {
        self.error.as_ref()
    }
}

impl fmt::Debug for MessageInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MessageInfo")
            .field("id_present", &self.id.is_some())
            .field("role_present", &self.role.is_some())
            .field("parent_id_present", &self.parent_id.is_some())
            .field("completed", &self.completed)
            .field("finish_present", &self.finish.is_some())
            .field("has_error", &self.has_error())
            .finish()
    }
}

impl fmt::Display for MessageInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "OpenCode message info (completed: {}, error: {})",
            self.completed,
            self.has_error()
        )
    }
}

/// Normalized token and cost accounting of one message (task 6.5).
///
/// The fields mirror the reference `usage.py` accounting and its `_number`
/// normalization: only JSON numbers are accepted, while a missing, boolean,
/// negative or non-finite value becomes `0.0`. This preserves the reference
/// semantics for a future worker aggregation without implementing it here.
///
/// [`fmt::Debug`]/[`fmt::Display`] render the numbers, which are accounting
/// totals rather than message content.
#[derive(Clone, Copy, PartialEq)]
pub struct Usage {
    input: f64,
    output: f64,
    reasoning: f64,
    cache_read: f64,
    cache_write: f64,
    cost: f64,
}

impl Usage {
    /// Returns an all-zero usage object.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            input: 0.0,
            output: 0.0,
            reasoning: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
            cost: 0.0,
        }
    }

    /// Returns the normalized `input` token count.
    #[must_use]
    pub const fn input(&self) -> f64 {
        self.input
    }

    /// Returns the normalized `output` token count.
    #[must_use]
    pub const fn output(&self) -> f64 {
        self.output
    }

    /// Returns the normalized `reasoning` token count.
    #[must_use]
    pub const fn reasoning(&self) -> f64 {
        self.reasoning
    }

    /// Returns the normalized cache `read` token count.
    #[must_use]
    pub const fn cache_read(&self) -> f64 {
        self.cache_read
    }

    /// Returns the normalized cache `write` token count.
    #[must_use]
    pub const fn cache_write(&self) -> f64 {
        self.cache_write
    }

    /// Returns the normalized `cost`.
    #[must_use]
    pub const fn cost(&self) -> f64 {
        self.cost
    }
}

impl Default for Usage {
    fn default() -> Self {
        Self::empty()
    }
}

impl fmt::Debug for Usage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Usage")
            .field("input", &self.input)
            .field("output", &self.output)
            .field("reasoning", &self.reasoning)
            .field("cache_read", &self.cache_read)
            .field("cache_write", &self.cache_write)
            .field("cost", &self.cost)
            .finish()
    }
}

impl fmt::Display for Usage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "OpenCode usage (input: {}, output: {}, reasoning: {}, cache read: {}, cache write: {}, cost: {})",
            self.input, self.output, self.reasoning, self.cache_read, self.cache_write, self.cost
        )
    }
}

/// Parsed content part (`parts[]`) of an OpenCode message (task 6.5).
///
/// The type preserves the fields the reference consumers read from a part:
/// `type`, `text` and `ignored` for text content, and `tool`, `state.status`,
/// `state.error`, `metadata.providerExecuted` and `state.metadata.interrupted`
/// for tool lifecycle. A wrongly typed scalar field becomes `None`/`false`,
/// while a present truthy lifecycle container or `text` value of the wrong type
/// is rejected during parsing as [`MessageError::Malformed`], so a malformed
/// part cannot panic the parser or look like a resolved tool.
///
/// [`fmt::Debug`]/[`fmt::Display`] render only kind and lifecycle flags, never
/// the text, tool name or error value.
#[derive(Clone)]
pub struct MessagePart {
    kind: Option<String>,
    text: Option<String>,
    ignored: bool,
    tool: Option<String>,
    tool_status: Option<String>,
    tool_error: Option<serde_json::Value>,
    provider_executed: bool,
    interrupted: bool,
}

impl MessagePart {
    /// Returns the part `type`, when the server reported a JSON string.
    #[must_use]
    pub fn kind(&self) -> Option<&str> {
        self.kind.as_deref()
    }

    /// Returns `true` when the part `type` is exactly `text`.
    #[must_use]
    pub fn is_text(&self) -> bool {
        self.kind.as_deref() == Some("text")
    }

    /// Returns `true` when the part `type` is exactly `tool`.
    #[must_use]
    pub fn is_tool(&self) -> bool {
        self.kind.as_deref() == Some("tool")
    }

    /// Returns the text content, when the server reported a JSON string.
    #[must_use]
    pub fn text(&self) -> Option<&str> {
        self.text.as_deref()
    }

    /// Returns `true` when the part `ignored` flag is truthy, mirroring the
    /// reference `not part.get("ignored")` check.
    #[must_use]
    pub const fn ignored(&self) -> bool {
        self.ignored
    }

    /// Returns the tool name, when the server reported a JSON string.
    #[must_use]
    pub fn tool_name(&self) -> Option<&str> {
        self.tool.as_deref()
    }

    /// Returns the tool state status, when the server reported a JSON string.
    #[must_use]
    pub fn tool_status(&self) -> Option<&str> {
        self.tool_status.as_deref()
    }

    /// Returns the raw tool state error value, when the server reported one.
    ///
    /// The value is preserved verbatim so a future worker consumer can render it
    /// exactly as the reference `str(state.get("error"))` does. It is never
    /// rendered by this crate's [`fmt::Debug`]/[`fmt::Display`].
    #[must_use]
    pub fn tool_error(&self) -> Option<&serde_json::Value> {
        self.tool_error.as_ref()
    }

    /// Returns `true` when the tool was provider-executed
    /// (`metadata.providerExecuted` is truthy).
    #[must_use]
    pub const fn provider_executed(&self) -> bool {
        self.provider_executed
    }

    /// Returns `true` when the tool is an interrupted error
    /// (`state.metadata.interrupted is true`).
    #[must_use]
    pub const fn interrupted(&self) -> bool {
        self.interrupted
    }

    /// Returns `true` when this tool part still keeps the turn running,
    /// mirroring the reference `_has_tool_parts` per-part check.
    fn is_pending_tool(&self) -> bool {
        if !self.is_tool() {
            return false;
        }
        if self.provider_executed {
            return false;
        }
        if self.tool_status.as_deref() == Some("error") && self.interrupted {
            return false;
        }
        true
    }
}

impl fmt::Debug for MessagePart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MessagePart")
            .field("kind_present", &self.kind.is_some())
            .field("is_text", &self.is_text())
            .field("is_tool", &self.is_tool())
            .field("ignored", &self.ignored)
            .field("provider_executed", &self.provider_executed)
            .field("interrupted", &self.interrupted)
            .finish()
    }
}

impl fmt::Display for MessagePart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "OpenCode message part (text: {}, tool: {})",
            self.is_text(),
            self.is_tool()
        )
    }
}

/// A typed session-message operation failure (task 6.5).
///
/// [`MessageError::Transport`] preserves the underlying [`TransportError`]
/// (timeout, unavailable, HTTP 401, HTTP 404, other non-success, protocol);
/// [`MessageError::Malformed`] means the successful response body was not valid
/// JSON, was not a JSON array, or contained a wrongly typed message/`parts`
/// element, lifecycle container or `text` value;
/// [`MessageError::InvalidSessionId`] means the
/// caller-supplied id could not be used to form a safe path and no request was
/// sent. Neither [`fmt::Debug`] nor [`fmt::Display`] renders the endpoint, the
/// query, the session id, the credential or the response body.
#[derive(Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MessageError {
    /// The transport itself failed.
    Transport(TransportError),
    /// The response body was not valid JSON, not a JSON array, or contained a
    /// wrongly typed message/`parts` element, lifecycle container or `text`.
    Malformed,
    /// The session id was empty or a bare dot-segment; no request was sent.
    InvalidSessionId,
}

impl From<TransportError> for MessageError {
    fn from(error: TransportError) -> Self {
        Self::Transport(error)
    }
}

impl fmt::Debug for MessageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(error) => write!(f, "Transport({error:?})"),
            Self::Malformed => f.write_str("Malformed"),
            Self::InvalidSessionId => f.write_str("InvalidSessionId"),
        }
    }
}

impl fmt::Display for MessageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(error) => write!(f, "OpenCode message request failed: {error}"),
            Self::Malformed => f.write_str("OpenCode message response is malformed"),
            Self::InvalidSessionId => f.write_str("OpenCode session id is not usable"),
        }
    }
}

impl Error for MessageError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Transport(error) => Some(error),
            Self::Malformed | Self::InvalidSessionId => None,
        }
    }
}

/// A typed async prompt-delivery failure (task 6.6).
///
/// [`PromptError::Transport`] preserves the underlying [`TransportError`]
/// (timeout, unavailable, HTTP 401, HTTP 404, other non-success, protocol);
/// [`PromptError::InvalidSessionId`] means the caller-supplied id could not be
/// used to form a safe path and no request was sent. There is no `Malformed`
/// variant: the reference `send_prompt_async` calls `_request` (not `_json`), so
/// the successful response body is read and framed by the transport but never
/// interpreted or parsed as JSON, and any `2xx`, including a bodyless `204`, is
/// success. The reference `_request` accepts any status `< 400`, including
/// `3xx`, while the shared transport keeps its `2xx`-only policy, so a `3xx`
/// stays [`TransportError::HttpStatus`]; this is a deliberate deviation.
///
/// A [`PromptError::Transport`] raised after the request was written (for
/// example a timeout or a `5xx` response) does **not** prove the prompt was not
/// delivered: the delivery outcome is undefined, exactly like the reference
/// `OpenCodeUnavailable` contract. Neither [`fmt::Debug`] nor [`fmt::Display`]
/// renders the endpoint, the query, the session id, the message id, the text,
/// the model, the credential or the response body.
#[derive(Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PromptError {
    /// The transport itself failed; delivery outcome is undefined.
    Transport(TransportError),
    /// The session id was empty or a bare dot-segment; no request was sent.
    InvalidSessionId,
}

impl From<TransportError> for PromptError {
    fn from(error: TransportError) -> Self {
        Self::Transport(error)
    }
}

impl fmt::Debug for PromptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(error) => write!(f, "Transport({error:?})"),
            Self::InvalidSessionId => f.write_str("InvalidSessionId"),
        }
    }
}

impl fmt::Display for PromptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(error) => write!(f, "OpenCode prompt delivery failed: {error}"),
            Self::InvalidSessionId => f.write_str("OpenCode session id is not usable"),
        }
    }
}

impl Error for PromptError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Transport(error) => Some(error),
            Self::InvalidSessionId => None,
        }
    }
}

/// The reply values the installed v1 permission operation accepts.
///
/// The reference `reply_permission` validates its `reply` argument against
/// `("once", "always", "reject")` and raises for anything else; a typed enum
/// makes an unsupported reply unrepresentable at compile time instead of a
/// runtime failure. [`PermissionReply::as_str`] renders the exact wire token.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PermissionReply {
    /// Answer only this request (`"once"`).
    Once,
    /// Answer this and future matching requests (`"always"`).
    Always,
    /// Reject the request (`"reject"`).
    Reject,
}

impl PermissionReply {
    /// Returns the exact wire token for this reply.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Once => "once",
            Self::Always => "always",
            Self::Reject => "reject",
        }
    }
}

impl fmt::Display for PermissionReply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The optional `tool` envelope of a permission request (task 6.7).
///
/// OpenCode's `PermissionRequest` carries an optional
/// `tool: {messageID, callID}` pair that future permission consumers may need
/// to correlate a request with the assistant tool call that raised it. Both
/// fields are exposed as `Option` and an absent or `null` envelope is `None`.
/// The [`fmt::Debug`]/[`fmt::Display`] representations render only presence
/// flags, never the message or call ids.
#[derive(Clone, PartialEq, Eq)]
pub struct PermissionTool {
    message_id: Option<String>,
    call_id: Option<String>,
}

impl PermissionTool {
    /// Returns the tool `messageID`, when the server reported a JSON string.
    #[must_use]
    pub fn message_id(&self) -> Option<&str> {
        self.message_id.as_deref()
    }

    /// Returns the tool `callID`, when the server reported a JSON string.
    #[must_use]
    pub fn call_id(&self) -> Option<&str> {
        self.call_id.as_deref()
    }
}

impl fmt::Debug for PermissionTool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PermissionTool")
            .field(
                "message_id",
                &self.message_id.as_ref().map(|_| "[redacted]"),
            )
            .field("call_id", &self.call_id.as_ref().map(|_| "[redacted]"))
            .finish()
    }
}

impl fmt::Display for PermissionTool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "OpenCode permission tool (message present: {}, call present: {})",
            self.message_id.is_some(),
            self.call_id.is_some()
        )
    }
}

/// A typed OpenCode permission request (task 6.7).
///
/// The reference `list_permissions` returns the raw JSON array of the installed
/// `/permission` operation, whose `PermissionRequest` shape is
/// `{id, sessionID, permission, patterns, metadata, always, tool?}` (OpenCode
/// SDK). This type preserves the fields the reference consumers read
/// (`worker.py::_permission_decision`/`_pending_permissions`,
/// `mcp_server.py::_blockers_present`) plus the optional `tool` envelope:
/// [`Permission::id`], [`Permission::session_id`],
/// [`Permission::permission`], [`Permission::patterns`],
/// [`Permission::always`], [`Permission::tool`] and the raw
/// [`Permission::metadata`].
///
/// Parsing distinguishes the *required* SDK fields from *absent optional
/// defaults* and from *malformed* values. The SDK `PermissionRequest` marks
/// `id`, `sessionID`, `permission` and `patterns` as required: a missing,
/// `null` or wrongly typed identity or `patterns` is
/// [`PermissionError::Malformed`], never `None`/an empty list, so a damaged
/// pending request cannot be mistaken for an absent one (for example, silently
/// dropped by a future `sessionID` filter) or be approved. A genuine empty
/// `patterns` array (`[]`) stays valid and distinguishable from the damaged
/// case. The remaining fields keep the reference defaults: an absent or `null`
/// `always` is empty, an absent or `null` `metadata` is an empty object, and an
/// absent or `null` `tool` is `None`; any other wrongly typed value is
/// [`PermissionError::Malformed`]. A non-object element or a non-array top-level
/// body is [`PermissionError::Malformed`] too, so a damaged request is never
/// silently skipped and can never make the list look empty or approved.
///
/// The [`fmt::Debug`]/[`fmt::Display`] representations render only presence
/// flags and counts, never the id, session id, permission name, patterns,
/// commands, metadata, tool ids or any other content. The raw `metadata` JSON is
/// available only through the explicit [`Permission::metadata`] accessor and is
/// redacted in both renderings.
#[derive(Clone)]
pub struct Permission {
    id: Option<String>,
    session_id: Option<String>,
    permission: Option<String>,
    patterns: Vec<String>,
    metadata: serde_json::Value,
    always: Vec<String>,
    tool: Option<PermissionTool>,
}

impl Permission {
    /// Returns the request id.
    ///
    /// Parsing rejects a missing, `null` or wrongly typed `id` as
    /// [`PermissionError::Malformed`], so every successfully parsed request
    /// reports `Some` here.
    #[must_use]
    pub fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }

    /// Returns the `sessionID`.
    ///
    /// Parsing rejects a missing, `null` or wrongly typed `sessionID` as
    /// [`PermissionError::Malformed`], so a damaged request can never be
    /// silently dropped by a session filter as if it were absent.
    #[must_use]
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// Returns the permission name.
    ///
    /// Parsing rejects a missing, `null` or wrongly typed `permission` as
    /// [`PermissionError::Malformed`].
    #[must_use]
    pub fn permission(&self) -> Option<&str> {
        self.permission.as_deref()
    }

    /// Returns the requested patterns.
    ///
    /// `patterns` is required by the SDK shape, so parsing rejects an absent or
    /// `null` value as [`PermissionError::Malformed`]; a valid empty `[]` is
    /// preserved and stays distinguishable from the damaged case.
    #[must_use]
    pub fn patterns(&self) -> &[String] {
        &self.patterns
    }

    /// Returns the raw `metadata` JSON object (empty when absent or `null`).
    ///
    /// This is the explicit raw-JSON accessor; the value is redacted in
    /// [`fmt::Debug`]/[`fmt::Display`] and never rendered implicitly.
    #[must_use]
    pub const fn metadata(&self) -> &serde_json::Value {
        &self.metadata
    }

    /// Returns the durable `always` patterns (empty when absent or `null`).
    #[must_use]
    pub fn always(&self) -> &[String] {
        &self.always
    }

    /// Returns the optional tool envelope.
    #[must_use]
    pub const fn tool(&self) -> Option<&PermissionTool> {
        self.tool.as_ref()
    }

    /// Returns `true` when this request's `sessionID` is exactly `session_id`.
    ///
    /// This is the reference `worker.py::_pending_permissions` /
    /// `mcp_server.py::_blockers_present` session filter. Parsing guarantees
    /// `sessionID` is a present string, so a damaged request can never match a
    /// session filter (nor be silently dropped as if it were absent).
    #[must_use]
    pub fn belongs_to_session(&self, session_id: &str) -> bool {
        self.session_id.as_deref() == Some(session_id)
    }
}

impl fmt::Debug for Permission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Permission")
            .field("id", &self.id.as_ref().map(|_| "[redacted]"))
            .field(
                "session_id",
                &self.session_id.as_ref().map(|_| "[redacted]"),
            )
            .field(
                "permission",
                &self.permission.as_ref().map(|_| "[redacted]"),
            )
            .field("patterns", &self.patterns.len())
            .field("metadata", &"[redacted]")
            .field("always", &self.always.len())
            .field("tool", &self.tool)
            .finish()
    }
}

impl fmt::Display for Permission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "OpenCode permission request (id present: {}, patterns: {}, tool present: {})",
            self.id.is_some(),
            self.patterns.len(),
            self.tool.is_some()
        )
    }
}

/// A typed permission-operation failure (task 6.7).
///
/// [`PermissionError::Transport`] preserves the underlying [`TransportError`]
/// (timeout, unavailable, HTTP 401, HTTP 404, other non-success, protocol);
/// [`PermissionError::Malformed`] means the successful `GET /permission` body
/// was not valid JSON, was not a JSON array, or carried a malformed element;
/// [`PermissionError::InvalidRequestId`] means the caller-supplied reply request
/// id was empty or a bare dot-segment and no request was sent. The reply
/// operation has no `Malformed` variant because the reference
/// `reply_permission` calls `_request` (not `_json`), so its successful response
/// body is framed but never interpreted as JSON. Neither [`fmt::Debug`] nor
/// [`fmt::Display`] renders the endpoint, the query, the request/session id,
/// the patterns, the metadata, the message, the credential or the response body.
#[derive(Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PermissionError {
    /// The transport itself failed; the reply outcome may be undefined.
    Transport(TransportError),
    /// The permission-list response body was not a valid JSON array of objects.
    Malformed,
    /// The reply request id was empty or a bare dot-segment; no request was sent.
    InvalidRequestId,
}

impl From<TransportError> for PermissionError {
    fn from(error: TransportError) -> Self {
        Self::Transport(error)
    }
}

impl fmt::Debug for PermissionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(error) => write!(f, "Transport({error:?})"),
            Self::Malformed => f.write_str("Malformed"),
            Self::InvalidRequestId => f.write_str("InvalidRequestId"),
        }
    }
}

impl fmt::Display for PermissionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(error) => write!(f, "OpenCode permission request failed: {error}"),
            Self::Malformed => f.write_str("OpenCode permission response is malformed"),
            Self::InvalidRequestId => f.write_str("OpenCode permission request id is not usable"),
        }
    }
}

impl Error for PermissionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Transport(error) => Some(error),
            Self::Malformed | Self::InvalidRequestId => None,
        }
    }
}

/// Parses a permission-list body (`list_permissions`).
///
/// The top level must be a JSON array; anything else is
/// [`PermissionError::Malformed`]. Every element must be an object and is parsed
/// fail-closed, so a non-object element or a malformed field is
/// [`PermissionError::Malformed`] rather than a silently skipped entry: a
/// damaged pending request must not disappear from the list or look approved.
fn parse_permission_list(body: &[u8]) -> Result<Vec<Permission>, PermissionError> {
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| PermissionError::Malformed)?;
    let items = value.as_array().ok_or(PermissionError::Malformed)?;
    items.iter().map(permission_from_value).collect()
}

/// Maps one JSON value to a [`Permission`], failing closed on a non-object.
fn permission_from_value(value: &serde_json::Value) -> Result<Permission, PermissionError> {
    let object = value.as_object().ok_or(PermissionError::Malformed)?;
    // The SDK `PermissionRequest` marks `id`, `sessionID` and `permission` as
    // required strings. A missing, `null` or wrongly typed identity is
    // [`PermissionError::Malformed`] rather than `None`, so a damaged pending
    // request can never be mistaken for an absent one (for example, silently
    // dropped by a future `sessionID` filter) or be approved.
    let required_string = |name: &str| match object.get(name) {
        Some(serde_json::Value::String(text)) => Ok(text.clone()),
        _ => Err(PermissionError::Malformed),
    };
    // `patterns` is a required array of strings in the SDK shape. An absent or
    // `null` value is malformed and must not collapse to an empty vector: the
    // reference `worker.py::_permission_decision` rejects it as
    // `malformed_patterns`, while a genuine `[]` is a valid ordinary permission
    // that stays distinguishable from the damaged case.
    let patterns = match object.get("patterns") {
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .ok_or(PermissionError::Malformed)
            })
            .collect::<Result<Vec<String>, PermissionError>>()?,
        _ => return Err(PermissionError::Malformed),
    };
    // `always` is not read by the permission decision and keeps the reference
    // permissive default: an absent or `null` value is an empty list. `metadata`
    // is read only by the external-directory branch, where the reference
    // tolerates an absent/`null` value as "no directories"; `tool` is the one
    // optional SDK field.
    let optional_list = |name: &str| match object.get(name) {
        None | Some(serde_json::Value::Null) => Ok(Vec::new()),
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .ok_or(PermissionError::Malformed)
            })
            .collect::<Result<Vec<String>, PermissionError>>(),
        Some(_) => Err(PermissionError::Malformed),
    };
    let metadata = match object.get("metadata") {
        None | Some(serde_json::Value::Null) => serde_json::Value::Object(serde_json::Map::new()),
        Some(value @ serde_json::Value::Object(_)) => value.clone(),
        Some(_) => return Err(PermissionError::Malformed),
    };
    let tool = match object.get("tool") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::Object(tool)) => Some(PermissionTool {
            message_id: tool_string(tool, "messageID", PermissionError::Malformed)?,
            call_id: tool_string(tool, "callID", PermissionError::Malformed)?,
        }),
        Some(_) => return Err(PermissionError::Malformed),
    };
    Ok(Permission {
        id: Some(required_string("id")?),
        session_id: Some(required_string("sessionID")?),
        permission: Some(required_string("permission")?),
        patterns,
        metadata,
        always: optional_list("always")?,
        tool,
    })
}

/// Reads an optional string member of a `{messageID, callID}` tool envelope,
/// failing closed on a wrongly typed value with the caller's error variant.
///
/// Both the permission and question `tool` envelopes share this exact shape, so
/// the helper is generic over the malformed variant ([`PermissionError`] or
/// [`QuestionError`]) instead of duplicating the fail-closed mapping.
fn tool_string<E>(
    tool: &serde_json::Map<String, serde_json::Value>,
    name: &str,
    malformed: E,
) -> Result<Option<String>, E> {
    match tool.get(name) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(malformed),
    }
}

/// The optional `tool` envelope of a question request (task 6.8).
///
/// OpenCode's `QuestionRequest` carries the same optional
/// `tool: {messageID, callID}` pair as `PermissionRequest`, used to correlate a
/// question with the assistant tool call that raised it. Both fields are
/// exposed as `Option` and an absent or `null` envelope is `None`. The
/// [`fmt::Debug`]/[`fmt::Display`] representations render only presence flags,
/// never the message or call ids.
#[derive(Clone, PartialEq, Eq)]
pub struct QuestionTool {
    message_id: Option<String>,
    call_id: Option<String>,
}

impl QuestionTool {
    /// Returns the tool `messageID`, when the server reported a JSON string.
    #[must_use]
    pub fn message_id(&self) -> Option<&str> {
        self.message_id.as_deref()
    }

    /// Returns the tool `callID`, when the server reported a JSON string.
    #[must_use]
    pub fn call_id(&self) -> Option<&str> {
        self.call_id.as_deref()
    }
}

impl fmt::Debug for QuestionTool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QuestionTool")
            .field(
                "message_id",
                &self.message_id.as_ref().map(|_| "[redacted]"),
            )
            .field("call_id", &self.call_id.as_ref().map(|_| "[redacted]"))
            .finish()
    }
}

impl fmt::Display for QuestionTool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "OpenCode question tool (message present: {}, call present: {})",
            self.message_id.is_some(),
            self.call_id.is_some()
        )
    }
}

/// One selectable answer of a [`QuestionInfo`] (task 6.8, OpenCode SDK
/// `QuestionOption`).
///
/// Both `label` and `description` are required strings in the SDK shape, so
/// parsing rejects a missing, `null` or wrongly typed value as
/// [`QuestionError::Malformed`]. The [`fmt::Debug`]/[`fmt::Display`]
/// representations render only presence flags, never the label or description.
#[derive(Clone, PartialEq, Eq)]
pub struct QuestionOption {
    label: String,
    description: String,
}

impl QuestionOption {
    /// Returns the option label.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Returns the option description.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }
}

impl fmt::Debug for QuestionOption {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QuestionOption")
            .field("label", &"[redacted]")
            .field("description", &"[redacted]")
            .finish()
    }
}

impl fmt::Display for QuestionOption {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OpenCode question option")
    }
}

/// One question inside a [`Question`] request (task 6.8, OpenCode SDK
/// `QuestionInfo`).
///
/// The SDK marks `question`, `header` and `options` required, so parsing
/// rejects a missing, `null` or wrongly typed value as
/// [`QuestionError::Malformed`]; a valid empty `options: []` stays valid and
/// distinguishable from the damaged case. The optional `multiple`/`custom`
/// flags keep the reference default of `None` when absent or `null`, and any
/// other wrongly typed value is malformed. The [`fmt::Debug`]/[`fmt::Display`]
/// representations render only presence flags and the option count, never the
/// question, header or option content.
#[derive(Clone, PartialEq, Eq)]
pub struct QuestionInfo {
    question: String,
    header: String,
    options: Vec<QuestionOption>,
    multiple: Option<bool>,
    custom: Option<bool>,
}

impl QuestionInfo {
    /// Returns the complete question text.
    #[must_use]
    pub fn question(&self) -> &str {
        &self.question
    }

    /// Returns the short header label.
    #[must_use]
    pub fn header(&self) -> &str {
        &self.header
    }

    /// Returns the available answer options (empty when the server sent `[]`).
    #[must_use]
    pub fn options(&self) -> &[QuestionOption] {
        &self.options
    }

    /// Returns the optional `multiple` flag.
    #[must_use]
    pub const fn multiple(&self) -> Option<bool> {
        self.multiple
    }

    /// Returns the optional `custom` flag.
    #[must_use]
    pub const fn custom(&self) -> Option<bool> {
        self.custom
    }
}

impl fmt::Debug for QuestionInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QuestionInfo")
            .field("question", &"[redacted]")
            .field("header", &"[redacted]")
            .field("options", &self.options.len())
            .field("multiple", &self.multiple)
            .field("custom", &self.custom)
            .finish()
    }
}

impl fmt::Display for QuestionInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "OpenCode question (options: {}, multiple: {:?}, custom: {:?})",
            self.options.len(),
            self.multiple,
            self.custom
        )
    }
}

/// A typed OpenCode question request (task 6.8).
///
/// The reference `list_questions` returns the raw JSON array of the installed
/// `/question` operation, whose `QuestionRequest` shape is
/// `{id, sessionID, questions, tool?}` (OpenCode SDK). This type preserves the
/// fields the reference consumers read (`worker.py::_pending_questions`,
/// `mcp_server.py::_blockers_present`): [`Question::id`],
/// [`Question::session_id`], the nested [`Question::questions`] and the
/// optional [`Question::tool`].
///
/// Parsing distinguishes the required SDK fields from absent optional defaults
/// and from malformed values. `id`, `sessionID` and `questions` are required:
/// a missing, `null` or wrongly typed identity is [`QuestionError::Malformed`],
/// never `None`/an empty list, so a damaged pending question cannot be mistaken
/// for an absent one (for example, silently dropped by a future `sessionID`
/// filter). Every nested [`QuestionInfo`] and [`QuestionOption`] required field
/// is checked the same way. A genuine empty `questions: []` stays valid and
/// distinguishable from the damaged case. A non-object element or a non-array
/// top-level body is [`QuestionError::Malformed`] too, so a damaged request is
/// never silently skipped and can never make the list look empty.
///
/// The [`fmt::Debug`]/[`fmt::Display`] representations render only presence
/// flags and counts, never the id, session id, question text, header, options
/// or tool ids.
#[derive(Clone, PartialEq, Eq)]
pub struct Question {
    id: Option<String>,
    session_id: Option<String>,
    questions: Vec<QuestionInfo>,
    tool: Option<QuestionTool>,
}

impl Question {
    /// Returns the request id.
    ///
    /// Parsing rejects a missing, `null` or wrongly typed `id` as
    /// [`QuestionError::Malformed`], so every successfully parsed request
    /// reports `Some` here.
    #[must_use]
    pub fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }

    /// Returns the `sessionID`.
    ///
    /// Parsing rejects a missing, `null` or wrongly typed `sessionID` as
    /// [`QuestionError::Malformed`], so a damaged request can never be silently
    /// dropped by a session filter as if it were absent.
    #[must_use]
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// Returns the nested questions (empty when the server sent `[]`).
    #[must_use]
    pub fn questions(&self) -> &[QuestionInfo] {
        &self.questions
    }

    /// Returns the optional tool envelope.
    #[must_use]
    pub const fn tool(&self) -> Option<&QuestionTool> {
        self.tool.as_ref()
    }

    /// Returns `true` when this request's `sessionID` is exactly `session_id`.
    ///
    /// This is the reference `worker.py::_pending_questions` /
    /// `mcp_server.py::_blockers_present` session filter. Parsing guarantees
    /// `sessionID` is a present string, so a damaged request can never match a
    /// session filter (nor be silently dropped as if it were absent).
    #[must_use]
    pub fn belongs_to_session(&self, session_id: &str) -> bool {
        self.session_id.as_deref() == Some(session_id)
    }

    /// Builds the reference `worker.py::_pending_questions` question blocker.
    ///
    /// The text is the first nested question's `question` string, truncated to
    /// the reference 300-character limit. The reference reads
    /// `str(question["questions"][0].get("question", ""))` only when `questions`
    /// is a non-empty list, so an empty list yields the empty text; the
    /// truncation counts Unicode code points exactly like Python string slicing.
    #[must_use]
    pub fn blocker(&self) -> QuestionBlocker {
        let text = self.questions.first().map_or("", QuestionInfo::question);
        QuestionBlocker {
            text: text.chars().take(QUESTION_BLOCKER_TEXT_LIMIT).collect(),
        }
    }
}

impl fmt::Debug for Question {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Question")
            .field("id", &self.id.as_ref().map(|_| "[redacted]"))
            .field(
                "session_id",
                &self.session_id.as_ref().map(|_| "[redacted]"),
            )
            .field("questions", &self.questions.len())
            .field("tool", &self.tool)
            .finish()
    }
}

impl fmt::Display for Question {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "OpenCode question request (id present: {}, questions: {}, tool present: {})",
            self.id.is_some(),
            self.questions.len(),
            self.tool.is_some()
        )
    }
}

/// A typed question-operation failure (task 6.8).
///
/// [`QuestionError::Transport`] preserves the underlying [`TransportError`]
/// (timeout, unavailable, HTTP 401, HTTP 404, other non-success, protocol);
/// [`QuestionError::Malformed`] means the successful `GET /question` body was
/// not valid JSON, was not a JSON array, or carried a malformed element. There
/// is no reply operation in this stage, so there is no invalid-id variant.
/// Neither [`fmt::Debug`] nor [`fmt::Display`] renders the endpoint, the query,
/// the request/session id, the question content, the credential or the response
/// body.
#[derive(Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum QuestionError {
    /// The transport itself failed.
    Transport(TransportError),
    /// The question-list response body was not a valid JSON array of objects.
    Malformed,
}

impl From<TransportError> for QuestionError {
    fn from(error: TransportError) -> Self {
        Self::Transport(error)
    }
}

impl fmt::Debug for QuestionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(error) => write!(f, "Transport({error:?})"),
            Self::Malformed => f.write_str("Malformed"),
        }
    }
}

impl fmt::Display for QuestionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(error) => write!(f, "OpenCode question request failed: {error}"),
            Self::Malformed => f.write_str("OpenCode question response is malformed"),
        }
    }
}

impl Error for QuestionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Transport(error) => Some(error),
            Self::Malformed => None,
        }
    }
}

/// Parses a question-list body (`list_questions`).
///
/// The top level must be a JSON array; anything else is
/// [`QuestionError::Malformed`]. Every element must be an object and is parsed
/// fail-closed, so a non-object element or a malformed field is
/// [`QuestionError::Malformed`] rather than a silently skipped entry: a damaged
/// pending question must not disappear from the list.
fn parse_question_list(body: &[u8]) -> Result<Vec<Question>, QuestionError> {
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| QuestionError::Malformed)?;
    let items = value.as_array().ok_or(QuestionError::Malformed)?;
    items.iter().map(question_from_value).collect()
}

/// Maps one JSON value to a [`Question`], failing closed on a non-object.
fn question_from_value(value: &serde_json::Value) -> Result<Question, QuestionError> {
    let object = value.as_object().ok_or(QuestionError::Malformed)?;
    // The SDK `QuestionRequest` marks `id` and `sessionID` as required strings.
    // A missing, `null` or wrongly typed identity is
    // [`QuestionError::Malformed`] rather than `None`, so a damaged pending
    // question can never be mistaken for an absent one (for example, silently
    // dropped by a future `sessionID` filter).
    let required_string = |name: &str| match object.get(name) {
        Some(serde_json::Value::String(text)) => Ok(text.clone()),
        _ => Err(QuestionError::Malformed),
    };
    // `questions` is a required array in the SDK shape; an absent or `null`
    // value is malformed and must not collapse to an empty vector, while a
    // genuine `[]` is valid and stays distinguishable from the damaged case.
    let questions = match object.get("questions") {
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .map(question_info_from_value)
            .collect::<Result<Vec<QuestionInfo>, QuestionError>>(
        )?,
        _ => return Err(QuestionError::Malformed),
    };
    let tool = match object.get("tool") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::Object(tool)) => Some(QuestionTool {
            message_id: tool_string(tool, "messageID", QuestionError::Malformed)?,
            call_id: tool_string(tool, "callID", QuestionError::Malformed)?,
        }),
        Some(_) => return Err(QuestionError::Malformed),
    };
    Ok(Question {
        id: Some(required_string("id")?),
        session_id: Some(required_string("sessionID")?),
        questions,
        tool,
    })
}

/// Maps one JSON value to a [`QuestionInfo`], failing closed on a non-object.
fn question_info_from_value(value: &serde_json::Value) -> Result<QuestionInfo, QuestionError> {
    let object = value.as_object().ok_or(QuestionError::Malformed)?;
    let required_string = |name: &str| match object.get(name) {
        Some(serde_json::Value::String(text)) => Ok(text.clone()),
        _ => Err(QuestionError::Malformed),
    };
    // `options` is a required array of `QuestionOption` in the SDK shape; an
    // absent or `null` value is malformed, while a valid `[]` is preserved.
    let options = match object.get("options") {
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .map(question_option_from_value)
            .collect::<Result<Vec<QuestionOption>, QuestionError>>(
        )?,
        _ => return Err(QuestionError::Malformed),
    };
    Ok(QuestionInfo {
        question: required_string("question")?,
        header: required_string("header")?,
        options,
        multiple: optional_bool(object.get("multiple"))?,
        custom: optional_bool(object.get("custom"))?,
    })
}

/// Maps one JSON value to a [`QuestionOption`], failing closed on a non-object.
fn question_option_from_value(value: &serde_json::Value) -> Result<QuestionOption, QuestionError> {
    let object = value.as_object().ok_or(QuestionError::Malformed)?;
    let required_string = |name: &str| match object.get(name) {
        Some(serde_json::Value::String(text)) => Ok(text.clone()),
        _ => Err(QuestionError::Malformed),
    };
    Ok(QuestionOption {
        label: required_string("label")?,
        description: required_string("description")?,
    })
}

/// Reads an optional boolean member, failing closed on a wrongly typed value.
///
/// The optional `multiple`/`custom` flags keep the reference default of `None`
/// when absent or `null`; a present value of any other JSON type is
/// [`QuestionError::Malformed`].
fn optional_bool(value: Option<&serde_json::Value>) -> Result<Option<bool>, QuestionError> {
    match value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Bool(flag)) => Ok(Some(*flag)),
        Some(_) => Err(QuestionError::Malformed),
    }
}

/// A pending question blocker for one session (task 6.8).
///
/// The reference `worker.py::_pending_questions` turns each matching question
/// request into `{"type": "question", "text": ...}`, where `text` is the first
/// `questions[].question` string truncated to 300 characters. This type carries
/// exactly that text; the `type` discriminator is exposed through
/// [`QuestionBlocker::kind`]. The [`fmt::Debug`]/[`fmt::Display`]
/// representations render only the text length, never the text itself.
#[derive(Clone, PartialEq, Eq)]
pub struct QuestionBlocker {
    text: String,
}

impl QuestionBlocker {
    /// Returns the reference `"question"` blocker type discriminator.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        "question"
    }

    /// Returns the first question's text, truncated to 300 characters.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }
}

impl fmt::Debug for QuestionBlocker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QuestionBlocker")
            .field("kind", &self.kind())
            .field("text", &self.text.chars().count())
            .finish()
    }
}

impl fmt::Display for QuestionBlocker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "OpenCode question blocker (text chars: {})",
            self.text.chars().count()
        )
    }
}

/// The pending permission and question blockers of one session (task 6.8).
///
/// A pure, reusable projection of the typed [`Permission`] and [`Question`]
/// lists onto a single `sessionID`, mirroring the reference presence checks
/// `worker.py::_pending_permissions`/`_pending_questions` and
/// `mcp_server.py::_blockers_present` without implementing the worker state
/// machine, auto-approval or any question reply API. It does not probe the
/// endpoint itself, so a transport or malformed response stays visible to the
/// caller (the reference consumers decide whether to swallow it or fail
/// closed); this helper cannot hide an error.
///
/// For successfully fetched lists the reference `_blockers_present` is `true`
/// exactly when at least one blocker is present, which is equivalent to
/// `!SessionBlockers::is_empty()` (a `true` [`SessionBlockers::is_empty`] means
/// the session has no blocker, the opposite of `_blockers_present`). The
/// reference error policy — whether a transport or malformed response counts as
/// a blocker — stays the caller's responsibility; this helper does not encode
/// it.
#[derive(Clone, Debug)]
pub struct SessionBlockers {
    permissions: Vec<Permission>,
    questions: Vec<QuestionBlocker>,
}

impl SessionBlockers {
    /// Filters `permissions` and `questions` to exactly `session_id`.
    ///
    /// The `sessionID` comparison is exact and byte-for-byte, so requests from
    /// another session are never mixed in. The question blockers carry the
    /// reference truncated text.
    #[must_use]
    pub fn detect(permissions: &[Permission], questions: &[Question], session_id: &str) -> Self {
        Self {
            permissions: permissions
                .iter()
                .filter(|permission| permission.belongs_to_session(session_id))
                .cloned()
                .collect(),
            questions: questions
                .iter()
                .filter(|question| question.belongs_to_session(session_id))
                .map(Question::blocker)
                .collect(),
        }
    }

    /// Returns `true` when the session has no pending permission or question.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.permissions.is_empty() && self.questions.is_empty()
    }

    /// Returns the session's pending permission requests.
    #[must_use]
    pub fn permissions(&self) -> &[Permission] {
        &self.permissions
    }

    /// Returns the session's pending question blockers.
    #[must_use]
    pub fn questions(&self) -> &[QuestionBlocker] {
        &self.questions
    }
}

impl fmt::Display for SessionBlockers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "OpenCode session blockers (permissions: {}, questions: {})",
            self.permissions.len(),
            self.questions.len()
        )
    }
}

/// Parses a session message-list body (`list_messages`).
///
/// The top level must be a JSON array; anything else is
/// [`MessageError::Malformed`]. Every element must be an object and is parsed
/// fail-closed, so a non-object element is [`MessageError::Malformed`] rather
/// than a silently skipped entry: a damaged trailing turn must not leave an
/// older completed assistant as the last observable message.
fn parse_message_list(body: &[u8]) -> Result<Vec<Message>, MessageError> {
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| MessageError::Malformed)?;
    let items = value.as_array().ok_or(MessageError::Malformed)?;
    items.iter().map(message_from_value).collect()
}

/// Maps a JSON object to a [`Message`], failing closed on a non-object element.
fn message_from_value(value: &serde_json::Value) -> Result<Message, MessageError> {
    let object = value.as_object().ok_or(MessageError::Malformed)?;
    Ok(Message {
        info: parse_message_info(object.get("info"))?,
        parts: parse_message_parts(object.get("parts"))?,
    })
}

/// Parses the `info` object of a message.
///
/// An absent `info` keeps the reference default (an empty metadata object) and
/// a wrongly typed scalar field stays permissive (`None`/`false`/zero). A
/// present `info` that is not an object, or a present truthy `time` that is not
/// an object, is [`MessageError::Malformed`]: the reference would call `.get`
/// on them and raise, so accepting them could only manufacture a false
/// completion.
fn parse_message_info(value: Option<&serde_json::Value>) -> Result<MessageInfo, MessageError> {
    let object = match value {
        None => {
            return Ok(MessageInfo {
                id: None,
                role: None,
                parent_id: None,
                session_id: None,
                provider_id: None,
                model_id: None,
                usage: Usage::empty(),
                completed: false,
                finish: None,
                error: None,
            });
        }
        Some(serde_json::Value::Object(object)) => object,
        Some(_) => return Err(MessageError::Malformed),
    };
    let string = |name: &str| {
        object
            .get(name)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    let usage = parse_usage(object.get("tokens"), object.get("cost"));
    let completed = match object.get("time") {
        None => false,
        Some(serde_json::Value::Object(time)) => {
            time.get("completed").is_some_and(|value| !value.is_null())
        }
        Some(time) if is_truthy(time) => return Err(MessageError::Malformed),
        Some(_) => false,
    };
    let error = object
        .get("error")
        .filter(|value| is_truthy(value))
        .cloned();
    Ok(MessageInfo {
        id: string("id"),
        role: string("role"),
        parent_id: string("parentID"),
        session_id: string("sessionID"),
        provider_id: string("providerID"),
        model_id: string("modelID"),
        usage,
        completed,
        finish: string("finish"),
        error,
    })
}

/// Parses the `tokens`/`cost` accounting with the reference `_number` rules.
fn parse_usage(tokens: Option<&serde_json::Value>, cost: Option<&serde_json::Value>) -> Usage {
    let tokens = tokens.and_then(serde_json::Value::as_object);
    let cache = tokens
        .and_then(|map| map.get("cache"))
        .and_then(serde_json::Value::as_object);
    Usage {
        input: normalized_number(tokens.and_then(|map| map.get("input"))),
        output: normalized_number(tokens.and_then(|map| map.get("output"))),
        reasoning: normalized_number(tokens.and_then(|map| map.get("reasoning"))),
        cache_read: normalized_number(cache.and_then(|map| map.get("read"))),
        cache_write: normalized_number(cache.and_then(|map| map.get("write"))),
        cost: normalized_number(cost),
    }
}

/// Normalizes a JSON value with the reference `usage._number` semantics: only
/// JSON numbers are accepted, and a missing, boolean, negative or non-finite
/// value becomes `0.0`.
fn normalized_number(value: Option<&serde_json::Value>) -> f64 {
    match value.and_then(serde_json::Value::as_f64) {
        Some(number) if number >= 0.0 && number.is_finite() => number,
        _ => 0.0,
    }
}

/// Parses the `parts` array.
///
/// An absent `parts` keeps the reference default (an empty part list). A
/// present `parts` that is not an array, or any element that is not an object,
/// is [`MessageError::Malformed`]: the reference would fail while iterating
/// them, so skipping them could hide an unresolved tool part and let a damaged
/// turn look final.
fn parse_message_parts(
    value: Option<&serde_json::Value>,
) -> Result<Vec<MessagePart>, MessageError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let items = value.as_array().ok_or(MessageError::Malformed)?;
    items.iter().map(message_part_from_value).collect()
}

/// Maps a JSON object to a [`MessagePart`], failing closed on a malformed
/// lifecycle container or text value.
///
/// The containers the reference reads on this part (`state`, part `metadata`
/// and `state.metadata` for a `tool` part, and `text` for a `text` part) are
/// validated only when that part type makes the reference read them: a present
/// truthy value of the wrong type is [`MessageError::Malformed`] because the
/// reference would raise, while a falsy value keeps the reference `or {}`/
/// truthy defaults. Other fields stay permissive.
fn message_part_from_value(value: &serde_json::Value) -> Result<MessagePart, MessageError> {
    let object = value.as_object().ok_or(MessageError::Malformed)?;
    let string = |name: &str| {
        object
            .get(name)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    let kind = string("type");
    let is_text = kind.as_deref() == Some("text");
    let text = match object.get("text") {
        None => None,
        Some(serde_json::Value::String(text)) => Some(text.clone()),
        Some(text) if is_text && is_truthy(text) => return Err(MessageError::Malformed),
        Some(_) => None,
    };
    let is_tool = kind.as_deref() == Some("tool");
    let state = match object.get("state") {
        None => None,
        Some(serde_json::Value::Object(state)) => Some(state),
        Some(state) if is_tool && is_truthy(state) => return Err(MessageError::Malformed),
        Some(_) => None,
    };
    let metadata = match object.get("metadata") {
        None => None,
        Some(serde_json::Value::Object(metadata)) => Some(metadata),
        Some(metadata) if is_tool && is_truthy(metadata) => return Err(MessageError::Malformed),
        Some(_) => None,
    };
    let state_metadata = match state.and_then(|map| map.get("metadata")) {
        None => None,
        Some(serde_json::Value::Object(metadata)) => Some(metadata),
        Some(metadata) if is_tool && is_truthy(metadata) => {
            return Err(MessageError::Malformed);
        }
        Some(_) => None,
    };
    Ok(MessagePart {
        kind,
        text,
        ignored: object.get("ignored").is_some_and(is_truthy),
        tool: string("tool"),
        tool_status: state
            .and_then(|map| map.get("status"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        tool_error: state.and_then(|map| map.get("error")).cloned(),
        provider_executed: metadata
            .and_then(|map| map.get("providerExecuted"))
            .is_some_and(is_truthy),
        interrupted: state_metadata
            .and_then(|map| map.get("interrupted"))
            .is_some_and(|value| value == &serde_json::Value::Bool(true)),
    })
}

/// Parses a `/global/health` body with the reference's exact truth rule.
fn parse_health(body: &[u8]) -> Result<Health, HealthError> {
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| HealthError::Malformed)?;
    let object = value.as_object().ok_or(HealthError::Malformed)?;
    let healthy = object.get("healthy").and_then(serde_json::Value::as_bool) == Some(true);
    let version = object
        .get("version")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    Ok(Health { healthy, version })
}

/// Extracts the required `directory` string from a `/path` body.
fn parse_server_directory(body: &[u8]) -> Result<String, IdentityError> {
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| IdentityError::Malformed)?;
    let object = value.as_object().ok_or(IdentityError::Malformed)?;
    match object.get("directory") {
        None => Err(IdentityError::MissingDirectory),
        Some(serde_json::Value::String(directory)) => Ok(directory.clone()),
        Some(_) => Err(IdentityError::Malformed),
    }
}

/// Maximum number of symlink components followed before a reported path is
/// treated as a loop and rejected. Mirrors the classic `SYMLOOP_MAX` bound, so
/// a genuine loop becomes a fail-closed rejection instead of an unbounded walk.
const MAX_SYMLINK_DEPTH: usize = 40;

/// Resolves a server-reported directory and compares it to the canonical
/// workspace, mirroring the reference `Path(reported).resolve() == workspace`.
///
/// The resolution follows Python `Path.resolve(strict=False)` semantics rather
/// than a purely lexical fallback: every existing symlink component is followed
/// (an absolute target restarts at the filesystem root, a relative target
/// resolves against the link's parent, and `.`/`..` inside a target are folded),
/// and a missing component is appended while the walk continues, so a `..` that
/// follows a missing component is still folded against the symlink-resolved
/// prefix. The reference always resolves the reported path, so there is no
/// lexical shortcut: a reported path is accepted only when its resolved form is
/// exactly the canonical workspace, even if it merely spells it.
///
/// Every condition that makes the resolution unprovable fails closed (`false`):
/// an embedded NUL (Python raises `ValueError`), a symlink loop, a
/// non-`NotFound` metadata failure (permission denied, not-a-directory, an
/// unreadable link, ...), or a failed current-directory lookup. A reported path
/// that resolves into a foreign tree is therefore rejected, not accepted by a
/// lexical alias.
fn reported_matches_workspace(reported: &str, workspace: &Path) -> bool {
    resolve_reported(reported).is_some_and(|resolved| resolved == workspace)
}

/// Resolves `reported` with Python `Path.resolve(strict=False)` semantics.
///
/// Returns `None` for any unresolvable condition (embedded NUL, symlink loop,
/// non-`NotFound` metadata error, unreadable symlink or unavailable current
/// directory) so that identity fails closed.
fn resolve_reported(reported: &str) -> Option<PathBuf> {
    if reported.as_bytes().contains(&0) {
        return None;
    }
    let reported = Path::new(reported);
    let mut resolved = if reported.is_absolute() {
        PathBuf::from(Component::RootDir.as_os_str())
    } else {
        std::env::current_dir().ok()?
    };
    let mut queue: VecDeque<OsString> = VecDeque::new();
    for component in reported.components() {
        match component {
            Component::Prefix(_) => return None,
            Component::RootDir => {
                queue.clear();
                resolved = PathBuf::from(Component::RootDir.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => queue.push_back(OsString::from("..")),
            Component::Normal(part) => queue.push_back(part.to_os_string()),
        }
    }

    let mut links = 0_usize;
    while let Some(part) = queue.pop_front() {
        let part = part.as_os_str();
        if part == OsStr::new(".") {
            continue;
        }
        if part == OsStr::new("..") {
            resolved.pop();
            continue;
        }
        let candidate = resolved.join(part);
        match std::fs::symlink_metadata(&candidate) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                links += 1;
                if links > MAX_SYMLINK_DEPTH {
                    return None;
                }
                let target = std::fs::read_link(&candidate).ok()?;
                if target.is_absolute() {
                    resolved = PathBuf::from(Component::RootDir.as_os_str());
                }
                let mut target_parts: Vec<OsString> = Vec::new();
                for component in target.components() {
                    match component {
                        Component::Prefix(_) => return None,
                        Component::RootDir | Component::CurDir => {}
                        Component::ParentDir => target_parts.push(OsString::from("..")),
                        Component::Normal(name) => target_parts.push(name.to_os_string()),
                    }
                }
                for target_part in target_parts.into_iter().rev() {
                    queue.push_front(target_part);
                }
            }
            Ok(_) => resolved.push(part),
            Err(error) if error.kind() == io::ErrorKind::NotFound => resolved.push(part),
            Err(_) => return None,
        }
    }
    Some(resolved)
}

/// The parsed status line and framing headers of a response.
struct ParsedHead {
    status: u16,
    chunked: bool,
    content_length: Option<usize>,
}

/// Reads a response incrementally under a single deadline.
struct ResponseReader<'a> {
    stream: &'a TcpStream,
    deadline: Instant,
    buffer: Vec<u8>,
}

impl<'a> ResponseReader<'a> {
    fn new(stream: &'a TcpStream, deadline: Instant) -> Self {
        Self {
            stream,
            deadline,
            buffer: Vec::new(),
        }
    }

    /// Reads the status line and header block, including the final blank line.
    fn read_head(&mut self) -> Result<Vec<u8>, TransportError> {
        loop {
            if let Some(index) = find_subslice(&self.buffer, b"\r\n\r\n") {
                let end = index + 4;
                return Ok(self.buffer.drain(..end).collect());
            }
            if !self.read_more()? {
                return Err(TransportError::Protocol);
            }
            if self.buffer.len() > MAX_HEAD_BYTES {
                return Err(TransportError::Protocol);
            }
        }
    }

    /// Reads the response body according to its framing headers.
    fn read_body(&mut self, parsed: &ParsedHead) -> Result<Vec<u8>, TransportError> {
        if parsed.chunked {
            self.read_chunked()
        } else if let Some(length) = parsed.content_length {
            if length > MAX_BODY_BYTES {
                return Err(TransportError::Protocol);
            }
            self.read_exact_len(length)
        } else {
            self.read_to_eof()
        }
    }

    /// Reads exactly `length` bytes.
    fn read_exact_len(&mut self, length: usize) -> Result<Vec<u8>, TransportError> {
        if length > MAX_BODY_BYTES {
            return Err(TransportError::Protocol);
        }
        while self.buffer.len() < length {
            if !self.read_more()? {
                return Err(TransportError::Protocol);
            }
        }
        Ok(self.buffer.drain(..length).collect())
    }

    /// Reads until the peer closes the connection.
    fn read_to_eof(&mut self) -> Result<Vec<u8>, TransportError> {
        while self.read_more()? {}
        Ok(std::mem::take(&mut self.buffer))
    }

    /// Decodes a `Transfer-Encoding: chunked` body.
    fn read_chunked(&mut self) -> Result<Vec<u8>, TransportError> {
        let mut body = Vec::new();
        loop {
            let line = self.read_line()?;
            let size_text = line
                .split(|byte| *byte == b';')
                .next()
                .unwrap_or_default()
                .trim_ascii();
            let size = parse_hex(size_text)?;
            if size == 0 {
                loop {
                    let trailer = self.read_line()?;
                    if trailer.is_empty() {
                        return Ok(body);
                    }
                }
            }
            if body.len().saturating_add(size) > MAX_BODY_BYTES {
                return Err(TransportError::Protocol);
            }
            let chunk = self.read_exact_len(size)?;
            body.extend_from_slice(&chunk);
            if self.read_exact_len(2)? != b"\r\n" {
                return Err(TransportError::Protocol);
            }
        }
    }

    /// Reads one `CRLF`-terminated line, returning it without the terminator.
    fn read_line(&mut self) -> Result<Vec<u8>, TransportError> {
        loop {
            if let Some(index) = find_subslice(&self.buffer, b"\r\n") {
                let line: Vec<u8> = self.buffer.drain(..index).collect();
                self.buffer.drain(..2);
                return Ok(line);
            }
            if !self.read_more()? {
                return Err(TransportError::Protocol);
            }
            if self.buffer.len() > MAX_HEAD_BYTES {
                return Err(TransportError::Protocol);
            }
        }
    }

    /// Reads more bytes into the buffer, bounded by the remaining deadline.
    fn read_more(&mut self) -> Result<bool, TransportError> {
        self.stream
            .set_read_timeout(Some(remaining(self.deadline)?))
            .map_err(map_io)?;
        let mut chunk = [0_u8; 8 * 1024];
        match self.stream.read(&mut chunk) {
            Ok(0) => Ok(false),
            Ok(count) => {
                self.buffer.extend_from_slice(&chunk[..count]);
                if self.buffer.len() > MAX_BODY_BYTES {
                    return Err(TransportError::Protocol);
                }
                Ok(true)
            }
            Err(error) => Err(map_io(error)),
        }
    }
}

/// Parses the response head into the status and framing headers.
fn parse_head(head: &[u8]) -> Result<ParsedHead, TransportError> {
    let text = head
        .strip_suffix(b"\r\n\r\n")
        .ok_or(TransportError::Protocol)?;
    let mut lines = text.split(|byte| *byte == b'\n');
    let status_line = strip_cr(lines.next().ok_or(TransportError::Protocol)?);
    let status = parse_status(status_line)?;
    let mut chunked = false;
    let mut content_length = None;
    for line in lines {
        let line = strip_cr(line);
        if line.is_empty() {
            continue;
        }
        let colon = line
            .iter()
            .position(|byte| *byte == b':')
            .ok_or(TransportError::Protocol)?;
        let name = line[..colon].trim_ascii();
        let value = line[colon + 1..].trim_ascii();
        if name.eq_ignore_ascii_case(b"content-length") {
            if content_length.is_some() {
                return Err(TransportError::Protocol);
            }
            content_length = Some(parse_decimal(value).ok_or(TransportError::Protocol)?);
        } else if name.eq_ignore_ascii_case(b"transfer-encoding")
            && value
                .split(|byte| *byte == b',')
                .any(|token| token.trim_ascii().eq_ignore_ascii_case(b"chunked"))
        {
            chunked = true;
        }
    }
    Ok(ParsedHead {
        status,
        chunked,
        content_length,
    })
}

/// Parses a supported `HTTP/1.0` or `HTTP/1.1` status line into its numeric
/// status code.
///
/// The version must be exactly `HTTP/1.0` or `HTTP/1.1`, and the status code
/// must be exactly three ASCII digits in the range `100..=599`. Anything else —
/// a different protocol token, a leading zero or a non-three-digit code, or a
/// code outside the valid range — is rejected as [`TransportError::Protocol`].
fn parse_status(line: &[u8]) -> Result<u16, TransportError> {
    let mut parts = line.splitn(3, |byte| *byte == b' ');
    let version = parts.next().ok_or(TransportError::Protocol)?;
    if version != b"HTTP/1.0" && version != b"HTTP/1.1" {
        return Err(TransportError::Protocol);
    }
    let code = parts.next().ok_or(TransportError::Protocol)?;
    if code.len() != 3 || !code.iter().all(u8::is_ascii_digit) {
        return Err(TransportError::Protocol);
    }
    let code = parse_decimal(code).ok_or(TransportError::Protocol)?;
    if !(100..=599).contains(&code) {
        return Err(TransportError::Protocol);
    }
    u16::try_from(code).map_err(|_| TransportError::Protocol)
}

/// Removes a single trailing `\r`.
fn strip_cr(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\r").unwrap_or(line)
}

/// Parses a non-empty decimal number.
fn parse_decimal(bytes: &[u8]) -> Option<usize> {
    if bytes.is_empty() {
        return None;
    }
    let mut value: usize = 0;
    for byte in bytes {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value
            .checked_mul(10)?
            .checked_add(usize::from(byte - b'0'))?;
    }
    Some(value)
}

/// Parses a non-empty hexadecimal chunk size.
fn parse_hex(bytes: &[u8]) -> Result<usize, TransportError> {
    if bytes.is_empty() {
        return Err(TransportError::Protocol);
    }
    let mut value: usize = 0;
    for byte in bytes {
        let digit = match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            b'A'..=b'F' => byte - b'A' + 10,
            _ => return Err(TransportError::Protocol),
        };
        value = value
            .checked_mul(16)
            .and_then(|value| value.checked_add(usize::from(digit)))
            .ok_or(TransportError::Protocol)?;
    }
    Ok(value)
}

/// Maps a numeric status to its typed error, keeping dedicated variants for the
/// categories the reference client distinguishes.
fn map_status(status: u16) -> TransportError {
    match status {
        401 => TransportError::Unauthorized,
        404 => TransportError::NotFound,
        other => TransportError::HttpStatus(other),
    }
}

/// Maps an I/O error to a timeout or a connection failure.
fn map_io(error: io::Error) -> TransportError {
    if matches!(
        error.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    ) {
        TransportError::Timeout
    } else {
        TransportError::Unavailable
    }
}

/// Returns the time left before `deadline`, or [`TransportError::Timeout`].
fn remaining(deadline: Instant) -> Result<Duration, TransportError> {
    let now = Instant::now();
    if now >= deadline {
        Err(TransportError::Timeout)
    } else {
        Ok(deadline - now)
    }
}

/// Writes `data` completely, recomputing the remaining deadline before every
/// partial write.
///
/// `remaining_time` yields the budget for the next blocking write (in
/// production it is the time left before the shared deadline, and the budget is
/// applied as the socket write timeout); `write` performs one bounded write
/// attempt and may make partial progress. The budget is recomputed on every
/// iteration rather than once, so a peer that accepts the connection but reads
/// slowly cannot extend the request by resetting a per-write timer: once the
/// deadline has passed `remaining_time` fails and no further byte is written. A
/// zero-byte write is treated as a failed socket, and an interrupted write is
/// retried under the same deadline.
///
/// The two closures keep the deadline loop directly testable with a fake clock
/// and deterministic partial writes, without touching the network or the public
/// API.
fn write_all_until<R, W>(
    mut data: &[u8],
    mut remaining_time: R,
    mut write: W,
) -> Result<(), TransportError>
where
    R: FnMut() -> Result<Duration, TransportError>,
    W: FnMut(Duration, &[u8]) -> io::Result<usize>,
{
    while !data.is_empty() {
        let budget = remaining_time()?;
        match write(budget, data) {
            Ok(0) => return Err(TransportError::Unavailable),
            Ok(count) => data = &data[count..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(map_io(error)),
        }
    }
    Ok(())
}

/// Returns `true` for statuses that never carry a response body.
///
/// Only successful responses reach body handling, so this is the successful
/// `204 No Content` in practice: it has no body irrespective of any
/// `Content-Length` or `Transfer-Encoding` header, so body framing must not be
/// consulted and the connection must not be drained to EOF. A `304 Not Modified`
/// is likewise bodyless in HTTP, but it is a non-success status that the
/// transport rejects before this point, so its body is never read either.
const fn status_has_no_body(status: u16) -> bool {
    matches!(status, 204 | 304)
}

/// Validates that a request path is rooted, uses only RFC 3986 path bytes and
/// spells every percent escape as a complete `%` HEXDIG HEXDIG triplet.
///
/// A bare `%`, a truncated `%X` or a non-hex `%GG` is not a valid
/// `pct-encoded` octet and would be forwarded verbatim to the peer, so it is
/// rejected as [`TransportError::InvalidRequest`] before any socket is opened.
/// Raw whitespace, query (`?`), fragment (`#`) and control bytes stay rejected
/// by [`is_path_byte`].
fn validate_path(path: &str) -> Result<(), TransportError> {
    let bytes = path.as_bytes();
    if !path.starts_with('/') || !bytes.iter().copied().all(is_path_byte) {
        return Err(TransportError::InvalidRequest);
    }
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            match bytes.get(index + 1..index + 3) {
                Some([high, low]) if high.is_ascii_hexdigit() && low.is_ascii_hexdigit() => {}
                _ => return Err(TransportError::InvalidRequest),
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    Ok(())
}

/// Returns `true` for the RFC 3986 `pchar`/`/` subset accepted in a path, plus
/// `%` for a percent-encoded octet (used to encode a session id as one path
/// segment). [`validate_path`] separately requires every `%` to start a
/// complete hex triplet.
fn is_path_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'-' | b'.'
                | b'_'
                | b'~'
                | b'!'
                | b'$'
                | b'&'
                | b'\''
                | b'('
                | b')'
                | b'*'
                | b'+'
                | b','
                | b';'
                | b'='
                | b':'
                | b'@'
                | b'/'
                | b'%'
        )
}

/// Finds the first occurrence of `needle` in `haystack`.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Encodes bytes with standard Base64 (RFC 4648) and padding.
fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let first = u32::from(chunk[0]);
        let second = u32::from(*chunk.get(1).unwrap_or(&0));
        let third = u32::from(*chunk.get(2).unwrap_or(&0));
        let triple = (first << 16) | (second << 8) | third;
        encoded.push(ALPHABET[(triple >> 18) as usize & 0x3f] as char);
        encoded.push(ALPHABET[(triple >> 12) as usize & 0x3f] as char);
        encoded.push(if chunk.len() > 1 {
            ALPHABET[(triple >> 6) as usize & 0x3f] as char
        } else {
            '='
        });
        encoded.push(if chunk.len() > 2 {
            ALPHABET[triple as usize & 0x3f] as char
        } else {
            '='
        });
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::{self, JoinHandle};
    use std::time::{SystemTime, UNIX_EPOCH};

    const PASSWORD: &str = "test-password";
    const PASSWORD_BASE64: &str = "b3BlbmNvZGU6dGVzdC1wYXNzd29yZA==";

    /// A temporary directory removed recursively on drop, mirroring the helper
    /// used by the `bridge-config` tests.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock must be after the Unix epoch")
                .as_nanos();
            let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "bridge-opencode-{tag}-{}-{nanos}-{sequence}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("temporary directory must be creatable");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }

        fn mkdir(&self, name: &str) -> PathBuf {
            let path = self.path.join(name);
            std::fs::create_dir_all(&path).expect("temporary subdirectory must be creatable");
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// Forces the mode of a credential file to `0600`.
    fn set_private(path: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .expect("credential mode must be settable");
        }
        #[cfg(not(unix))]
        {
            let _ = path;
        }
    }

    /// Writes a one-project configuration for `port` and returns the path.
    fn write_project(dir: &TempDir, port: u16, with_password: bool) -> PathBuf {
        dir.mkdir("ws");
        dir.mkdir("secrets");
        if with_password {
            let password = dir.path().join("secrets/proj.password");
            std::fs::write(&password, format!("{PASSWORD}\n")).expect("password must be writable");
            set_private(&password);
        }
        let config = format!(
            "[projects.proj]\nworkspace = \"ws\"\nopencode_url = \"http://127.0.0.1:{port}\"\npassword_file = \"secrets/proj.password\"\nmax_rounds = 3\n"
        );
        let config_path = dir.path().join("projects.toml");
        std::fs::write(&config_path, config).expect("config must be writable");
        config_path
    }

    /// Loads one project and returns its validated endpoint and password.
    fn project_material(port: u16) -> (TempDir, Endpoint, Secret) {
        let dir = TempDir::new("material");
        let config_path = write_project(&dir, port, true);
        let config = bridge_config::load_config(&config_path).expect("config must load");
        let project = config.project("proj").expect("project must exist");
        let endpoint = *project.opencode_endpoint();
        let secret = project.read_password().expect("password must read");
        (dir, endpoint, secret)
    }

    /// Reads one full HTTP request (head plus a `Content-Length` body).
    fn read_request(stream: &mut TcpStream) -> Vec<u8> {
        let mut data = Vec::new();
        let mut chunk = [0_u8; 1024];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(count) => {
                    data.extend_from_slice(&chunk[..count]);
                    if let Some(head_end) = find_subslice(&data, b"\r\n\r\n") {
                        let length = content_length(&data[..head_end]).unwrap_or(0);
                        if data.len() >= head_end + 4 + length {
                            break;
                        }
                    }
                }
                Err(_) => break,
            }
        }
        data
    }

    /// Returns the declared `Content-Length` of a request head, if any.
    fn content_length(head: &[u8]) -> Option<usize> {
        for line in head.split(|byte| *byte == b'\n') {
            let line = strip_cr(line);
            if let Some(colon) = line.iter().position(|byte| *byte == b':')
                && line[..colon]
                    .trim_ascii()
                    .eq_ignore_ascii_case(b"content-length")
            {
                return parse_decimal(line[colon + 1..].trim_ascii());
            }
        }
        None
    }

    /// A loopback mock server plus the buffer of raw requests it recorded.
    ///
    /// The handle owns the server thread: dropping it signals the accept loop
    /// to stop and joins the thread, so every test releases its listener and
    /// thread before the test function returns. `Deref` exposes the recorded
    /// bytes as the underlying `Arc<Mutex<Vec<u8>>>`, so callers keep using
    /// `captured.lock()` unchanged.
    struct ServerCapture {
        captured: Arc<Mutex<Vec<u8>>>,
        stop: Arc<AtomicBool>,
        handle: Option<JoinHandle<()>>,
    }

    impl std::ops::Deref for ServerCapture {
        type Target = Arc<Mutex<Vec<u8>>>;

        fn deref(&self) -> &Self::Target {
            &self.captured
        }
    }

    impl Drop for ServerCapture {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    /// Spawns a loopback server that records the raw request and replies with
    /// whatever `handler` returns.
    ///
    /// The listener is non-blocking and the accept loop polls a stop flag with
    /// a short sleep, so the thread wakes up and exits promptly when the
    /// returned [`ServerCapture`] is dropped; no test leaves an accept loop or
    /// a listener behind.
    fn spawn_server<F>(handler: F) -> (u16, ServerCapture)
    where
        F: Fn(&[u8]) -> Vec<u8> + Send + 'static,
    {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .expect("server must bind an ephemeral port");
        let port = listener
            .local_addr()
            .expect("bound address must be available")
            .port();
        listener
            .set_nonblocking(true)
            .expect("server listener must be non-blocking");
        let captured: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&captured);
        let stop = Arc::new(AtomicBool::new(false));
        let stopper = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            while !stopper.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_nonblocking(false)
                            .expect("accepted stream must be blocking");
                        stream
                            .set_read_timeout(Some(Duration::from_secs(5)))
                            .expect("server read timeout must be settable");
                        let request = read_request(&mut stream);
                        if request.is_empty() {
                            continue;
                        }
                        recorder
                            .lock()
                            .expect("capture mutex must not be poisoned")
                            .extend_from_slice(&request);
                        let response = handler(&request);
                        let _ = stream.write_all(&response);
                        let _ = stream.flush();
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(_) => break,
                }
            }
        });
        let capture = ServerCapture {
            captured,
            stop,
            handle: Some(handle),
        };
        (port, capture)
    }

    /// Builds a `200 OK` JSON response with a `Content-Length` body.
    fn ok_json(body: &[u8]) -> Vec<u8> {
        let mut response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        response
    }

    /// Builds an unsuccessful response with a body that must never leak.
    fn error_response(status: u16) -> Vec<u8> {
        format!(
            "HTTP/1.1 {status} Error\r\ncontent-length: 17\r\nconnection: close\r\n\r\ndo-not-leak-body!"
        )
        .into_bytes()
    }

    #[test]
    fn base64_matches_rfc4648_vectors() {
        let cases: [(&[u8], &str); 7] = [
            (b"", ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg=="),
            (b"fooba", "Zm9vYmE="),
            (b"foobar", "Zm9vYmFy"),
        ];
        for (input, expected) in cases {
            assert_eq!(base64_encode(input), expected);
        }
    }

    #[test]
    fn basic_auth_header_matches_opencode_contract() {
        let (_dir, _endpoint, secret) = project_material(4101);
        let auth = BasicAuth::new(secret);
        assert_eq!(
            auth.header_value(),
            format!("Basic {PASSWORD_BASE64}"),
            "the header must be Basic base64('opencode:<password>')"
        );
    }

    #[test]
    fn basic_auth_from_project_reads_password_file() {
        let dir = TempDir::new("from-project");
        let config_path = write_project(&dir, 4102, true);
        let config = bridge_config::load_config(&config_path).expect("config must load");
        let project = config.project("proj").expect("project must exist");
        let auth = BasicAuth::from_project(project).expect("auth must build");
        assert_eq!(auth.header_value(), format!("Basic {PASSWORD_BASE64}"));
    }

    #[test]
    fn missing_password_file_maps_to_invalid_auth() {
        let dir = TempDir::new("missing-password");
        let config_path = write_project(&dir, 4103, false);
        let config = bridge_config::load_config(&config_path).expect("config must load");
        let project = config.project("proj").expect("project must exist");
        let error = BasicAuth::from_project(project).expect_err("auth must fail");
        assert_eq!(error, TransportError::InvalidAuth);
        assert_eq!(
            error.to_string(),
            "OpenCode auth material could not be read"
        );
    }

    #[test]
    fn get_request_is_constructed_with_auth_and_query() {
        let (port, captured) = spawn_server(|_request| ok_json(b"{\"ok\":true}"));
        let (_dir, endpoint, secret) = project_material(port);
        assert_eq!(endpoint.port(), port);
        let transport =
            HttpTransport::new(endpoint, BasicAuth::new(secret), Duration::from_secs(5));
        let request = HttpRequest::get("/global/health").with_query("directory", "/tmp/ws");
        let response = transport.request(&request).expect("request must succeed");
        assert_eq!(response.status(), 200);
        assert_eq!(response.body(), b"{\"ok\":true}");

        let raw = String::from_utf8(captured.lock().expect("capture mutex").clone())
            .expect("captured request must be ASCII");
        assert!(raw.starts_with("GET /global/health?directory=%2Ftmp%2Fws HTTP/1.1\r\n"));
        assert!(raw.contains(&format!("host: 127.0.0.1:{port}\r\n")));
        assert!(raw.contains(&format!("authorization: Basic {PASSWORD_BASE64}\r\n")));
        assert!(raw.contains("accept: application/json\r\n"));
        assert!(raw.contains("connection: close\r\n"));
        assert!(!raw.contains("content-type"));
        assert!(!raw.contains("content-length"));
    }

    #[test]
    fn post_request_carries_json_body_and_length() {
        let (port, captured) = spawn_server(|_request| ok_json(b"{}"));
        let (_dir, endpoint, secret) = project_material(port);
        let transport =
            HttpTransport::new(endpoint, BasicAuth::new(secret), Duration::from_secs(5));
        let body = br#"{"title":"t"}"#.to_vec();
        let request = HttpRequest::post("/session", body.clone());
        let response = transport.request(&request).expect("request must succeed");
        assert_eq!(response.status(), 200);

        let raw = captured.lock().expect("capture mutex").clone();
        let text = String::from_utf8_lossy(&raw);
        assert!(text.starts_with("POST /session HTTP/1.1\r\n"));
        assert!(text.contains("content-type: application/json\r\n"));
        assert!(text.contains(&format!("content-length: {}\r\n", body.len())));
        assert!(raw.ends_with(&body));
    }

    #[test]
    fn large_post_to_stalled_peer_times_out_within_deadline() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .expect("server must bind an ephemeral port");
        let port = listener
            .local_addr()
            .expect("bound address must be available")
            .port();
        let handle = thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                // Never read; hold the connection until the client gives up.
                thread::sleep(Duration::from_millis(600));
                drop(stream);
            }
        });
        let (_dir, endpoint, secret) = project_material(port);
        let transport =
            HttpTransport::new(endpoint, BasicAuth::new(secret), Duration::from_millis(200));
        let body = vec![b'x'; 16 * 1024 * 1024];
        let started = Instant::now();
        let error = transport
            .request(&HttpRequest::post("/session", body))
            .expect_err("a stalled peer must not complete the request");
        assert_eq!(error, TransportError::Timeout);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the shared deadline must bound connect, partial writes and reads"
        );
        handle.join().expect("server thread must join");
    }

    #[test]
    fn large_post_to_slow_reader_completes_within_deadline() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .expect("server must bind an ephemeral port");
        let port = listener
            .local_addr()
            .expect("bound address must be available")
            .port();
        let handle = thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut data = Vec::new();
                let mut chunk = [0_u8; 16 * 1024];
                // Read the whole request and reply; a large POST must still
                // complete while the peer is making progress well inside the
                // client deadline.
                loop {
                    match stream.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(count) => {
                            data.extend_from_slice(&chunk[..count]);
                            if let Some(head_end) = find_subslice(&data, b"\r\n\r\n") {
                                let length = content_length(&data[..head_end]).unwrap_or(0);
                                if data.len() >= head_end + 4 + length {
                                    break;
                                }
                            }
                            thread::sleep(Duration::from_millis(1));
                        }
                        Err(_) => break,
                    }
                }
                let _ = stream.write_all(&ok_json(b"{}"));
                let _ = stream.flush();
            }
        });
        let (_dir, endpoint, secret) = project_material(port);
        let transport =
            HttpTransport::new(endpoint, BasicAuth::new(secret), Duration::from_secs(5));
        let body = vec![b'y'; 512 * 1024];
        let started = Instant::now();
        let response = transport
            .request(&HttpRequest::post("/session", body))
            .expect("request must succeed");
        assert_eq!(response.status(), 200);
        assert_eq!(response.body(), b"{}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the request must finish well inside its deadline"
        );
        handle.join().expect("server thread must join");
    }

    #[test]
    fn write_loop_stops_at_deadline_despite_progressing_partial_writes() {
        // Every partial write succeeds and returns immediately, but the fake
        // clock advances past the overall timeout. The loop must re-check the
        // remaining budget before *every* write and stop at the deadline,
        // instead of letting a sequence of individually quick partial writes
        // extend the request. A single `set_write_timeout` + `write_all` would
        // write the whole body and return `Ok`.
        let step = Duration::from_millis(100);
        let timeout = Duration::from_millis(250);
        let body = vec![b'x'; 64];
        let clock = std::cell::Cell::new(Duration::ZERO);
        let writes = std::cell::Cell::new(0_usize);
        let result = write_all_until(
            &body,
            || {
                let elapsed = clock.get();
                if elapsed >= timeout {
                    Err(TransportError::Timeout)
                } else {
                    Ok(timeout - elapsed)
                }
            },
            |_budget, data| {
                clock.set(clock.get() + step);
                writes.set(writes.get() + 1);
                Ok(1.min(data.len()))
            },
        );
        assert_eq!(result, Err(TransportError::Timeout));
        assert!(
            writes.get() >= 2,
            "the peer must have completed several partial writes before the deadline"
        );
        assert!(
            writes.get() < body.len(),
            "the loop must not write the whole body once the deadline has passed"
        );
        assert!(
            clock.get() <= timeout + step,
            "the loop must stop within one partial write of the overall deadline"
        );
    }

    #[test]
    fn timeout_maps_to_typed_error() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .expect("server must bind an ephemeral port");
        let port = listener
            .local_addr()
            .expect("bound address must be available")
            .port();
        let handle = thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                thread::sleep(Duration::from_millis(500));
                drop(stream);
            }
        });
        let (_dir, endpoint, secret) = project_material(port);
        let transport =
            HttpTransport::new(endpoint, BasicAuth::new(secret), Duration::from_millis(150));
        let error = transport
            .request(&HttpRequest::get("/global/health"))
            .expect_err("request must time out");
        assert_eq!(error, TransportError::Timeout);
        assert_eq!(error.to_string(), "OpenCode request timed out");
        handle.join().expect("server thread must join");
    }

    #[test]
    fn connection_failure_maps_to_unavailable() {
        let port = {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
                .expect("server must bind an ephemeral port");
            listener
                .local_addr()
                .expect("bound address must be available")
                .port()
        };
        let (_dir, endpoint, secret) = project_material(port);
        let transport =
            HttpTransport::new(endpoint, BasicAuth::new(secret), Duration::from_secs(2));
        let error = transport
            .request(&HttpRequest::get("/global/health"))
            .expect_err("request must fail");
        assert_eq!(error, TransportError::Unavailable);
        assert_eq!(error.to_string(), "OpenCode endpoint is unreachable");
    }

    #[test]
    fn non_success_statuses_map_to_typed_errors() {
        let cases = [
            (401_u16, TransportError::Unauthorized),
            (404, TransportError::NotFound),
            (400, TransportError::HttpStatus(400)),
            (403, TransportError::HttpStatus(403)),
            (500, TransportError::HttpStatus(500)),
            (503, TransportError::HttpStatus(503)),
            (304, TransportError::HttpStatus(304)),
        ];
        for (status, expected) in cases {
            let (port, _captured) = spawn_server(move |_request| error_response(status));
            let (_dir, endpoint, secret) = project_material(port);
            let transport =
                HttpTransport::new(endpoint, BasicAuth::new(secret), Duration::from_secs(5));
            let error = transport
                .request(&HttpRequest::get("/global/health"))
                .expect_err("request must fail");
            assert_eq!(error, expected, "status {status} must map to {expected:?}");
            assert_eq!(error.status(), expected.status());
            let rendered = format!("{error} {error:?}");
            assert!(!rendered.contains("do-not-leak-body"));
        }
    }

    #[test]
    fn chunked_response_is_decoded() {
        let (port, _captured) = spawn_server(|_request| {
            b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n".to_vec()
        });
        let (_dir, endpoint, secret) = project_material(port);
        let transport =
            HttpTransport::new(endpoint, BasicAuth::new(secret), Duration::from_secs(5));
        let response = transport
            .request(&HttpRequest::get("/global/health"))
            .expect("request must succeed");
        assert_eq!(response.body(), b"hello world");
    }

    #[test]
    fn body_without_length_is_read_to_eof() {
        let (port, _captured) = spawn_server(|_request| {
            b"HTTP/1.1 200 OK\r\nconnection: close\r\n\r\nstreamed".to_vec()
        });
        let (_dir, endpoint, secret) = project_material(port);
        let transport =
            HttpTransport::new(endpoint, BasicAuth::new(secret), Duration::from_secs(5));
        let response = transport
            .request(&HttpRequest::get("/global/health"))
            .expect("request must succeed");
        assert_eq!(response.body(), b"streamed");
    }

    #[test]
    fn empty_body_with_zero_length_is_supported() {
        let (port, _captured) = spawn_server(|_request| {
            b"HTTP/1.1 204 No Content\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_vec()
        });
        let (_dir, endpoint, secret) = project_material(port);
        let transport =
            HttpTransport::new(endpoint, BasicAuth::new(secret), Duration::from_secs(5));
        let response = transport
            .request(&HttpRequest::get("/global/health"))
            .expect("request must succeed");
        assert_eq!(response.status(), 204);
        assert!(response.body().is_empty());
    }

    #[test]
    fn no_content_204_without_length_returns_while_connection_is_open() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .expect("server must bind an ephemeral port");
        let port = listener
            .local_addr()
            .expect("bound address must be available")
            .port();
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let handle = thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let _ = stream.write_all(b"HTTP/1.1 204 No Content\r\nconnection: close\r\n\r\n");
                let _ = stream.flush();
                // Hold the connection open until the client has consumed the
                // response, so a body-framing bug cannot hide behind EOF.
                let _ = wait.recv_timeout(Duration::from_secs(2));
                drop(stream);
            }
        });
        let (_dir, endpoint, secret) = project_material(port);
        let transport =
            HttpTransport::new(endpoint, BasicAuth::new(secret), Duration::from_secs(5));
        let started = Instant::now();
        let response = transport
            .request(&HttpRequest::get("/global/health"))
            .expect("request must succeed");
        assert_eq!(response.status(), 204);
        assert!(response.body().is_empty());
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a 204 must not wait for the peer to close the connection"
        );
        let _ = release.send(());
        handle.join().expect("server thread must join");
    }

    #[test]
    fn malformed_status_line_maps_to_protocol() {
        let (port, _captured) = spawn_server(|_request| b"garbage\r\n\r\n".to_vec());
        let (_dir, endpoint, secret) = project_material(port);
        let transport =
            HttpTransport::new(endpoint, BasicAuth::new(secret), Duration::from_secs(5));
        let error = transport
            .request(&HttpRequest::get("/global/health"))
            .expect_err("request must fail");
        assert_eq!(error, TransportError::Protocol);
    }

    #[test]
    fn parse_status_accepts_supported_versions_and_three_digit_codes() {
        let cases: [(&[u8], u16); 4] = [
            (b"HTTP/1.1 200 OK", 200),
            (b"HTTP/1.0 404 Not Found", 404),
            (b"HTTP/1.1 204 No Content", 204),
            (b"HTTP/1.1 503 Service Unavailable", 503),
        ];
        for (line, expected) in cases {
            assert_eq!(
                parse_status(line),
                Ok(expected),
                "status line {line:?} must parse to {expected}"
            );
        }
    }

    #[test]
    fn parse_status_rejects_unsupported_versions_and_malformed_codes() {
        let cases: [&[u8]; 10] = [
            b"HTTP/garbage 200 OK",
            b"HTTP/2 200 OK",
            b"HTTP/1.1 0200 OK",
            b"HTTP/1.1 20 OK",
            b"HTTP/1.1 2000 OK",
            b"HTTP/1.1 abc OK",
            b"HTTP/1.1 99 OK",
            b"HTTP/1.1 600 OK",
            b"HTTP/1.1",
            b"",
        ];
        for line in cases {
            assert_eq!(
                parse_status(line),
                Err(TransportError::Protocol),
                "status line {line:?} must be rejected"
            );
        }
    }

    #[test]
    fn leading_zero_status_line_maps_to_protocol_end_to_end() {
        let (port, _captured) =
            spawn_server(|_request| b"HTTP/1.1 0200 OK\r\nconnection: close\r\n\r\n".to_vec());
        let (_dir, endpoint, secret) = project_material(port);
        let transport =
            HttpTransport::new(endpoint, BasicAuth::new(secret), Duration::from_secs(5));
        let error = transport
            .request(&HttpRequest::get("/global/health"))
            .expect_err("request must fail");
        assert_eq!(error, TransportError::Protocol);
    }

    #[test]
    fn malformed_request_path_is_rejected_without_leaking_it() {
        let (_dir, endpoint, secret) = project_material(4104);
        let transport =
            HttpTransport::new(endpoint, BasicAuth::new(secret), Duration::from_secs(5));
        for path in ["global/health", "/sp ace", "/inject\r\nx"] {
            let error = transport
                .request(&HttpRequest::get(path))
                .expect_err("request must be rejected");
            assert_eq!(error, TransportError::InvalidRequest);
            let rendered = format!("{error} {error:?}");
            assert!(!rendered.contains("global/health"));
            assert!(!rendered.contains("inject"));
            assert!(!rendered.contains(path));
        }
    }

    #[test]
    fn validate_path_accepts_only_complete_percent_triplets() {
        for path in [
            "/session/ses%2Fx",
            "/session/%25",
            "/session/%C3%A9",
            "/a%20b",
        ] {
            assert_eq!(
                validate_path(path),
                Ok(()),
                "path {path:?} must be accepted"
            );
        }
        for path in [
            "/session/%",
            "/session/%2",
            "/session/%GG",
            "/session/%2G",
            "/%",
            "/%G1",
        ] {
            assert_eq!(
                validate_path(path),
                Err(TransportError::InvalidRequest),
                "path {path:?} must be rejected"
            );
        }
        for path in ["/sp ace", "/x?y", "/x#y", "/x\ty", "/x\r\ny", "no-slash"] {
            assert_eq!(
                validate_path(path),
                Err(TransportError::InvalidRequest),
                "path {path:?} must stay rejected"
            );
        }
    }

    #[test]
    fn malformed_percent_escapes_are_rejected_before_sending() {
        let (port, captured) = spawn_server(|_request| ok_json(b"{}"));
        let (_dir, endpoint, secret) = project_material(port);
        let transport =
            HttpTransport::new(endpoint, BasicAuth::new(secret), Duration::from_secs(5));
        for path in [
            "/session/%",
            "/session/%2",
            "/session/%GG",
            "/session/%2G",
            "/session/a%",
        ] {
            let error = transport
                .request(&HttpRequest::get(path))
                .expect_err("malformed percent escape must be rejected");
            assert_eq!(error, TransportError::InvalidRequest, "path {path:?}");
        }
        assert!(
            captured.lock().expect("capture mutex").is_empty(),
            "a malformed percent escape must not open a connection"
        );
    }

    #[test]
    fn valid_percent_encoded_paths_reach_the_server() {
        let (port, captured) = spawn_server(|_request| ok_json(b"{}"));
        let (_dir, endpoint, secret) = project_material(port);
        let transport =
            HttpTransport::new(endpoint, BasicAuth::new(secret), Duration::from_secs(5));
        for path in ["/session/ses%2Fx", "/session/%25", "/session/%C3%A9"] {
            let response = transport
                .request(&HttpRequest::get(path))
                .expect("valid percent-encoded path must be sent");
            assert_eq!(response.status(), 200);
        }
        let raw = captured.lock().expect("capture mutex").clone();
        assert!(
            !raw.is_empty(),
            "valid percent-encoded paths must reach the server"
        );
    }

    #[test]
    fn zero_timeout_fails_closed() {
        let (_dir, endpoint, secret) = project_material(4105);
        let transport = HttpTransport::new(endpoint, BasicAuth::new(secret), Duration::ZERO);
        let error = transport
            .request(&HttpRequest::get("/global/health"))
            .expect_err("request must fail");
        assert_eq!(error, TransportError::Timeout);
    }

    #[test]
    fn debug_and_display_never_render_secrets_paths_or_bodies() {
        let (_dir, endpoint, secret) = project_material(4106);
        let auth = BasicAuth::new(secret);
        for rendered in [format!("{auth:?}"), auth.to_string()] {
            assert!(!rendered.contains(PASSWORD));
            assert!(!rendered.contains(PASSWORD_BASE64));
        }
        let transport = HttpTransport::new(endpoint, auth, Duration::from_secs(30));
        let request = HttpRequest::post("/session", b"secret-body".to_vec())
            .with_query("directory", "/secret/workspace");
        for rendered in [format!("{request:?}"), format!("{transport:?}")] {
            assert!(!rendered.contains(PASSWORD));
            assert!(!rendered.contains("secret-body"));
            assert!(!rendered.contains("/secret/workspace"));
            assert!(!rendered.contains("/session"));
        }
        let response = HttpResponse {
            status: 200,
            body: b"response-secret".to_vec(),
        };
        let rendered = format!("{response:?}");
        assert!(!rendered.contains("response-secret"));
        assert!(rendered.contains("200"));
    }

    #[test]
    fn debug_and_display_never_render_endpoint_details_on_error() {
        let (_dir, endpoint, secret) = project_material(4107);
        let transport =
            HttpTransport::new(endpoint, BasicAuth::new(secret), Duration::from_secs(5));
        let error = transport
            .request(&HttpRequest::get("/global/health"))
            .expect_err("request must fail");
        let rendered = format!("{error} {error:?}");
        assert!(!rendered.contains("127.0.0.1"));
        assert!(!rendered.contains("4107"));
        assert!(!rendered.contains(PASSWORD));
        assert!(!rendered.contains(PASSWORD_BASE64));
    }

    // --- 6.2 health and workspace identity ---------------------------------

    /// Returns the request line (method, target, version) of a raw request.
    fn request_target(request: &[u8]) -> String {
        let end = find_subslice(request, b"\r\n").unwrap_or(request.len());
        String::from_utf8_lossy(&request[..end]).into_owned()
    }

    /// Builds a `200 OK` JSON response from a typed JSON value.
    fn ok_json_value(value: &serde_json::Value) -> Vec<u8> {
        ok_json(value.to_string().as_bytes())
    }

    /// Builds a client bound to `workspace` against a freshly spawned server.
    fn client_for(port: u16, workspace: PathBuf) -> (TempDir, OpenCodeClient) {
        let (dir, endpoint, secret) = project_material(port);
        let transport =
            HttpTransport::new(endpoint, BasicAuth::new(secret), Duration::from_secs(5));
        (dir, OpenCodeClient::new(transport, workspace))
    }

    #[test]
    fn health_success_is_scoped_and_reports_healthy_version() {
        let (port, captured) =
            spawn_server(|_request| ok_json(br#"{"healthy":true,"version":"1.18.31"}"#));
        let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
        let health = client.health().expect("health must parse");
        assert!(health.healthy());
        assert_eq!(health.version(), Some("1.18.31"));
        assert_eq!(health.to_string(), "OpenCode health: healthy=true");

        let raw = captured.lock().expect("capture mutex").clone();
        let target = request_target(&raw);
        assert!(
            target.starts_with("GET /global/health?directory=%2Ftmp%2Fws HTTP/1.1"),
            "health must carry the workspace directory query, got {target}"
        );
    }

    #[test]
    fn health_is_false_for_missing_or_non_boolean_healthy() {
        let bodies: [&[u8]; 5] = [
            br#"{"healthy":false,"version":"1"}"#,
            br#"{}"#,
            br#"{"healthy":"true"}"#,
            br#"{"healthy":1}"#,
            br#"{"healthy":null}"#,
        ];
        for body in bodies {
            let (port, _captured) = spawn_server(move |_request| ok_json(body));
            let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
            let health = client.health().expect("a JSON object must parse");
            assert!(!health.healthy(), "body {body:?} must not be healthy");
        }
    }

    #[test]
    fn health_malformed_body_fails_closed() {
        let bodies: [&[u8]; 4] = [b"[]", b"\"x\"", b"null", b"not-json"];
        for body in bodies {
            let (port, _captured) = spawn_server(move |_request| ok_json(body));
            let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
            assert_eq!(
                client.health().expect_err("must be malformed"),
                HealthError::Malformed
            );
        }
    }

    #[test]
    fn health_transport_failures_are_preserved() {
        let cases = [
            (401_u16, TransportError::Unauthorized),
            (404, TransportError::NotFound),
            (500, TransportError::HttpStatus(500)),
        ];
        for (status, expected) in cases {
            let (port, _captured) = spawn_server(move |_request| error_response(status));
            let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
            assert_eq!(
                client.health().expect_err("must fail"),
                HealthError::Transport(expected)
            );
        }
    }

    #[test]
    fn health_timeout_and_unavailable_are_preserved() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .expect("server must bind an ephemeral port");
        let port = listener
            .local_addr()
            .expect("bound address must be available")
            .port();
        let handle = thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                thread::sleep(Duration::from_millis(500));
                drop(stream);
            }
        });
        let (_dir, endpoint, secret) = project_material(port);
        let transport =
            HttpTransport::new(endpoint, BasicAuth::new(secret), Duration::from_millis(150));
        let client = OpenCodeClient::new(transport, PathBuf::from("/tmp/ws"));
        assert_eq!(
            client.health().expect_err("must time out"),
            HealthError::Transport(TransportError::Timeout)
        );
        handle.join().expect("server thread must join");

        let port = {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
                .expect("server must bind an ephemeral port");
            listener
                .local_addr()
                .expect("bound address must be available")
                .port()
        };
        let (_dir, endpoint, secret) = project_material(port);
        let transport =
            HttpTransport::new(endpoint, BasicAuth::new(secret), Duration::from_secs(2));
        let client = OpenCodeClient::new(transport, PathBuf::from("/tmp/ws"));
        assert_eq!(
            client.health().expect_err("must be unavailable"),
            HealthError::Transport(TransportError::Unavailable)
        );
    }

    #[test]
    fn verify_workspace_succeeds_and_queries_path_without_directory() {
        let dir = TempDir::new("identity-ok");
        let workspace = std::fs::canonicalize(dir.path()).expect("workspace must canonicalize");
        let response = ok_json_value(&serde_json::json!({
            "directory": workspace.to_string_lossy(),
        }));
        let (port, captured) = spawn_server(move |_request| response.clone());
        let (_client_dir, client) = client_for(port, workspace);
        client.verify_workspace().expect("identity must hold");

        let raw = captured.lock().expect("capture mutex").clone();
        assert_eq!(
            request_target(&raw),
            "GET /path HTTP/1.1",
            "the server root must be queried without a directory context"
        );
    }

    #[test]
    fn verify_workspace_ignores_scoped_directory_echo() {
        let dir = TempDir::new("identity-scoped");
        let workspace = std::fs::canonicalize(dir.path()).expect("workspace must canonicalize");
        let echo = ok_json_value(&serde_json::json!({
            "directory": workspace.to_string_lossy(),
        }));
        let foreign = ok_json_value(&serde_json::json!({"directory": "/somewhere/else"}));
        let (port, captured) = spawn_server(move |request| {
            if request_target(request).contains("directory=") {
                echo.clone()
            } else {
                foreign.clone()
            }
        });
        let (_client_dir, client) = client_for(port, workspace);
        assert_eq!(
            client
                .verify_workspace()
                .expect_err("foreign root must fail"),
            IdentityError::Mismatch
        );
        let raw = captured.lock().expect("capture mutex").clone();
        assert!(
            !request_target(&raw).contains("directory="),
            "the identity probe must not send a directory context"
        );
    }

    #[test]
    fn verify_workspace_fails_closed_on_missing_or_bad_fields() {
        let cases: [(&[u8], IdentityError); 5] = [
            (br#"{}"#, IdentityError::MissingDirectory),
            (br#"{"directory":null}"#, IdentityError::Malformed),
            (br#"{"directory":123}"#, IdentityError::Malformed),
            (br#"[]"#, IdentityError::Malformed),
            (b"not-json", IdentityError::Malformed),
        ];
        for (body, expected) in cases {
            let (port, _captured) = spawn_server(move |_request| ok_json(body));
            let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
            assert_eq!(
                client.verify_workspace().expect_err("must fail closed"),
                expected
            );
        }
    }

    #[test]
    fn verify_workspace_mismatch_fails_closed() {
        let dir = TempDir::new("identity-mismatch");
        let workspace = std::fs::canonicalize(dir.path()).expect("workspace must canonicalize");
        let other = std::fs::canonicalize(dir.path().parent().expect("parent must exist"))
            .expect("parent must canonicalize");
        let response = ok_json_value(&serde_json::json!({
            "directory": other.to_string_lossy(),
        }));
        let (port, _captured) = spawn_server(move |_request| response.clone());
        let (_dir, client) = client_for(port, workspace);
        assert_eq!(
            client.verify_workspace().expect_err("must mismatch"),
            IdentityError::Mismatch
        );
    }

    #[test]
    fn verify_workspace_transport_failure_is_preserved() {
        let (port, _captured) = spawn_server(|_request| error_response(401));
        let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
        assert_eq!(
            client.verify_workspace().expect_err("must fail"),
            IdentityError::Transport(TransportError::Unauthorized)
        );
    }

    #[test]
    fn reported_path_resolution_matches_reference_semantics() {
        let dir = TempDir::new("resolve");
        let workspace = std::fs::canonicalize(dir.path()).expect("workspace must canonicalize");
        let ws = workspace.to_string_lossy().into_owned();

        assert!(reported_matches_workspace(&ws, &workspace));
        assert!(reported_matches_workspace(&format!("{ws}/"), &workspace));
        assert!(reported_matches_workspace(&format!("{ws}/."), &workspace));
        assert!(reported_matches_workspace(
            &format!("{ws}/missing/.."),
            &workspace
        ));

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let link = dir.path().join("alias");
            symlink(&workspace, &link).expect("symlink must be creatable");
            assert!(reported_matches_workspace(
                &link.to_string_lossy(),
                &workspace
            ));
        }

        assert!(!reported_matches_workspace("/somewhere/else", &workspace));
        assert!(!reported_matches_workspace(
            "/definitely/not/here",
            &workspace
        ));
        assert!(!reported_matches_workspace(
            &format!("{ws}/missing"),
            &workspace
        ));
    }

    #[test]
    fn reported_path_resolution_fails_closed_on_symlink_missing_tail() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let dir = TempDir::new("resolve-symlink");
            let workspace = std::fs::canonicalize(dir.path()).expect("workspace must canonicalize");
            let ws = workspace.to_string_lossy().into_owned();
            let foreign_dir = TempDir::new("resolve-foreign");
            let foreign =
                std::fs::canonicalize(foreign_dir.path()).expect("foreign must canonicalize");
            let sub = foreign.join("sub");
            std::fs::create_dir_all(&sub).expect("foreign subdir must be creatable");

            // Regression for the review's false positive: `alias` links outside
            // the workspace, then `missing/../..` folds back into the workspace
            // only when the symlink is ignored. The reference resolves the link
            // first, so the reported path is foreign and must not match.
            let alias = workspace.join("alias");
            symlink(&sub, &alias).expect("symlink must be creatable");
            assert!(
                !reported_matches_workspace(&format!("{ws}/alias/missing/../.."), &workspace),
                "a symlink escape followed by missing/../.. must not match"
            );

            // A `..` after a missing component must still follow a later
            // symlink: `missing/../link/..` is lexically the workspace, but the
            // reference resolves `link` outside and folds `..` there.
            let link = workspace.join("link");
            symlink(&foreign, &link).expect("symlink must be creatable");
            assert!(
                !reported_matches_workspace(&format!("{ws}/missing/../link/.."), &workspace),
                "a `..` after a missing component must not hide a symlink escape"
            );

            // The legitimate non-existent tail still resolves to the workspace.
            assert!(reported_matches_workspace(
                &format!("{ws}/missing/.."),
                &workspace
            ));
        }
    }

    #[test]
    fn reported_path_is_resolved_even_when_it_spells_the_workspace() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let root = TempDir::new("resolve-lexical");
            let workspace_path = root.path().join("ws");
            std::fs::create_dir_all(&workspace_path).expect("workspace must be creatable");
            let workspace =
                std::fs::canonicalize(&workspace_path).expect("workspace must canonicalize");
            let ws = workspace.to_string_lossy().into_owned();
            let foreign_dir = TempDir::new("resolve-lexical-foreign");
            let foreign =
                std::fs::canonicalize(foreign_dir.path()).expect("foreign must canonicalize");

            // Swap the workspace directory for a symlink to a foreign tree. The
            // reported string still equals the canonical workspace text, but a
            // lexical shortcut would accept it; the reference always resolves,
            // so the reported text must be rejected.
            std::fs::remove_dir_all(&workspace).expect("workspace must be removable");
            symlink(&foreign, &workspace).expect("symlink must be creatable");
            assert!(
                !reported_matches_workspace(&ws, &workspace),
                "string equality with the canonical workspace must not bypass resolution"
            );
        }
    }

    #[test]
    fn verify_workspace_rejects_symlink_escape_and_escaped_nul() {
        // A symlink escape followed by a missing tail, served through `GET
        // /path`, must not be accepted by a lexical alias.
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let dir = TempDir::new("identity-symlink-escape");
            let workspace = std::fs::canonicalize(dir.path()).expect("workspace must canonicalize");
            let foreign = dir.mkdir("foreign");
            let sub = foreign.join("sub");
            std::fs::create_dir_all(&sub).expect("foreign subdir must be creatable");
            symlink(&sub, workspace.join("alias")).expect("symlink must be creatable");
            let reported = format!("{}/alias/missing/../..", workspace.to_string_lossy());
            let response = ok_json_value(&serde_json::json!({ "directory": reported }));
            let (port, _captured) = spawn_server(move |_request| response.clone());
            let (_client_dir, client) = client_for(port, workspace);
            assert_eq!(
                client
                    .verify_workspace()
                    .expect_err("a symlink escape must fail closed"),
                IdentityError::Mismatch
            );
        }

        // An embedded NUL is an invalid path (`Path.resolve()` raises
        // `ValueError`) and must be rejected even though it lexically wraps the
        // workspace.
        let dir = TempDir::new("identity-nul");
        let workspace = std::fs::canonicalize(dir.path()).expect("workspace must canonicalize");
        let reported = format!("{}\u{0}/..", workspace.to_string_lossy());
        let response = ok_json_value(&serde_json::json!({ "directory": reported }));
        let (port, _captured) = spawn_server(move |_request| response.clone());
        let (_client_dir, client) = client_for(port, workspace);
        assert_eq!(
            client
                .verify_workspace()
                .expect_err("an embedded NUL must fail closed"),
            IdentityError::Mismatch
        );
    }

    #[test]
    fn client_from_project_binds_workspace_and_reuses_auth() {
        let dir = TempDir::new("client-project");
        let config_path = write_project(&dir, 4200, true);
        let config = bridge_config::load_config(&config_path).expect("config must load");
        let project = config.project("proj").expect("project must exist");
        let client = OpenCodeClient::from_project(project, Duration::from_secs(7))
            .expect("client must build");
        assert_eq!(client.workspace(), project.workspace());
        assert_eq!(client.timeout(), Duration::from_secs(7));
    }

    #[test]
    fn client_from_project_maps_missing_password_to_invalid_auth() {
        let dir = TempDir::new("client-no-password");
        let config_path = write_project(&dir, 4201, false);
        let config = bridge_config::load_config(&config_path).expect("config must load");
        let project = config.project("proj").expect("project must exist");
        let error = OpenCodeClient::from_project(project, Duration::from_secs(5))
            .expect_err("missing password must fail");
        assert_eq!(error, TransportError::InvalidAuth);
    }

    #[test]
    fn new_types_never_render_workspace_or_response_contents() {
        let dir = TempDir::new("redact");
        let workspace = std::fs::canonicalize(dir.path()).expect("workspace must canonicalize");
        let workspace_text = workspace.to_string_lossy().into_owned();
        let (port, _captured) =
            spawn_server(|_request| ok_json(br#"{"healthy":true,"version":"secret-version"}"#));
        let (_client_dir, client) = client_for(port, workspace);
        let health = client.health().expect("health must parse");

        for rendered in [format!("{health:?}"), health.to_string()] {
            assert!(!rendered.contains("secret-version"));
        }
        assert!(!format!("{client:?}").contains(&workspace_text));

        for error in [
            HealthError::Malformed,
            HealthError::Transport(TransportError::Unauthorized),
        ] {
            let rendered = format!("{error} {error:?}");
            assert!(!rendered.contains(&workspace_text));
            assert!(!rendered.contains(PASSWORD));
            assert!(!rendered.contains(PASSWORD_BASE64));
        }
        for error in [
            IdentityError::Mismatch,
            IdentityError::MissingDirectory,
            IdentityError::Malformed,
            IdentityError::Transport(TransportError::Unauthorized),
        ] {
            let rendered = format!("{error} {error:?}");
            assert!(!rendered.contains(&workspace_text));
            assert!(!rendered.contains(PASSWORD));
            assert!(!rendered.contains(PASSWORD_BASE64));
        }
    }

    /// A valid `/doc` fixture mirroring the reference `_valid_doc`.
    fn reference_doc() -> serde_json::Value {
        serde_json::json!({
            "openapi": "3.1.0",
            "paths": {
                "/global/health": {"get": {}},
                "/path": {"get": {}},
                "/session": {"get": {}, "post": {}},
                "/session/status": {"get": {}},
                "/session/{sessionID}": {"get": {}},
                "/session/{sessionID}/message": {"get": {}},
                "/session/{sessionID}/prompt_async": {
                    "post": {
                        "requestBody": {
                            "content": {
                                "application/json": {
                                    "schema": {
                                        "type": "object",
                                        "properties": {"messageID": {}, "parts": {}}
                                    }
                                }
                            }
                        }
                    }
                },
                "/permission": {"get": {}},
                "/permission/{requestID}/reply": {
                    "post": {
                        "requestBody": {
                            "content": {
                                "application/json": {
                                    "schema": {
                                        "type": "object",
                                        "properties": {
                                            "reply": {
                                                "type": "string",
                                                "enum": ["once", "always", "reject"]
                                            },
                                            "message": {"type": "string"}
                                        }
                                    }
                                }
                            }
                        }
                    }
                },
                "/question": {"get": {}}
            },
            "components": {
                "schemas": {
                    "Path": {"type": "object", "properties": {"directory": {"type": "string"}}},
                    "Session": {"type": "object", "properties": {"directory": {"type": "string"}}},
                    "AssistantMessage": {
                        "type": "object",
                        "properties": {
                            "parentID": {"type": "string"},
                            "finish": {"type": "string"},
                            "time": {
                                "type": "object",
                                "properties": {"created": {}, "completed": {}}
                            }
                        }
                    }
                }
            }
        })
    }

    /// Builds the reference `/doc` fixture as raw JSON text so the checker sees
    /// the exact JSON insertion order. The bare `/session/{}` spelling is
    /// replaced by two conflicting spellings placed in the requested order
    /// (`first` before `second`); `first`/`second` are raw JSON path entries.
    fn raw_reference_doc_with_conflicting_session_paths(first: &str, second: &str) -> String {
        let mut json = String::from(r#"{"openapi":"3.1.0","paths":{"#);
        json.push_str(r#""/global/health":{"get":{}},"#);
        json.push_str(r#""/path":{"get":{}},"#);
        json.push_str(r#""/session":{"get":{},"post":{}},"#);
        json.push_str(r#""/session/status":{"get":{}},"#);
        json.push_str(first);
        json.push_str(second);
        json.push_str(r#""/session/{sessionID}/message":{"get":{}},"#);
        json.push_str(
            r#""/session/{sessionID}/prompt_async":{"post":{"requestBody":{"content":{"application/json":{"schema":{"type":"object","properties":{"messageID":{},"parts":{}}}}}}}},"#,
        );
        json.push_str(r#""/permission":{"get":{}},"#);
        json.push_str(
            r#""/permission/{requestID}/reply":{"post":{"requestBody":{"content":{"application/json":{"schema":{"type":"object","properties":{"reply":{"type":"string","enum":["once","always","reject"]},"message":{"type":"string"}}}}}}}},"#,
        );
        json.push_str(r#""/question":{"get":{}}"#);
        json.push_str(r#"},"components":{"schemas":{"#);
        json.push_str(r#""Path":{"type":"object","properties":{"directory":{"type":"string"}}},"#);
        json.push_str(
            r#""Session":{"type":"object","properties":{"directory":{"type":"string"}}},"#,
        );
        json.push_str(
            r#""AssistantMessage":{"type":"object","properties":{"parentID":{"type":"string"},"finish":{"type":"string"},"time":{"type":"object","properties":{"created":{},"completed":{}}}}}"#,
        );
        json.push_str(r#"}}}"#);
        json
    }

    #[test]
    fn openapi_compatibility_accepts_reference_document() {
        let doc = reference_doc();
        assert!(openapi_problems(&doc, false).is_empty());
        // The reference fixture has no model property, so requiring one must
        // report exactly the missing model.
        let strict = openapi_problems(&doc, true);
        assert_eq!(
            strict,
            vec![CompatibilityProblem::PromptAsyncBodyMissingModel]
        );

        let mut with_model = reference_doc();
        with_model["paths"]["/session/{sessionID}/prompt_async"]["post"]["requestBody"]["content"]
            ["application/json"]["schema"]["properties"]["model"] = serde_json::json!({});
        assert!(openapi_problems(&with_model, true).is_empty());
        assert!(openapi_problems(&with_model, false).is_empty());
    }

    #[test]
    fn openapi_compatibility_reports_missing_paths_and_wrong_methods() {
        let mut doc = reference_doc();
        let paths = doc["paths"].as_object_mut().expect("paths object");
        paths.remove("/session/{sessionID}/message");
        paths["/session"] = serde_json::json!({"get": {}});
        paths["/global/health"] = serde_json::json!({});
        let problems = openapi_problems(&doc, false);
        assert!(problems.contains(&CompatibilityProblem::MissingPath("/session/{}/message")));
        assert!(problems.contains(&CompatibilityProblem::MissingOperation {
            method: HttpMethod::Post,
            path: "/session",
        }));
        assert!(problems.contains(&CompatibilityProblem::MissingOperation {
            method: HttpMethod::Get,
            path: "/global/health",
        }));

        let mut doc = reference_doc();
        doc["paths"]
            .as_object_mut()
            .expect("paths object")
            .remove("/question");
        assert!(
            openapi_problems(&doc, false).contains(&CompatibilityProblem::MissingPath("/question"))
        );
    }

    #[test]
    fn openapi_compatibility_normalizes_path_parameter_names() {
        let mut doc = reference_doc();
        let paths = doc["paths"].as_object_mut().expect("paths object");
        let session = paths.remove("/session/{sessionID}").expect("session path");
        paths.insert("/session/{id}".to_string(), session);
        let message = paths
            .remove("/session/{sessionID}/message")
            .expect("message path");
        paths.insert("/session/{id}/message".to_string(), message);
        let prompt = paths
            .remove("/session/{sessionID}/prompt_async")
            .expect("prompt path");
        paths.insert("/session/{id}/prompt_async".to_string(), prompt);
        let reply = paths
            .remove("/permission/{requestID}/reply")
            .expect("reply path");
        paths.insert("/permission/{permissionID}/reply".to_string(), reply);
        assert!(openapi_problems(&doc, false).is_empty());
    }

    #[test]
    fn openapi_compatibility_requires_schema_properties_and_time_completed() {
        let mut doc = reference_doc();
        let schemas = doc["components"]["schemas"]
            .as_object_mut()
            .expect("schemas");
        schemas["Session"]["properties"]
            .as_object_mut()
            .expect("session properties")
            .remove("directory");
        schemas["AssistantMessage"]["properties"]
            .as_object_mut()
            .expect("assistant properties")
            .remove("finish");
        schemas["AssistantMessage"]["properties"]["time"]["properties"]
            .as_object_mut()
            .expect("time properties")
            .remove("completed");
        let problems = openapi_problems(&doc, false);
        assert!(
            problems.contains(&CompatibilityProblem::MissingSchemaProperty {
                schema: "Session",
                property: "directory",
            })
        );
        assert!(
            problems.contains(&CompatibilityProblem::MissingSchemaProperty {
                schema: "AssistantMessage",
                property: "finish",
            })
        );
        assert!(problems.contains(&CompatibilityProblem::MissingAssistantTimeCompleted));
    }

    #[test]
    fn openapi_compatibility_time_properties_non_object_fails_closed() {
        // A single damaged nested field on an otherwise compatible document:
        // `AssistantMessage.time.properties` exists but is not an object. Every
        // wrong JSON type must be equivalent to an absent `completed` and fail
        // closed, even though the Python reference silently skips a non-dict
        // `properties`.
        for damaged in [
            serde_json::json!([]),
            serde_json::json!(null),
            serde_json::json!("not-an-object"),
            serde_json::json!(7),
            serde_json::json!(true),
        ] {
            let mut doc = reference_doc();
            doc["components"]["schemas"]["AssistantMessage"]["properties"]["time"]["properties"] =
                damaged.clone();
            let problems = openapi_problems(&doc, false);
            assert!(
                problems.contains(&CompatibilityProblem::MissingAssistantTimeCompleted),
                "non-object time.properties must fail closed: {damaged}"
            );
        }
    }

    #[test]
    fn openapi_compatibility_resolves_inline_and_chained_refs() {
        let mut doc = reference_doc();
        let schemas = doc["components"]["schemas"]
            .as_object_mut()
            .expect("schemas");
        schemas.insert(
            "PathBase".to_string(),
            serde_json::json!({"type": "object", "properties": {"directory": {"type": "string"}}}),
        );
        schemas.insert(
            "Path".to_string(),
            serde_json::json!({"$ref": "#/components/schemas/PathBase"}),
        );
        schemas.insert(
            "SessionBase".to_string(),
            serde_json::json!({"type": "object", "properties": {"directory": {}}}),
        );
        schemas.insert(
            "SessionAlias".to_string(),
            serde_json::json!({"$ref": "#/components/schemas/SessionBase"}),
        );
        schemas.insert(
            "Session".to_string(),
            serde_json::json!({"$ref": "#/components/schemas/SessionAlias"}),
        );
        schemas.insert(
            "TimeBase".to_string(),
            serde_json::json!({"type": "object", "properties": {"created": {}, "completed": {}}}),
        );
        schemas["AssistantMessage"]["properties"]["time"] =
            serde_json::json!({"$ref": "#/components/schemas/TimeBase"});
        assert!(openapi_problems(&doc, false).is_empty());
    }

    #[test]
    fn openapi_compatibility_cycles_and_broken_refs_do_not_prove_structure() {
        let mut doc = reference_doc();
        let schemas = doc["components"]["schemas"]
            .as_object_mut()
            .expect("schemas");
        schemas.insert(
            "Path".to_string(),
            serde_json::json!({"$ref": "#/components/schemas/PathLoop"}),
        );
        schemas.insert(
            "PathLoop".to_string(),
            serde_json::json!({"$ref": "#/components/schemas/Path"}),
        );
        schemas.insert(
            "Session".to_string(),
            serde_json::json!({"$ref": "#/components/schemas/Missing"}),
        );
        schemas.insert(
            "AssistantMessage".to_string(),
            serde_json::json!({"$ref": "external.json#/AssistantMessage"}),
        );
        let problems = openapi_problems(&doc, false);
        assert!(
            problems.contains(&CompatibilityProblem::MissingSchemaProperty {
                schema: "Path",
                property: "directory",
            })
        );
        assert!(
            problems.contains(&CompatibilityProblem::MissingSchemaProperty {
                schema: "Session",
                property: "directory",
            })
        );
        assert!(
            problems.contains(&CompatibilityProblem::MissingSchemaProperty {
                schema: "AssistantMessage",
                property: "parentID",
            })
        );
    }

    #[test]
    fn openapi_compatibility_unresolvable_ref_with_siblings_fails_closed() {
        // Each case is isolated: the mandatory `Path` schema is replaced by an
        // unresolvable `$ref` that still carries a `properties.directory`
        // sibling. Siblings must never prove the structure, so the checker must
        // report the missing property for every unresolvable reference kind.
        let cases = [
            // Cyclic self-reference with siblings.
            serde_json::json!({
                "$ref": "#/components/schemas/Path",
                "properties": {"directory": {"type": "string"}}
            }),
            // External reference with siblings.
            serde_json::json!({
                "$ref": "external.json#/Path",
                "properties": {"directory": {"type": "string"}}
            }),
            // Non-string `$ref` with siblings.
            serde_json::json!({
                "$ref": 42,
                "properties": {"directory": {"type": "string"}}
            }),
            // Broken local reference with siblings.
            serde_json::json!({
                "$ref": "#/components/schemas/Missing",
                "properties": {"directory": {"type": "string"}}
            }),
        ];
        for damaged in cases {
            let mut doc = reference_doc();
            doc["components"]["schemas"]["Path"] = damaged.clone();
            let problems = openapi_problems(&doc, false);
            assert!(
                problems.contains(&CompatibilityProblem::MissingSchemaProperty {
                    schema: "Path",
                    property: "directory",
                }),
                "an unresolvable ref with siblings must not prove Path.directory: {damaged}"
            );
        }
    }

    #[test]
    fn openapi_compatibility_unresolvable_refs_in_body_and_time_fail_closed() {
        // prompt_async body schema: external `$ref` plus sibling body properties.
        let mut doc = reference_doc();
        doc["paths"]["/session/{sessionID}/prompt_async"]["post"]["requestBody"]["content"]["application/json"]
            ["schema"] = serde_json::json!({
            "$ref": "external.json#/PromptBody",
            "properties": {"messageID": {}, "parts": {}}
        });
        let problems = openapi_problems(&doc, false);
        assert!(problems.contains(&CompatibilityProblem::PromptAsyncBodyMissingMessageId));
        assert!(problems.contains(&CompatibilityProblem::PromptAsyncBodyMissingParts));

        // permission reply body schema: non-string `$ref` plus sibling reply enum.
        let mut doc = reference_doc();
        doc["paths"]["/permission/{requestID}/reply"]["post"]["requestBody"]["content"]["application/json"]
            ["schema"] = serde_json::json!({
            "$ref": 42,
            "properties": {"reply": {"type": "string", "enum": ["once", "always", "reject"]}}
        });
        assert!(
            openapi_problems(&doc, false)
                .contains(&CompatibilityProblem::PermissionReplyBodyMissingReply)
        );

        // AssistantMessage.time schema: cyclic `$ref` plus sibling completed.
        let mut doc = reference_doc();
        doc["components"]["schemas"]["TimeLoop"] = serde_json::json!({
            "$ref": "#/components/schemas/TimeLoop",
            "properties": {"created": {}, "completed": {}}
        });
        doc["components"]["schemas"]["AssistantMessage"]["properties"]["time"] =
            serde_json::json!({"$ref": "#/components/schemas/TimeLoop"});
        assert!(
            openapi_problems(&doc, false)
                .contains(&CompatibilityProblem::MissingAssistantTimeCompleted)
        );
    }

    #[test]
    fn openapi_compatibility_normalized_path_conflicts_follow_insertion_order() {
        // `/session/{z}` first (empty) -> the reference `setdefault` keeps it and
        // reports the missing GET even though `/session/{a}` has it later.
        let z_first = raw_reference_doc_with_conflicting_session_paths(
            r#""/session/{z}": {},"#,
            r#""/session/{a}": {"get": {}},"#,
        );
        let doc = serde_json::from_str(&z_first).expect("raw doc must parse");
        assert!(
            openapi_problems(&doc, false).contains(&CompatibilityProblem::MissingOperation {
                method: HttpMethod::Get,
                path: "/session/{}",
            })
        );

        // `/session/{a}` first (with GET) -> the reference keeps it and the
        // document is compatible.
        let a_first = raw_reference_doc_with_conflicting_session_paths(
            r#""/session/{a}": {"get": {}},"#,
            r#""/session/{z}": {},"#,
        );
        let doc = serde_json::from_str(&a_first).expect("raw doc must parse");
        assert!(openapi_problems(&doc, false).is_empty());
    }

    #[test]
    fn openapi_compatibility_malformed_nested_containers_fail_closed() {
        let mut doc = reference_doc();
        doc["components"] = serde_json::json!(["not", "an", "object"]);
        assert!(!openapi_problems(&doc, false).is_empty());

        let mut doc = reference_doc();
        doc["components"]["schemas"]["Session"] =
            serde_json::json!({"properties": ["not", "an", "object"]});
        assert!(openapi_problems(&doc, false).contains(
            &CompatibilityProblem::MissingSchemaProperty {
                schema: "Session",
                property: "directory",
            }
        ));

        let mut doc = reference_doc();
        doc["components"]["schemas"]["AssistantMessage"]["properties"] =
            serde_json::json!(["not", "an", "object"]);
        assert!(!openapi_problems(&doc, false).is_empty());

        let mut doc = reference_doc();
        doc["paths"]["/session/{sessionID}/prompt_async"]["post"] =
            serde_json::json!("not-an-object");
        assert!(
            openapi_problems(&doc, false)
                .contains(&CompatibilityProblem::PromptAsyncMissingBodySchema)
        );

        let mut doc = reference_doc();
        doc["paths"]["/permission/{requestID}/reply"]["post"]["requestBody"]["content"]["application/json"]
            ["schema"]["properties"] = serde_json::json!("not-an-object");
        assert!(
            openapi_problems(&doc, false)
                .contains(&CompatibilityProblem::PermissionReplyBodyMissingReply)
        );
    }

    #[test]
    fn openapi_compatibility_non_object_and_empty_paths_fail_closed() {
        assert_eq!(
            openapi_problems(&serde_json::json!([]), false),
            vec![CompatibilityProblem::DocumentNotObject]
        );
        assert_eq!(
            openapi_problems(&serde_json::json!("x"), false),
            vec![CompatibilityProblem::DocumentNotObject]
        );
        let empty = openapi_problems(&serde_json::json!({}), false);
        assert!(empty.contains(&CompatibilityProblem::MissingOpenapiVersion));
        assert!(empty.contains(&CompatibilityProblem::MissingPaths));
        assert!(
            openapi_problems(&serde_json::json!({"openapi": "3.1.0", "paths": {}}), false)
                .contains(&CompatibilityProblem::MissingPaths)
        );
        assert!(
            openapi_problems(&serde_json::json!({"openapi": "3.1.0", "paths": []}), false)
                .contains(&CompatibilityProblem::MissingPaths)
        );
    }

    #[test]
    fn openapi_compatibility_prompt_body_and_conditional_model() {
        let mut doc = reference_doc();
        doc["paths"]["/session/{sessionID}/prompt_async"]["post"]
            .as_object_mut()
            .expect("prompt post")
            .remove("requestBody");
        assert!(
            openapi_problems(&doc, false)
                .contains(&CompatibilityProblem::PromptAsyncMissingBodySchema)
        );

        let mut doc = reference_doc();
        doc["paths"]["/session/{sessionID}/prompt_async"]["post"]["requestBody"]["content"]["application/json"]
            ["schema"]["properties"] = serde_json::json!({"parts": {}});
        let problems = openapi_problems(&doc, false);
        assert!(problems.contains(&CompatibilityProblem::PromptAsyncBodyMissingMessageId));
        assert!(!problems.contains(&CompatibilityProblem::PromptAsyncBodyMissingParts));

        let mut doc = reference_doc();
        doc["paths"]["/session/{sessionID}/prompt_async"]["post"]["requestBody"]["content"]["application/json"]
            ["schema"]["properties"] = serde_json::json!({"messageID": {}});
        assert!(
            openapi_problems(&doc, false)
                .contains(&CompatibilityProblem::PromptAsyncBodyMissingParts)
        );
    }

    #[test]
    fn openapi_compatibility_permission_reply_schema_and_enum() {
        let mut doc = reference_doc();
        doc["paths"]["/permission/{requestID}/reply"]["post"]
            .as_object_mut()
            .expect("reply post")
            .remove("requestBody");
        assert!(
            openapi_problems(&doc, false)
                .contains(&CompatibilityProblem::PermissionReplyMissingBodySchema)
        );

        let mut doc = reference_doc();
        doc["paths"]["/permission/{requestID}/reply"]["post"]["requestBody"]["content"]["application/json"]
            ["schema"]["properties"] = serde_json::json!({"message": {}});
        assert!(
            openapi_problems(&doc, false)
                .contains(&CompatibilityProblem::PermissionReplyBodyMissingReply)
        );

        let mut doc = reference_doc();
        doc["paths"]["/permission/{requestID}/reply"]["post"]["requestBody"]["content"]["application/json"]
            ["schema"]["properties"]["reply"] = serde_json::json!({"type": "string"});
        assert!(
            openapi_problems(&doc, false)
                .contains(&CompatibilityProblem::PermissionReplyMissingReplyEnum)
        );

        let mut doc = reference_doc();
        doc["paths"]["/permission/{requestID}/reply"]["post"]["requestBody"]["content"]["application/json"]
            ["schema"]["properties"]["reply"]["enum"] = serde_json::json!("once");
        assert!(
            openapi_problems(&doc, false)
                .contains(&CompatibilityProblem::PermissionReplyMissingReplyEnum)
        );

        let mut doc = reference_doc();
        doc["paths"]["/permission/{requestID}/reply"]["post"]["requestBody"]["content"]["application/json"]
            ["schema"]["properties"]["reply"]["enum"] = serde_json::json!(["always", "reject"]);
        assert!(openapi_problems(&doc, false).contains(
            &CompatibilityProblem::PermissionReplyEnumMissingValue("once")
        ));
    }

    #[test]
    fn openapi_compatibility_permission_reply_wrong_method_is_reported() {
        let mut doc = reference_doc();
        doc["paths"]["/permission/{requestID}/reply"]
            .as_object_mut()
            .expect("reply path")
            .remove("post");
        assert!(
            openapi_problems(&doc, false).contains(&CompatibilityProblem::MissingOperation {
                method: HttpMethod::Post,
                path: "/permission/{}/reply",
            })
        );
    }

    #[test]
    fn check_compatibility_gets_scoped_doc_and_accepts_reference_document() {
        let response = ok_json_value(&reference_doc());
        let (port, captured) = spawn_server(move |_request| response.clone());
        let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
        let compatibility = client.check_compatibility().expect("doc must parse");
        assert!(compatibility.is_compatible());
        assert!(compatibility.problems().is_empty());

        let raw = captured.lock().expect("capture mutex").clone();
        let target = request_target(&raw);
        assert!(
            target.starts_with("GET /doc?directory=%2Ftmp%2Fws HTTP/1.1"),
            "doc must carry the workspace directory query, got {target}"
        );
    }

    #[test]
    fn check_compatibility_reports_incompatible_document() {
        let mut doc = reference_doc();
        doc["paths"]
            .as_object_mut()
            .expect("paths")
            .remove("/question");
        let response = ok_json_value(&doc);
        let (port, _captured) = spawn_server(move |_request| response.clone());
        let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
        let compatibility = client.check_compatibility().expect("doc must parse");
        assert!(!compatibility.is_compatible());
        assert!(
            compatibility
                .problems()
                .contains(&CompatibilityProblem::MissingPath("/question"))
        );
    }

    #[test]
    fn check_compatibility_malformed_and_non_object_fail_closed() {
        let (port, _captured) = spawn_server(|_request| ok_json(b"not-json"));
        let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
        assert_eq!(
            client.check_compatibility().expect_err("must be malformed"),
            DocError::Malformed
        );

        let (port, _captured) = spawn_server(|_request| ok_json(b"[]"));
        let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
        let compatibility = client
            .check_compatibility()
            .expect("a JSON array must parse");
        assert!(!compatibility.is_compatible());
        assert_eq!(
            compatibility.problems(),
            &[CompatibilityProblem::DocumentNotObject]
        );
    }

    #[test]
    fn check_compatibility_raw_json_path_conflicts_follow_insertion_order() {
        // The endpoint serves the raw JSON bytes, so this exercises parsing,
        // not a pre-built `serde_json::Value`. `/session/{z}` first must yield
        // the missing GET (reference first-wins); `/session/{a}` first must be
        // compatible. Both orders are proven end to end.
        let z_first = raw_reference_doc_with_conflicting_session_paths(
            r#""/session/{z}": {},"#,
            r#""/session/{a}": {"get": {}},"#,
        );
        let body = z_first.into_bytes();
        let (port, _captured) = spawn_server(move |_request| ok_json(&body));
        let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
        let compatibility = client.check_compatibility().expect("doc must parse");
        assert!(
            compatibility
                .problems()
                .contains(&CompatibilityProblem::MissingOperation {
                    method: HttpMethod::Get,
                    path: "/session/{}",
                })
        );

        let a_first = raw_reference_doc_with_conflicting_session_paths(
            r#""/session/{a}": {"get": {}},"#,
            r#""/session/{z}": {},"#,
        );
        let body = a_first.into_bytes();
        let (port, _captured) = spawn_server(move |_request| ok_json(&body));
        let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
        let compatibility = client.check_compatibility().expect("doc must parse");
        assert!(compatibility.is_compatible());
    }

    #[test]
    fn check_compatibility_preserves_transport_errors() {
        let cases = [
            (401_u16, TransportError::Unauthorized),
            (404, TransportError::NotFound),
            (500, TransportError::HttpStatus(500)),
        ];
        for (status, expected) in cases {
            let (port, _captured) = spawn_server(move |_request| error_response(status));
            let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
            assert_eq!(
                client.check_compatibility().expect_err("must fail"),
                DocError::Transport(expected)
            );
        }
    }

    #[test]
    fn from_project_applies_prompt_model_requirement() {
        let dir = TempDir::new("doc-model-off");
        let config_path = write_project(&dir, 4300, true);
        let config = bridge_config::load_config(&config_path).expect("config must load");
        let project = config.project("proj").expect("project must exist");
        let client = OpenCodeClient::from_project(project, Duration::from_secs(5))
            .expect("client must build");
        assert!(!client.require_prompt_model());

        let dir = TempDir::new("doc-model-on");
        dir.mkdir("ws");
        dir.mkdir("secrets");
        let password = dir.path().join("secrets/proj.password");
        std::fs::write(&password, format!("{PASSWORD}\n")).expect("password must be writable");
        set_private(&password);
        let config = "[projects.proj]\nworkspace = \"ws\"\nopencode_url = \"http://127.0.0.1:4301\"\npassword_file = \"secrets/proj.password\"\nopencode_model = \"anthropic/claude-sonnet\"\nmax_rounds = 3\n";
        let config_path = dir.path().join("projects.toml");
        std::fs::write(&config_path, config).expect("config must be writable");
        let config = bridge_config::load_config(&config_path).expect("config must load");
        let project = config.project("proj").expect("project must exist");
        let client = OpenCodeClient::from_project(project, Duration::from_secs(5))
            .expect("client must build");
        assert!(client.require_prompt_model());
    }

    #[test]
    fn require_prompt_model_flag_rejects_document_without_model() {
        let response = ok_json_value(&reference_doc());
        let (port, _captured) = spawn_server(move |_request| response.clone());
        let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
        assert!(client.check_compatibility().expect("parse").is_compatible());

        let strict = client.with_require_prompt_model(true);
        let compatibility = strict.check_compatibility().expect("parse");
        assert!(!compatibility.is_compatible());
        assert!(
            compatibility
                .problems()
                .contains(&CompatibilityProblem::PromptAsyncBodyMissingModel)
        );
    }

    #[test]
    fn doc_types_never_render_workspace_or_document_content() {
        let dir = TempDir::new("doc-redact");
        let workspace = std::fs::canonicalize(dir.path()).expect("workspace must canonicalize");
        let workspace_text = workspace.to_string_lossy().into_owned();
        let mut doc = reference_doc();
        doc["paths"]["/secret-route/{secretID}"] = serde_json::json!({"get": {}});
        doc["paths"]
            .as_object_mut()
            .expect("paths")
            .remove("/question");
        doc["components"]["schemas"]["SecretSchema"] =
            serde_json::json!({"properties": {"secret": {}}});
        let response = ok_json_value(&doc);
        let (port, _captured) = spawn_server(move |_request| response.clone());
        let (_client_dir, client) = client_for(port, workspace);
        let compatibility = client.check_compatibility().expect("doc must parse");
        assert!(!compatibility.is_compatible());

        let rendered = format!("{compatibility:?} {compatibility}");
        assert!(!rendered.contains("secret-route"));
        assert!(!rendered.contains("SecretSchema"));
        assert!(!rendered.contains(&workspace_text));
        for problem in compatibility.problems() {
            let text = format!("{problem} {problem:?}");
            assert!(!text.contains("secret-route"));
            assert!(!text.contains("SecretSchema"));
        }
        for error in [
            DocError::Malformed,
            DocError::Transport(TransportError::Unauthorized),
        ] {
            let rendered = format!("{error} {error:?}");
            assert!(!rendered.contains(&workspace_text));
            assert!(!rendered.contains(PASSWORD));
            assert!(!rendered.contains(PASSWORD_BASE64));
        }
        assert!(!format!("{client:?}").contains(&workspace_text));
    }

    // --- 6.4 session create/list/get ---------------------------------------

    #[test]
    fn list_sessions_success_is_scoped_and_parses_sessions() {
        let body = serde_json::json!([
            {"id": "ses_one", "title": "first", "directory": "/tmp/ws"},
            {"id": "ses_two"}
        ]);
        let response = ok_json_value(&body);
        let (port, captured) = spawn_server(move |_request| response.clone());
        let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
        let sessions = client.list_sessions().expect("list must parse");
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].id(), Some("ses_one"));
        assert_eq!(sessions[0].title(), Some("first"));
        assert_eq!(sessions[0].directory(), Some("/tmp/ws"));
        assert_eq!(sessions[1].id(), Some("ses_two"));
        assert_eq!(sessions[1].title(), None);
        assert_eq!(sessions[1].directory(), None);

        let raw = captured.lock().expect("capture mutex").clone();
        assert_eq!(
            request_target(&raw),
            "GET /session?directory=%2Ftmp%2Fws HTTP/1.1"
        );
        let text = String::from_utf8_lossy(&raw);
        assert!(text.contains(&format!("authorization: Basic {PASSWORD_BASE64}\r\n")));
        assert!(text.contains("accept: application/json\r\n"));
        assert!(!text.contains("content-type"));
    }

    #[test]
    fn list_sessions_skips_non_object_entries_like_reference() {
        let body = serde_json::json!([1, "x", null, {"id": "ses_ok"}, []]);
        let response = ok_json_value(&body);
        let (port, _captured) = spawn_server(move |_request| response.clone());
        let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
        let sessions = client.list_sessions().expect("list must parse");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id(), Some("ses_ok"));
    }

    #[test]
    fn create_session_posts_scoped_title_body_and_parses_session() {
        let response = ok_json_value(&serde_json::json!({
            "id": "ses_new",
            "title": "hello",
            "directory": "/tmp/ws"
        }));
        let (port, captured) = spawn_server(move |_request| response.clone());
        let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
        let session = client.create_session("hello").expect("create must parse");
        assert_eq!(session.id(), Some("ses_new"));
        assert_eq!(session.title(), Some("hello"));
        assert_eq!(session.directory(), Some("/tmp/ws"));

        let raw = captured.lock().expect("capture mutex").clone();
        let text = String::from_utf8_lossy(&raw);
        assert!(text.starts_with("POST /session?directory=%2Ftmp%2Fws HTTP/1.1\r\n"));
        assert!(text.contains("content-type: application/json\r\n"));
        assert!(text.contains(&format!("authorization: Basic {PASSWORD_BASE64}\r\n")));
        let body = br#"{"title":"hello"}"#;
        assert!(text.contains(&format!("content-length: {}\r\n", body.len())));
        assert!(raw.ends_with(body));
    }

    #[test]
    fn create_session_body_is_compact_utf8_like_reference() {
        let response = ok_json_value(&serde_json::json!({"id": "ses_u"}));
        let (port, captured) = spawn_server(move |_request| response.clone());
        let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
        client.create_session("héllo").expect("create must parse");
        let raw = captured.lock().expect("capture mutex").clone();
        assert!(
            raw.ends_with("{\"title\":\"héllo\"}".as_bytes()),
            "the body must be compact UTF-8 JSON without ASCII escaping"
        );
    }

    #[test]
    fn get_session_percent_encodes_id_and_scopes_request() {
        let response = ok_json_value(&serde_json::json!({
            "id": "ses/x y?z#w",
            "directory": "/tmp/ws"
        }));
        let (port, captured) = spawn_server(move |_request| response.clone());
        let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
        let session = client.get_session("ses/x y?z#w").expect("get must parse");
        assert_eq!(session.id(), Some("ses/x y?z#w"));

        let raw = captured.lock().expect("capture mutex").clone();
        assert_eq!(
            request_target(&raw),
            "GET /session/ses%2Fx%20y%3Fz%23w?directory=%2Ftmp%2Fws HTTP/1.1"
        );
    }

    #[test]
    fn get_session_keeps_plain_id_verbatim() {
        let response = ok_json_value(&serde_json::json!({"id": "ses_abc"}));
        let (port, captured) = spawn_server(move |_request| response.clone());
        let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
        client.get_session("ses_abc").expect("get must parse");
        let raw = captured.lock().expect("capture mutex").clone();
        assert_eq!(
            request_target(&raw),
            "GET /session/ses_abc?directory=%2Ftmp%2Fws HTTP/1.1"
        );
    }

    #[test]
    fn get_session_encodes_literal_percent_and_unicode() {
        let response = ok_json_value(&serde_json::json!({"id": "100%"}));
        let (port, captured) = spawn_server(move |_request| response.clone());
        let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
        client.get_session("100%").expect("get must parse");
        client
            .get_session("héllo/世界")
            .expect("get must parse unicode id");
        let raw = captured.lock().expect("capture mutex").clone();
        let text = String::from_utf8_lossy(&raw);
        assert!(
            text.contains("GET /session/100%25?directory=%2Ftmp%2Fws HTTP/1.1"),
            "a literal percent must be encoded, not forwarded: {text}"
        );
        assert!(
            text.contains(
                "GET /session/h%C3%A9llo%2F%E4%B8%96%E7%95%8C?directory=%2Ftmp%2Fws HTTP/1.1"
            ),
            "unicode must be UTF-8 percent-encoded: {text}"
        );
    }

    #[test]
    fn get_session_rejects_unusable_id_without_sending() {
        let (port, captured) = spawn_server(|_request| ok_json(b"{}"));
        let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
        for id in ["", ".", ".."] {
            assert_eq!(
                client.get_session(id).expect_err("id must be rejected"),
                SessionError::InvalidSessionId,
                "id {id:?} must fail closed"
            );
        }
        assert!(
            captured.lock().expect("capture mutex").is_empty(),
            "an unusable id must not produce a request"
        );
    }

    #[test]
    fn session_requests_preserve_transport_errors() {
        let cases = [
            (401_u16, TransportError::Unauthorized),
            (404, TransportError::NotFound),
            (500, TransportError::HttpStatus(500)),
        ];
        for (status, expected) in cases {
            let (port, _captured) = spawn_server(move |_request| error_response(status));
            let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
            assert_eq!(
                client.list_sessions().expect_err("list must fail"),
                SessionError::Transport(expected)
            );
            assert_eq!(
                client.create_session("t").expect_err("create must fail"),
                SessionError::Transport(expected)
            );
            assert_eq!(
                client.get_session("ses_x").expect_err("get must fail"),
                SessionError::Transport(expected)
            );
        }
    }

    #[test]
    fn list_sessions_malformed_response_fails_closed() {
        for body in [&b"{}"[..], b"\"x\"", b"null", b"not-json"] {
            let owned = body.to_vec();
            let (port, _captured) = spawn_server(move |_request| ok_json(&owned));
            let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
            assert_eq!(
                client.list_sessions().expect_err("list must be malformed"),
                SessionError::Malformed,
                "list body {body:?}"
            );
        }
    }

    #[test]
    fn create_and_get_session_malformed_response_fails_closed() {
        for body in [&b"[]"[..], b"\"x\"", b"null", b"not-json"] {
            let owned = body.to_vec();
            let (port, _captured) = spawn_server(move |_request| ok_json(&owned));
            let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
            assert_eq!(
                client
                    .create_session("t")
                    .expect_err("create must be malformed"),
                SessionError::Malformed,
                "create body {body:?}"
            );
            assert_eq!(
                client
                    .get_session("ses_x")
                    .expect_err("get must be malformed"),
                SessionError::Malformed,
                "get body {body:?}"
            );
        }
    }

    #[test]
    fn session_types_never_render_content_workspace_or_credentials() {
        let dir = TempDir::new("session-redact");
        let workspace = std::fs::canonicalize(dir.path()).expect("workspace must canonicalize");
        let workspace_text = workspace.to_string_lossy().into_owned();
        let secret_id = "ses_secret-id";
        let secret_title = "secret-title";
        let secret_directory = "/secret/session/dir";
        let response = ok_json_value(&serde_json::json!({
            "id": secret_id,
            "title": secret_title,
            "directory": secret_directory
        }));
        let (port, _captured) = spawn_server(move |_request| response.clone());
        let (_client_dir, client) = client_for(port, workspace);
        let session = client.get_session(secret_id).expect("get must parse");

        for rendered in [format!("{session:?}"), session.to_string()] {
            assert!(!rendered.contains(secret_id));
            assert!(!rendered.contains(secret_title));
            assert!(!rendered.contains(secret_directory));
            assert!(!rendered.contains(&workspace_text));
        }
        for error in [
            SessionError::Malformed,
            SessionError::InvalidSessionId,
            SessionError::Transport(TransportError::Unauthorized),
        ] {
            let rendered = format!("{error} {error:?}");
            assert!(!rendered.contains(secret_id));
            assert!(!rendered.contains(secret_title));
            assert!(!rendered.contains(secret_directory));
            assert!(!rendered.contains(&workspace_text));
            assert!(!rendered.contains(PASSWORD));
            assert!(!rendered.contains(PASSWORD_BASE64));
        }
        assert!(!format!("{client:?}").contains(&workspace_text));
    }

    // --- 6.5 message list and parsing --------------------------------------

    /// Builds a reference-compatible `list_messages` fixture: one user turn and
    /// one completed assistant turn with two text parts.
    fn reference_message_fixture() -> serde_json::Value {
        serde_json::json!([
            {
                "info": {
                    "id": "msg_user",
                    "sessionID": "ses_x",
                    "role": "user",
                    "time": {"created": 1},
                    "agent": "build",
                    "model": {"providerID": "p", "modelID": "m"}
                },
                "parts": [{"type": "text", "text": "task"}]
            },
            {
                "info": {
                    "id": "msg_asst",
                    "sessionID": "ses_x",
                    "role": "assistant",
                    "time": {"created": 1, "completed": 2},
                    "parentID": "msg_user",
                    "cost": 0.25,
                    "tokens": {
                        "input": 10,
                        "output": 5,
                        "reasoning": 2,
                        "cache": {"read": 3, "write": 4}
                    },
                    "providerID": "p",
                    "modelID": "m",
                    "finish": "stop"
                },
                "parts": [
                    {"type": "text", "text": "hello", "messageID": "msg_asst"},
                    {"type": "text", "text": "world", "messageID": "msg_asst"}
                ]
            }
        ])
    }

    #[test]
    fn list_messages_success_is_scoped_and_parses_reference_fixture() {
        let response = ok_json_value(&reference_message_fixture());
        let (port, captured) = spawn_server(move |_request| response.clone());
        let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
        let messages = client.list_messages("ses_x").expect("list must parse");
        assert_eq!(messages.len(), 2);

        let user = &messages[0];
        assert!(user.is_user());
        assert!(!user.is_assistant());
        assert_eq!(user.info().id(), Some("msg_user"));
        assert_eq!(user.info().role(), Some("user"));
        assert_eq!(user.info().session_id(), Some("ses_x"));
        assert_eq!(user.info().parent_id(), None);
        assert!(!user.is_completed());
        assert!(!user.has_error());
        assert_eq!(user.text(), "task");
        assert_eq!(user.parts().len(), 1);
        assert!(user.parts()[0].is_text());
        assert_eq!(user.parts()[0].text(), Some("task"));
        assert!(!user.has_pending_tool_parts());

        let assistant = &messages[1];
        assert!(assistant.is_assistant());
        assert_eq!(assistant.info().id(), Some("msg_asst"));
        assert_eq!(assistant.info().parent_id(), Some("msg_user"));
        assert!(assistant.is_completed());
        assert_eq!(assistant.info().finish(), Some("stop"));
        assert_eq!(assistant.info().model(), Some(("p", "m")));
        assert!(!assistant.has_error());
        assert_eq!(assistant.text(), "hello\nworld");
        let usage = assistant.info().usage();
        assert_eq!(usage.input(), 10.0);
        assert_eq!(usage.output(), 5.0);
        assert_eq!(usage.reasoning(), 2.0);
        assert_eq!(usage.cache_read(), 3.0);
        assert_eq!(usage.cache_write(), 4.0);
        assert_eq!(usage.cost(), 0.25);

        let raw = captured.lock().expect("capture mutex").clone();
        assert_eq!(
            request_target(&raw),
            "GET /session/ses_x/message?directory=%2Ftmp%2Fws HTTP/1.1"
        );
        let text = String::from_utf8_lossy(&raw);
        assert!(text.contains(&format!("authorization: Basic {PASSWORD_BASE64}\r\n")));
        assert!(text.contains("accept: application/json\r\n"));
        assert!(!text.contains("content-type"));
    }

    #[test]
    fn list_messages_percent_encodes_id_and_scopes_request() {
        let response = ok_json_value(&serde_json::json!([]));
        let (port, captured) = spawn_server(move |_request| response.clone());
        let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
        client
            .list_messages("ses/x y?z#w")
            .expect("encoded id must parse");
        let raw = captured.lock().expect("capture mutex").clone();
        assert_eq!(
            request_target(&raw),
            "GET /session/ses%2Fx%20y%3Fz%23w/message?directory=%2Ftmp%2Fws HTTP/1.1"
        );
    }

    #[test]
    fn list_messages_rejects_unusable_id_without_sending() {
        let (port, captured) = spawn_server(|_request| ok_json(b"[]"));
        let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
        for id in ["", ".", ".."] {
            assert_eq!(
                client.list_messages(id).expect_err("id must be rejected"),
                MessageError::InvalidSessionId,
                "id {id:?} must fail closed"
            );
        }
        assert!(
            captured.lock().expect("capture mutex").is_empty(),
            "an unusable id must not produce a request"
        );
    }

    #[test]
    fn list_messages_preserves_transport_errors() {
        let cases = [
            (401_u16, TransportError::Unauthorized),
            (404, TransportError::NotFound),
            (500, TransportError::HttpStatus(500)),
        ];
        for (status, expected) in cases {
            let (port, _captured) = spawn_server(move |_request| error_response(status));
            let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
            assert_eq!(
                client.list_messages("ses_x").expect_err("list must fail"),
                MessageError::Transport(expected)
            );
        }
    }

    #[test]
    fn list_messages_preserves_unavailable_transport_error() {
        let port = {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
                .expect("server must bind an ephemeral port");
            listener
                .local_addr()
                .expect("bound address must be available")
                .port()
        };
        let (_dir, endpoint, secret) = project_material(port);
        let transport =
            HttpTransport::new(endpoint, BasicAuth::new(secret), Duration::from_secs(2));
        let client = OpenCodeClient::new(transport, PathBuf::from("/tmp/ws"));
        assert_eq!(
            client.list_messages("ses_x").expect_err("list must fail"),
            MessageError::Transport(TransportError::Unavailable)
        );
    }

    #[test]
    fn list_messages_malformed_response_fails_closed() {
        for body in [&b"{}"[..], b"\"x\"", b"null", b"not-json", b"1"] {
            let owned = body.to_vec();
            let (port, _captured) = spawn_server(move |_request| ok_json(&owned));
            let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
            assert_eq!(
                client
                    .list_messages("ses_x")
                    .expect_err("list must be malformed"),
                MessageError::Malformed,
                "list body {body:?}"
            );
        }
    }

    #[test]
    fn list_messages_fails_closed_on_malformed_entries_and_parts() {
        for body in [
            serde_json::json!([1]),
            serde_json::json!(["x"]),
            serde_json::json!([null]),
            serde_json::json!([[]]),
            serde_json::json!([{"info": "not-an-object"}]),
            serde_json::json!([{"info": {"role": "assistant"}, "parts": "not-an-array"}]),
            serde_json::json!([{"info": {"role": "assistant"}, "parts": [1]}]),
            serde_json::json!([{"info": {"role": "assistant"}, "parts": ["x"]}]),
            serde_json::json!([{"info": {"role": "assistant"}, "parts": [null]}]),
        ] {
            let response = ok_json_value(&body);
            let (port, _captured) = spawn_server(move |_request| response.clone());
            let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
            assert_eq!(
                client
                    .list_messages("ses_x")
                    .expect_err("malformed list must fail closed"),
                MessageError::Malformed,
                "body {body:?}"
            );
        }
    }

    #[test]
    fn list_messages_malformed_parts_never_look_completed() {
        // A completed assistant whose `parts` is damaged must be rejected
        // outright rather than parsed into a final, tool-free turn.
        for parts in [
            serde_json::json!("broken"),
            serde_json::json!([null]),
            serde_json::json!([{"type": "tool", "state": "completed"}]),
            serde_json::json!([{"type": "tool", "metadata": "providerExecuted"}]),
            serde_json::json!([{"type": "text", "text": 5}]),
        ] {
            let body = serde_json::json!([{
                "info": {"role": "assistant", "time": {"completed": 1}, "finish": "stop"},
                "parts": parts
            }]);
            let response = ok_json_value(&body);
            let (port, _captured) = spawn_server(move |_request| response.clone());
            let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
            assert_eq!(
                client
                    .list_messages("ses_x")
                    .expect_err("damaged completed turn must fail closed"),
                MessageError::Malformed,
                "parts {parts:?}"
            );
        }
    }

    #[test]
    fn list_messages_malformed_trailing_entry_is_not_hidden() {
        // An older completed assistant must not remain the last message when a
        // later unfinished entry is malformed.
        let body = serde_json::json!([
            {
                "info": {"role": "assistant", "time": {"completed": 1}, "finish": "stop"},
                "parts": [{"type": "text", "text": "done"}]
            },
            {"info": {"role": "assistant"}, "parts": [null]}
        ]);
        let response = ok_json_value(&body);
        let (port, _captured) = spawn_server(move |_request| response.clone());
        let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
        assert_eq!(
            client
                .list_messages("ses_x")
                .expect_err("malformed trailing entry must fail closed"),
            MessageError::Malformed
        );
    }

    #[test]
    fn message_info_lifecycle_and_error_match_reference() {
        let body = serde_json::json!([
            {"info": {"role": "assistant", "time": {"completed": null}, "finish": "stop"}},
            {"info": {"role": "assistant", "time": {"completed": 0}}},
            {"info": {"role": "assistant", "time": {}}},
            {"info": {"role": "assistant"}},
            {"info": {"role": "assistant", "error": {}}},
            {"info": {"role": "assistant", "error": ""}},
            {"info": {"role": "assistant", "error": 0}},
            {"info": {"role": "assistant", "error": "boom"}},
            {"info": {"role": "assistant", "error": {"message": "x"}}}
        ]);
        let response = ok_json_value(&body);
        let (port, _captured) = spawn_server(move |_request| response.clone());
        let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
        let messages = client.list_messages("ses_x").expect("list must parse");

        assert!(!messages[0].is_completed());
        assert_eq!(messages[0].info().finish(), Some("stop"));
        assert!(messages[1].is_completed());
        assert!(!messages[2].is_completed());
        assert!(!messages[3].is_completed());

        assert!(!messages[4].has_error());
        assert!(!messages[5].has_error());
        assert!(!messages[6].has_error());
        assert!(messages[7].has_error());
        assert_eq!(messages[7].info().error(), Some(&serde_json::json!("boom")));
        assert!(messages[8].has_error());
        assert_eq!(
            messages[8].info().error(),
            Some(&serde_json::json!({"message": "x"}))
        );
    }

    #[test]
    fn message_malformed_lifecycle_containers_fail_closed() {
        // A present truthy `time`/`state`/`metadata` of the wrong type would
        // make the reference raise; it must be Malformed, not a permissive
        // default that could look completed or tool-free.
        for body in [
            serde_json::json!([{"info": {"role": "assistant", "time": "not-an-object"}}]),
            serde_json::json!([{"info": {"role": "assistant", "time": 1}}]),
            serde_json::json!([{"info": {"role": "assistant", "time": [1]}}]),
            serde_json::json!([{
                "info": {"role": "assistant"},
                "parts": [{"type": "tool", "state": "completed"}]
            }]),
            serde_json::json!([{
                "info": {"role": "assistant"},
                "parts": [{"type": "tool", "metadata": "providerExecuted"}]
            }]),
            serde_json::json!([{
                "info": {"role": "assistant"},
                "parts": [{"type": "tool", "state": {"metadata": "interrupted"}}]
            }]),
        ] {
            let response = ok_json_value(&body);
            let (port, _captured) = spawn_server(move |_request| response.clone());
            let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
            assert_eq!(
                client
                    .list_messages("ses_x")
                    .expect_err("malformed lifecycle container must fail closed"),
                MessageError::Malformed,
                "body {body:?}"
            );
        }
    }

    #[test]
    fn message_falsy_non_object_containers_keep_reference_defaults() {
        // A falsy non-object `time`/`state`/`metadata` is `or {}` in the
        // reference, so it stays a permissive default rather than an error.
        let body = serde_json::json!([
            {"info": {"role": "assistant", "time": null}},
            {"info": {"role": "assistant"}, "parts": [{"type": "tool", "state": null}]},
            {"info": {"role": "assistant"}, "parts": [{"type": "tool", "metadata": 0}]},
            {"info": {"role": "assistant"}, "parts": [{"type": "text", "text": null}]}
        ]);
        let response = ok_json_value(&body);
        let (port, _captured) = spawn_server(move |_request| response.clone());
        let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
        let messages = client.list_messages("ses_x").expect("list must parse");
        assert!(!messages[0].is_completed());
        assert!(messages[1].has_pending_tool_parts());
        assert!(messages[2].has_pending_tool_parts());
        assert_eq!(messages[3].text(), "");
    }

    #[test]
    fn message_usage_normalization_matches_reference() {
        let body = serde_json::json!([{
            "info": {
                "role": "assistant",
                "cost": -1,
                "tokens": {
                    "input": true,
                    "output": "unknown",
                    "reasoning": null,
                    "cache": {"read": -5, "write": 7}
                }
            }
        }]);
        let response = ok_json_value(&body);
        let (port, _captured) = spawn_server(move |_request| response.clone());
        let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
        let messages = client.list_messages("ses_x").expect("list must parse");
        let usage = messages[0].info().usage();
        assert_eq!(usage.input(), 0.0);
        assert_eq!(usage.output(), 0.0);
        assert_eq!(usage.reasoning(), 0.0);
        assert_eq!(usage.cache_read(), 0.0);
        assert_eq!(usage.cache_write(), 7.0);
        assert_eq!(usage.cost(), 0.0);

        let missing = serde_json::json!([{"info": {"role": "assistant"}}]);
        let response = ok_json_value(&missing);
        let (port, _captured) = spawn_server(move |_request| response.clone());
        let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
        let messages = client.list_messages("ses_x").expect("list must parse");
        assert_eq!(messages[0].info().usage(), &Usage::empty());
    }

    #[test]
    fn message_model_requires_non_empty_strings_like_reference() {
        let body = serde_json::json!([
            {"info": {"role": "assistant", "providerID": "p", "modelID": "m"}},
            {"info": {"role": "assistant", "providerID": "p"}},
            {"info": {"role": "assistant", "providerID": "", "modelID": "m"}},
            {"info": {"role": "assistant", "providerID": "p", "modelID": ""}},
            {"info": {"role": "assistant", "providerID": 1, "modelID": "m"}},
            {"info": {"role": "assistant", "providerID": "p", "modelID": ["m"]}}
        ]);
        let response = ok_json_value(&body);
        let (port, _captured) = spawn_server(move |_request| response.clone());
        let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
        let messages = client.list_messages("ses_x").expect("list must parse");
        assert_eq!(messages[0].info().model(), Some(("p", "m")));
        for message in &messages[1..] {
            assert_eq!(message.info().model(), None);
        }
    }

    #[test]
    fn message_text_extraction_matches_reference() {
        let body = serde_json::json!([{
            "info": {"role": "assistant"},
            "parts": [
                {"type": "text", "text": "  first  "},
                {"type": "text", "text": ""},
                {"type": "text", "text": "second"},
                {"type": "text", "text": "hidden", "ignored": true},
                {"type": "text", "text": "hidden-string", "ignored": "false"},
                {"type": "tool", "tool": "bash"}
            ]
        }]);
        let response = ok_json_value(&body);
        let (port, _captured) = spawn_server(move |_request| response.clone());
        let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
        let messages = client.list_messages("ses_x").expect("list must parse");
        assert_eq!(messages[0].text(), "first  \nsecond");
        assert!(messages[0].parts()[3].ignored());
        assert!(messages[0].parts()[4].ignored());
    }

    #[test]
    fn message_text_strips_python_whitespace_only_at_edges() {
        // Python `str.strip()` additionally removes U+001C..U+001F, which the
        // Unicode `White_Space` property behind Rust `trim()` omits. Ordinary
        // Unicode whitespace must still be stripped and interior separators
        // preserved.
        let body = serde_json::json!([
            {"info": {"role": "assistant"}, "parts": [{"type": "text", "text": "\u{1c}OK\u{1f}"}]},
            {"info": {"role": "assistant"}, "parts": [{"type": "text", "text": "\u{1c}\u{1d}\u{1e}\u{1f}"}]},
            {"info": {"role": "assistant"}, "parts": [{"type": "text", "text": "\u{a0}\u{3000}hi\u{2028}\u{205f}"}]},
            {"info": {"role": "assistant"}, "parts": [{"type": "text", "text": "a\u{1c}b"}]},
            {"info": {"role": "assistant"}, "parts": [{"type": "text", "text": "  spaced  "}]},
            {"info": {"role": "assistant"}, "parts": [{"type": "text", "text": "\u{1c}A"}, {"type": "text", "text": "B\u{1f}"}]}
        ]);
        let response = ok_json_value(&body);
        let (port, _captured) = spawn_server(move |_request| response.clone());
        let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
        let messages = client.list_messages("ses_x").expect("list must parse");
        assert_eq!(messages[0].parts()[0].text(), Some("\u{1c}OK\u{1f}"));
        assert_eq!(messages[0].text(), "OK");
        assert_eq!(messages[1].text(), "");
        assert_eq!(messages[2].text(), "hi");
        assert_eq!(messages[3].text(), "a\u{1c}b");
        assert_eq!(messages[4].text(), "spaced");
        assert_eq!(messages[5].text(), "A\nB");
    }

    #[test]
    fn message_tool_parts_lifecycle_matches_reference() {
        let body = serde_json::json!([
            {
                "info": {"role": "assistant"},
                "parts": [{"type": "tool", "tool": "bash", "state": {"status": "running"}}]
            },
            {
                "info": {"role": "assistant"},
                "parts": [{
                    "type": "tool",
                    "tool": "bash",
                    "state": {
                        "status": "error",
                        "error": "boom",
                        "metadata": {"interrupted": true}
                    }
                }]
            },
            {
                "info": {"role": "assistant"},
                "parts": [{
                    "type": "tool",
                    "tool": "bash",
                    "state": {"status": "error", "error": "boom"}
                }]
            },
            {
                "info": {"role": "assistant"},
                "parts": [{
                    "type": "tool",
                    "tool": "bash",
                    "metadata": {"providerExecuted": true},
                    "state": {"status": "running"}
                }]
            },
            {
                "info": {"role": "assistant"},
                "parts": [{"type": "text", "text": "done"}]
            }
        ]);
        let response = ok_json_value(&body);
        let (port, _captured) = spawn_server(move |_request| response.clone());
        let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
        let messages = client.list_messages("ses_x").expect("list must parse");

        assert!(messages[0].has_pending_tool_parts());
        assert!(!messages[1].has_pending_tool_parts());
        assert!(messages[2].has_pending_tool_parts());
        assert!(!messages[3].has_pending_tool_parts());
        assert!(!messages[4].has_pending_tool_parts());

        let part = &messages[1].parts()[0];
        assert!(part.is_tool());
        assert_eq!(part.tool_name(), Some("bash"));
        assert_eq!(part.tool_status(), Some("error"));
        assert_eq!(part.tool_error(), Some(&serde_json::json!("boom")));
        assert!(part.interrupted());
        assert!(!part.provider_executed());
        assert!(messages[3].parts()[0].provider_executed());
    }

    #[test]
    fn message_types_never_render_content_workspace_or_credentials() {
        let dir = TempDir::new("message-redact");
        let workspace = std::fs::canonicalize(dir.path()).expect("workspace must canonicalize");
        let workspace_text = workspace.to_string_lossy().into_owned();
        let secret_id = "msg_secret-id";
        let secret_text = "secret message text";
        let secret_error = "secret-error-value";
        let secret_tool_error = "secret-tool-error";
        let body = serde_json::json!([{
            "info": {
                "id": secret_id,
                "role": "assistant",
                "providerID": "secret-provider",
                "modelID": "secret-model",
                "error": secret_error,
                "tokens": {"input": 1}
            },
            "parts": [
                {"type": "text", "text": secret_text},
                {
                    "type": "tool",
                    "tool": "secret-tool",
                    "state": {"status": "error", "error": secret_tool_error}
                }
            ]
        }]);
        let response = ok_json_value(&body);
        let (port, _captured) = spawn_server(move |_request| response.clone());
        let (_client_dir, client) = client_for(port, workspace);
        let messages = client.list_messages(secret_id).expect("list must parse");
        let message = &messages[0];
        let part = &message.parts()[1];

        for rendered in [
            format!("{message:?}"),
            message.to_string(),
            format!("{:?}", message.info()),
            message.info().to_string(),
            format!("{part:?}"),
            part.to_string(),
            format!("{:?}", message.info().usage()),
            message.info().usage().to_string(),
        ] {
            assert!(!rendered.contains(secret_id));
            assert!(!rendered.contains(secret_text));
            assert!(!rendered.contains(secret_error));
            assert!(!rendered.contains(secret_tool_error));
            assert!(!rendered.contains("secret-provider"));
            assert!(!rendered.contains("secret-model"));
            assert!(!rendered.contains("secret-tool"));
            assert!(!rendered.contains(&workspace_text));
        }
        for error in [
            MessageError::Malformed,
            MessageError::InvalidSessionId,
            MessageError::Transport(TransportError::Unauthorized),
        ] {
            let rendered = format!("{error} {error:?}");
            assert!(!rendered.contains(secret_id));
            assert!(!rendered.contains(secret_text));
            assert!(!rendered.contains(secret_error));
            assert!(!rendered.contains(&workspace_text));
            assert!(!rendered.contains(PASSWORD));
            assert!(!rendered.contains(PASSWORD_BASE64));
        }
        assert!(!format!("{client:?}").contains(&workspace_text));
    }

    // --- 6.6 async prompt delivery -----------------------------------------

    /// Builds a bodyless `204 No Content` response for a successful delivery.
    fn no_content_204() -> Vec<u8> {
        b"HTTP/1.1 204 No Content\r\nconnection: close\r\n\r\n".to_vec()
    }

    /// Builds a client bound to a project that selects an `opencode_model`.
    ///
    /// The canonical workspace is the temporary `ws` directory, so callers must
    /// keep the returned [`TempDir`] alive and must not assert a literal query.
    fn client_with_project_model(port: u16, model: &str) -> (TempDir, OpenCodeClient) {
        let dir = TempDir::new("prompt-model");
        dir.mkdir("ws");
        dir.mkdir("secrets");
        let password = dir.path().join("secrets/proj.password");
        std::fs::write(&password, format!("{PASSWORD}\n")).expect("password must be writable");
        set_private(&password);
        let config = format!(
            "[projects.proj]\nworkspace = \"ws\"\nopencode_url = \"http://127.0.0.1:{port}\"\npassword_file = \"secrets/proj.password\"\nopencode_model = \"{model}\"\nmax_rounds = 3\n"
        );
        let config_path = dir.path().join("projects.toml");
        std::fs::write(&config_path, config).expect("config must be writable");
        let config = bridge_config::load_config(&config_path).expect("config must load");
        let project = config.project("proj").expect("project must exist");
        let client = OpenCodeClient::from_project(project, Duration::from_secs(5))
            .expect("client must build");
        (dir, client)
    }

    #[test]
    fn send_prompt_async_posts_scoped_body_without_model() {
        let (port, captured) = spawn_server(|_request| no_content_204());
        let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
        client
            .send_prompt_async("ses_abc", "msg_1", "hello")
            .expect("delivery must succeed");

        let raw = captured.lock().expect("capture mutex").clone();
        let text = String::from_utf8_lossy(&raw);
        assert!(
            text.starts_with(
                "POST /session/ses_abc/prompt_async?directory=%2Ftmp%2Fws HTTP/1.1\r\n"
            ),
            "prompt must be a scoped POST to prompt_async, got {text}"
        );
        assert!(text.contains("content-type: application/json\r\n"));
        assert!(text.contains(&format!("authorization: Basic {PASSWORD_BASE64}\r\n")));
        let body = br#"{"messageID":"msg_1","parts":[{"type":"text","text":"hello"}]}"#;
        assert!(text.contains(&format!("content-length: {}\r\n", body.len())));
        assert!(raw.ends_with(body), "body must match the reference exactly");
        assert!(
            !text.contains("\"model\""),
            "a client without a configured model must send no model field"
        );
    }

    #[test]
    fn send_prompt_async_includes_project_model_last() {
        let (port, captured) = spawn_server(|_request| no_content_204());
        let (_dir, client) = client_with_project_model(port, "anthropic/claude-sonnet");
        client
            .send_prompt_async("ses_abc", "msg_1", "hi")
            .expect("delivery must succeed");

        let raw = captured.lock().expect("capture mutex").clone();
        let body = br#"{"messageID":"msg_1","parts":[{"type":"text","text":"hi"}],"model":{"providerID":"anthropic","modelID":"claude-sonnet"}}"#;
        assert!(
            raw.ends_with(body),
            "the model must be emitted last from the validated project entry"
        );
    }

    #[test]
    fn send_prompt_async_escapes_utf8_and_json_special_characters() {
        let (port, captured) = spawn_server(|_request| no_content_204());
        let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
        let message_id = "msg\"id\\1";
        let text = "héllo\n\"world\"\t\\";
        client
            .send_prompt_async("ses_abc", message_id, text)
            .expect("delivery must succeed");

        let raw = captured.lock().expect("capture mutex").clone();
        let body = raw
            .windows(4)
            .rposition(|window| window == b"\r\n\r\n")
            .map(|index| &raw[index + 4..])
            .expect("body separator must be present");
        assert!(
            body.windows(2).any(|window| window == "é".as_bytes()),
            "non-ASCII text must be emitted as raw UTF-8, not ASCII-escaped"
        );
        assert!(
            !String::from_utf8_lossy(body).contains("\\u00e9"),
            "the body must not ASCII-escape UTF-8"
        );
        let value: serde_json::Value =
            serde_json::from_slice(body).expect("body must be valid JSON");
        assert_eq!(value["messageID"], serde_json::json!(message_id));
        assert_eq!(value["parts"][0]["type"], serde_json::json!("text"));
        assert_eq!(value["parts"][0]["text"], serde_json::json!(text));
    }

    #[test]
    fn send_prompt_async_ignores_success_body() {
        // A 2xx is success even when the body is not JSON, because the
        // reference `_request` helper never parses the delivery response.
        let (port, _captured) = spawn_server(|_request| ok_json(b"not-json"));
        let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
        client
            .send_prompt_async("ses_abc", "msg_1", "hello")
            .expect("a 2xx delivery must succeed regardless of the body");
    }

    #[test]
    fn send_prompt_async_rejects_unusable_session_id_without_sending() {
        let (port, captured) = spawn_server(|_request| no_content_204());
        let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
        for id in ["", ".", ".."] {
            assert_eq!(
                client
                    .send_prompt_async(id, "msg_1", "hello")
                    .expect_err("id must be rejected"),
                PromptError::InvalidSessionId,
                "id {id:?} must fail closed"
            );
        }
        assert!(
            captured.lock().expect("capture mutex").is_empty(),
            "an unusable id must not produce a request"
        );
    }

    #[test]
    fn send_prompt_async_percent_encodes_id_and_scopes_request() {
        let (port, captured) = spawn_server(|_request| no_content_204());
        let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
        client
            .send_prompt_async("ses/x y?z#w", "msg_1", "hello")
            .expect("encoded id must be accepted");
        let raw = captured.lock().expect("capture mutex").clone();
        assert_eq!(
            request_target(&raw),
            "POST /session/ses%2Fx%20y%3Fz%23w/prompt_async?directory=%2Ftmp%2Fws HTTP/1.1"
        );
    }

    #[test]
    fn send_prompt_async_preserves_transport_errors() {
        let cases = [
            (401_u16, TransportError::Unauthorized),
            (404, TransportError::NotFound),
            (500, TransportError::HttpStatus(500)),
        ];
        for (status, expected) in cases {
            let (port, _captured) = spawn_server(move |_request| error_response(status));
            let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
            assert_eq!(
                client
                    .send_prompt_async("ses_x", "msg_1", "hello")
                    .expect_err("delivery must fail"),
                PromptError::Transport(expected)
            );
        }
    }

    #[test]
    fn send_prompt_async_rejects_redirect_as_http_status() {
        // The reference `_request` returns the response for any status < 400
        // (including 3xx); the shared transport keeps its `2xx`-only policy, so
        // a redirect is a non-success `HttpStatus` rather than a delivery.
        let (port, captured) = spawn_server(move |_request| error_response(302));
        let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
        assert_eq!(
            client
                .send_prompt_async("ses_x", "msg_1", "hello")
                .expect_err("a redirect must not be treated as success"),
            PromptError::Transport(TransportError::HttpStatus(302)),
            "a 3xx must keep the existing non-success transport category"
        );
        assert!(
            !captured.lock().expect("capture mutex").is_empty(),
            "the request must have been sent before the status was judged"
        );
    }

    #[test]
    fn send_prompt_async_preserves_protocol_error() {
        let (port, _captured) = spawn_server(|_request| b"not-http\r\n\r\n".to_vec());
        let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
        assert_eq!(
            client
                .send_prompt_async("ses_x", "msg_1", "hello")
                .expect_err("a malformed response must fail closed"),
            PromptError::Transport(TransportError::Protocol)
        );
    }

    #[test]
    fn send_prompt_async_preserves_unavailable_and_timeout() {
        let port = {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
                .expect("server must bind an ephemeral port");
            listener
                .local_addr()
                .expect("bound address must be available")
                .port()
        };
        let (_dir, endpoint, secret) = project_material(port);
        let transport =
            HttpTransport::new(endpoint, BasicAuth::new(secret), Duration::from_secs(2));
        let client = OpenCodeClient::new(transport, PathBuf::from("/tmp/ws"));
        assert_eq!(
            client
                .send_prompt_async("ses_x", "msg_1", "hello")
                .expect_err("delivery must fail"),
            PromptError::Transport(TransportError::Unavailable)
        );

        let (port, captured) = spawn_server(|_request| no_content_204());
        let (_dir2, endpoint, secret) = project_material(port);
        let transport = HttpTransport::new(endpoint, BasicAuth::new(secret), Duration::ZERO);
        let client = OpenCodeClient::new(transport, PathBuf::from("/tmp/ws"));
        assert_eq!(
            client
                .send_prompt_async("ses_x", "msg_1", "hello")
                .expect_err("zero timeout must fail closed"),
            PromptError::Transport(TransportError::Timeout)
        );
        assert!(
            captured.lock().expect("capture mutex").is_empty(),
            "a zero timeout must fail before sending"
        );
    }

    #[test]
    fn send_prompt_async_times_out_after_full_delivery_without_retry() {
        // The mock fully receives and records the POST body, then withholds the
        // HTTP response past the client deadline. The client must surface a
        // timeout (the delivery outcome is undefined), the request must have
        // been delivered exactly once, and no second POST may be sent.
        let (port, captured) = spawn_server(|_request| {
            thread::sleep(Duration::from_millis(600));
            no_content_204()
        });
        let (_dir, endpoint, secret) = project_material(port);
        let transport =
            HttpTransport::new(endpoint, BasicAuth::new(secret), Duration::from_millis(300));
        let client = OpenCodeClient::new(transport, PathBuf::from("/tmp/ws"));
        assert_eq!(
            client
                .send_prompt_async("ses_x", "msg_1", "hello")
                .expect_err("a stalled response must time out"),
            PromptError::Transport(TransportError::Timeout),
            "a response withheld past the deadline must map to Timeout"
        );

        let raw = captured.lock().expect("capture mutex").clone();
        let requests = raw
            .windows(b"POST ".len())
            .filter(|window| *window == b"POST ")
            .count();
        assert_eq!(
            requests, 1,
            "the non-idempotent prompt must be delivered once, with no retry"
        );
        assert!(
            raw.ends_with(br#"{"messageID":"msg_1","parts":[{"type":"text","text":"hello"}]}"#),
            "the full POST body must have reached the server before the timeout"
        );
    }

    #[test]
    fn prompt_types_never_render_ids_text_model_workspace_or_credentials() {
        let (port, _captured) = spawn_server(|_request| no_content_204());
        let (dir, client) = client_with_project_model(port, "secret-provider/secret-model");
        let workspace_text = client.workspace().to_string_lossy().into_owned();
        let _keep = dir;
        let secret_id = "ses_secret-id";
        let secret_message = "msg_secret-id";
        let secret_text = "secret prompt text";

        // Exercise the request path so the body is actually built.
        client
            .send_prompt_async(secret_id, secret_message, secret_text)
            .expect("delivery must succeed");

        let rendered = format!("{client:?}");
        assert!(!rendered.contains(&workspace_text));
        assert!(!rendered.contains("secret-provider"));
        assert!(!rendered.contains("secret-model"));
        assert!(!rendered.contains(secret_id));
        assert!(!rendered.contains(secret_message));
        assert!(!rendered.contains(secret_text));
        assert!(!rendered.contains(PASSWORD));
        assert!(!rendered.contains(PASSWORD_BASE64));
        for error in [
            PromptError::InvalidSessionId,
            PromptError::Transport(TransportError::Unauthorized),
            PromptError::Transport(TransportError::Timeout),
            PromptError::Transport(TransportError::Unavailable),
        ] {
            let rendered = format!("{error} {error:?}");
            assert!(!rendered.contains(&workspace_text));
            assert!(!rendered.contains("secret-provider"));
            assert!(!rendered.contains("secret-model"));
            assert!(!rendered.contains(secret_id));
            assert!(!rendered.contains(secret_message));
            assert!(!rendered.contains(secret_text));
            assert!(!rendered.contains(PASSWORD));
            assert!(!rendered.contains(PASSWORD_BASE64));
        }
    }

    // --- 6.7 permissions list/reply ----------------------------------------

    /// A reference-compatible `PermissionRequest` array (OpenCode SDK shape).
    fn reference_permission_fixture() -> serde_json::Value {
        serde_json::json!([
            {
                "id": "per_1",
                "sessionID": "ses_1",
                "permission": "bash",
                "patterns": ["git status"],
                "metadata": {"directories": ["/trusted"]},
                "always": ["git status"],
                "tool": {"messageID": "msg_1", "callID": "call_1"}
            },
            {
                "id": "per_2",
                "sessionID": "ses_2",
                "permission": "external_directory",
                "patterns": ["/etc/*"],
                "metadata": {},
                "always": []
            }
        ])
    }

    #[test]
    fn permission_reply_enum_maps_to_wire_tokens() {
        assert_eq!(PermissionReply::Once.as_str(), "once");
        assert_eq!(PermissionReply::Always.as_str(), "always");
        assert_eq!(PermissionReply::Reject.as_str(), "reject");
        assert_eq!(PermissionReply::Once.to_string(), "once");
    }

    #[test]
    fn list_permissions_success_is_scoped_and_parses_reference_fixture() {
        let fixture = reference_permission_fixture();
        let (port, captured) = spawn_server(move |_request| ok_json_value(&fixture));
        let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
        let permissions = client.list_permissions().expect("list must parse");
        assert_eq!(permissions.len(), 2);

        let first = &permissions[0];
        assert_eq!(first.id(), Some("per_1"));
        assert_eq!(first.session_id(), Some("ses_1"));
        assert_eq!(first.permission(), Some("bash"));
        assert_eq!(first.patterns(), ["git status"]);
        assert_eq!(first.always(), ["git status"]);
        assert_eq!(
            first.metadata(),
            &serde_json::json!({"directories": ["/trusted"]})
        );
        let tool = first.tool().expect("tool must parse");
        assert_eq!(tool.message_id(), Some("msg_1"));
        assert_eq!(tool.call_id(), Some("call_1"));
        assert!(permissions[1].tool().is_none());

        let raw = captured.lock().expect("capture mutex").clone();
        assert_eq!(
            request_target(&raw),
            "GET /permission?directory=%2Ftmp%2Fws HTTP/1.1"
        );
        let text = String::from_utf8_lossy(&raw);
        assert!(text.contains(&format!("authorization: Basic {PASSWORD_BASE64}\r\n")));
        assert!(text.contains("accept: application/json\r\n"));
    }

    #[test]
    fn list_permissions_keeps_valid_empty_patterns_and_optional_defaults() {
        let permissions = parse_permission_list(
            br#"[{"id":"per_1","sessionID":"ses_1","permission":"read","patterns":[]}]"#,
        )
        .expect("a valid ordinary permission with empty patterns must parse");
        let permission = &permissions[0];
        assert_eq!(permission.id(), Some("per_1"));
        assert_eq!(permission.session_id(), Some("ses_1"));
        assert_eq!(permission.permission(), Some("read"));
        assert!(permission.patterns().is_empty());
        assert_eq!(permission.metadata(), &serde_json::json!({}));
        assert!(permission.always().is_empty());
        assert!(permission.tool().is_none());

        let nulls = parse_permission_list(
            br#"[{"id":"per_2","sessionID":"ses_2","permission":"read","patterns":[],"metadata":null,"always":null,"tool":null}]"#,
        )
        .expect("null metadata/always/tool keep the reference defaults");
        let permission = &nulls[0];
        assert_eq!(permission.id(), Some("per_2"));
        assert!(permission.patterns().is_empty());
        assert_eq!(permission.metadata(), &serde_json::json!({}));
        assert!(permission.always().is_empty());
        assert!(permission.tool().is_none());
    }

    #[test]
    fn list_permissions_rejects_missing_or_null_required_identity_and_patterns() {
        // The SDK marks id/sessionID/permission/patterns required. A missing or
        // null required field must fail closed instead of collapsing to
        // None/an empty list that a consumer could mistake for an absent or
        // approvable request.
        let bodies: [&[u8]; 8] = [
            br#"[{"sessionID":"ses_1","permission":"read","patterns":[]}]"#,
            br#"[{"id":null,"sessionID":"ses_1","permission":"read","patterns":[]}]"#,
            br#"[{"id":"per_1","permission":"read","patterns":[]}]"#,
            br#"[{"id":"per_1","sessionID":null,"permission":"read","patterns":[]}]"#,
            br#"[{"id":"per_1","sessionID":"ses_1","patterns":[]}]"#,
            br#"[{"id":"per_1","sessionID":"ses_1","permission":null,"patterns":[]}]"#,
            br#"[{"id":"per_1","sessionID":"ses_1","permission":"read"}]"#,
            br#"[{"id":"per_1","sessionID":"ses_1","permission":"read","patterns":null}]"#,
        ];
        for body in bodies {
            assert_eq!(
                parse_permission_list(body).expect_err("must fail closed"),
                PermissionError::Malformed,
                "body {body:?} must be malformed rather than a missing/empty default"
            );
        }
    }

    #[test]
    fn list_permissions_does_not_skip_a_malformed_request_between_valid_ones() {
        // A damaged middle request (here: missing required patterns) must fail
        // the whole operation, never be skipped so the surrounding valid
        // requests make the list look complete or approved.
        let body = br#"[
            {"id":"per_1","sessionID":"ses_1","permission":"read","patterns":[]},
            {"id":"per_2","sessionID":"ses_2","permission":"bash"},
            {"id":"per_3","sessionID":"ses_3","permission":"read","patterns":[]}
        ]"#;
        assert_eq!(
            parse_permission_list(body).expect_err("must not skip the damaged element"),
            PermissionError::Malformed
        );
    }

    #[test]
    fn list_permissions_malformed_response_fails_closed() {
        let bodies: [&[u8]; 5] = [b"not-json", b"{}", b"null", b"\"x\"", b"1"];
        for body in bodies {
            let (port, _captured) = spawn_server(move |_request| ok_json(body));
            let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
            assert_eq!(
                client.list_permissions().expect_err("must fail closed"),
                PermissionError::Malformed,
                "body {body:?} must be malformed"
            );
        }
    }

    #[test]
    fn list_permissions_fails_closed_on_malformed_elements_and_fields() {
        let bodies: [&[u8]; 9] = [
            br#"[42]"#,
            br#"[{"id":42,"sessionID":"ses_1","permission":"read","patterns":[]}]"#,
            br#"[{"id":"per_1","sessionID":42,"permission":"read","patterns":[]}]"#,
            br#"[{"id":"per_1","sessionID":"ses_1","permission":42,"patterns":[]}]"#,
            br#"[{"id":"per_1","sessionID":"ses_1","permission":"read","patterns":"ls"}]"#,
            br#"[{"id":"per_1","sessionID":"ses_1","permission":"read","patterns":["ls",42]}]"#,
            br#"[{"id":"per_1","sessionID":"ses_1","permission":"read","patterns":[],"metadata":"x"}]"#,
            br#"[{"id":"per_1","sessionID":"ses_1","permission":"read","patterns":[],"always":42}]"#,
            br#"[{"id":"per_1","sessionID":"ses_1","permission":"read","patterns":[],"tool":"x"}]"#,
        ];
        for body in bodies {
            assert_eq!(
                parse_permission_list(body).expect_err("must fail closed"),
                PermissionError::Malformed,
                "body {body:?} must be malformed rather than skipped"
            );
        }
    }

    #[test]
    fn list_permissions_preserves_transport_errors() {
        let cases = [
            (401_u16, TransportError::Unauthorized),
            (404, TransportError::NotFound),
            (500, TransportError::HttpStatus(500)),
        ];
        for (status, expected) in cases {
            let (port, _captured) = spawn_server(move |_request| error_response(status));
            let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
            assert_eq!(
                client.list_permissions().expect_err("must fail"),
                PermissionError::Transport(expected)
            );
        }
    }

    #[test]
    fn list_permissions_preserves_unavailable_and_timeout() {
        let port = {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
                .expect("server must bind an ephemeral port");
            listener
                .local_addr()
                .expect("bound address must be available")
                .port()
        };
        let (_dir, endpoint, secret) = project_material(port);
        let transport =
            HttpTransport::new(endpoint, BasicAuth::new(secret), Duration::from_secs(2));
        let client = OpenCodeClient::new(transport, PathBuf::from("/tmp/ws"));
        assert_eq!(
            client.list_permissions().expect_err("must fail"),
            PermissionError::Transport(TransportError::Unavailable)
        );

        let (port, captured) = spawn_server(|_request| ok_json(b"[]"));
        let (_dir2, endpoint, secret) = project_material(port);
        let transport = HttpTransport::new(endpoint, BasicAuth::new(secret), Duration::ZERO);
        let client = OpenCodeClient::new(transport, PathBuf::from("/tmp/ws"));
        assert_eq!(
            client.list_permissions().expect_err("must fail"),
            PermissionError::Transport(TransportError::Timeout)
        );
        assert!(
            captured.lock().expect("capture mutex").is_empty(),
            "a zero timeout must fail before sending"
        );
    }

    #[test]
    fn reply_permission_posts_scoped_body_for_each_reply() {
        let cases = [
            (PermissionReply::Once, "once"),
            (PermissionReply::Always, "always"),
            (PermissionReply::Reject, "reject"),
        ];
        for (reply, token) in cases {
            let (port, captured) = spawn_server(|_request| ok_json(b"true"));
            let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
            client
                .reply_permission("per_1", reply, None)
                .expect("reply must succeed");
            let raw = captured.lock().expect("capture mutex").clone();
            assert_eq!(
                request_target(&raw),
                "POST /permission/per_1/reply?directory=%2Ftmp%2Fws HTTP/1.1"
            );
            let text = String::from_utf8_lossy(&raw);
            assert!(text.contains("content-type: application/json\r\n"));
            assert!(text.contains(&format!("authorization: Basic {PASSWORD_BASE64}\r\n")));
            let body = format!(r#"{{"reply":"{token}"}}"#);
            assert!(text.contains(&format!("content-length: {}\r\n", body.len())));
            assert!(
                raw.ends_with(body.as_bytes()),
                "body must be exactly {{reply}} for {reply:?}"
            );
        }
    }

    #[test]
    fn reply_permission_includes_optional_message_and_distinguishes_empty() {
        let cases: [(PermissionReply, Option<&str>, &[u8]); 3] = [
            (PermissionReply::Once, None, br#"{"reply":"once"}"#),
            (
                PermissionReply::Reject,
                Some(""),
                br#"{"reply":"reject","message":""}"#,
            ),
            (
                PermissionReply::Always,
                Some("why"),
                br#"{"reply":"always","message":"why"}"#,
            ),
        ];
        for (reply, message, expected) in cases {
            let (port, captured) = spawn_server(|_request| ok_json(b"true"));
            let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
            client
                .reply_permission("per_1", reply, message)
                .expect("reply must succeed");
            let raw = captured.lock().expect("capture mutex").clone();
            assert!(
                raw.ends_with(expected),
                "body must match reference exactly for {reply:?} {message:?}"
            );
        }
    }

    #[test]
    fn reply_permission_percent_encodes_request_id_and_scopes_request() {
        let (port, captured) = spawn_server(|_request| ok_json(b"true"));
        let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
        client
            .reply_permission("per/x y?z#w", PermissionReply::Once, None)
            .expect("encoded id must be accepted");
        let raw = captured.lock().expect("capture mutex").clone();
        assert_eq!(
            request_target(&raw),
            "POST /permission/per%2Fx%20y%3Fz%23w/reply?directory=%2Ftmp%2Fws HTTP/1.1"
        );
    }

    #[test]
    fn reply_permission_rejects_unusable_id_without_sending() {
        let (port, captured) = spawn_server(|_request| ok_json(b"true"));
        let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
        for id in ["", ".", ".."] {
            assert_eq!(
                client
                    .reply_permission(id, PermissionReply::Once, None)
                    .expect_err("id must be rejected"),
                PermissionError::InvalidRequestId,
                "id {id:?} must fail closed"
            );
        }
        assert!(
            captured.lock().expect("capture mutex").is_empty(),
            "an unusable id must not produce a request"
        );
    }

    #[test]
    fn reply_permission_ignores_success_body_and_accepts_bodyless_204() {
        let (port, _captured) = spawn_server(|_request| ok_json(b"not-json"));
        let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
        client
            .reply_permission("per_1", PermissionReply::Once, None)
            .expect("a 2xx reply must succeed regardless of the body");

        let (port, _captured) = spawn_server(|_request| no_content_204());
        let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
        client
            .reply_permission("per_1", PermissionReply::Once, None)
            .expect("a bodyless 204 reply must succeed");
    }

    #[test]
    fn reply_permission_preserves_transport_errors() {
        let cases = [
            (401_u16, TransportError::Unauthorized),
            (404, TransportError::NotFound),
            (500, TransportError::HttpStatus(500)),
        ];
        for (status, expected) in cases {
            let (port, _captured) = spawn_server(move |_request| error_response(status));
            let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
            assert_eq!(
                client
                    .reply_permission("per_1", PermissionReply::Once, None)
                    .expect_err("must fail"),
                PermissionError::Transport(expected)
            );
        }
    }

    #[test]
    fn reply_permission_rejects_redirect_as_http_status() {
        let (port, captured) = spawn_server(move |_request| error_response(302));
        let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
        assert_eq!(
            client
                .reply_permission("per_1", PermissionReply::Once, None)
                .expect_err("a redirect must not be treated as success"),
            PermissionError::Transport(TransportError::HttpStatus(302)),
            "a 3xx must keep the existing non-success transport category"
        );
        assert!(
            !captured.lock().expect("capture mutex").is_empty(),
            "the request must have been sent before the status was judged"
        );
    }

    #[test]
    fn reply_permission_preserves_protocol_error() {
        let (port, _captured) = spawn_server(|_request| b"not-http\r\n\r\n".to_vec());
        let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
        assert_eq!(
            client
                .reply_permission("per_1", PermissionReply::Once, None)
                .expect_err("a malformed response must fail closed"),
            PermissionError::Transport(TransportError::Protocol)
        );
    }

    #[test]
    fn reply_permission_preserves_unavailable_and_timeout() {
        let port = {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
                .expect("server must bind an ephemeral port");
            listener
                .local_addr()
                .expect("bound address must be available")
                .port()
        };
        let (_dir, endpoint, secret) = project_material(port);
        let transport =
            HttpTransport::new(endpoint, BasicAuth::new(secret), Duration::from_secs(2));
        let client = OpenCodeClient::new(transport, PathBuf::from("/tmp/ws"));
        assert_eq!(
            client
                .reply_permission("per_1", PermissionReply::Once, None)
                .expect_err("must fail"),
            PermissionError::Transport(TransportError::Unavailable)
        );

        let (port, captured) = spawn_server(|_request| ok_json(b"true"));
        let (_dir2, endpoint, secret) = project_material(port);
        let transport = HttpTransport::new(endpoint, BasicAuth::new(secret), Duration::ZERO);
        let client = OpenCodeClient::new(transport, PathBuf::from("/tmp/ws"));
        assert_eq!(
            client
                .reply_permission("per_1", PermissionReply::Once, None)
                .expect_err("must fail"),
            PermissionError::Transport(TransportError::Timeout)
        );
        assert!(
            captured.lock().expect("capture mutex").is_empty(),
            "a zero timeout must fail before sending"
        );
    }

    #[test]
    fn reply_permission_times_out_after_full_delivery_without_retry() {
        // The mock fully receives and records the POST body, then withholds the
        // HTTP response past the client deadline. The client must surface a
        // timeout (the reply outcome is undefined), the request must have been
        // delivered exactly once, and no second POST may be sent.
        let (port, captured) = spawn_server(|_request| {
            thread::sleep(Duration::from_millis(600));
            ok_json(b"true")
        });
        let (_dir, endpoint, secret) = project_material(port);
        let transport =
            HttpTransport::new(endpoint, BasicAuth::new(secret), Duration::from_millis(300));
        let client = OpenCodeClient::new(transport, PathBuf::from("/tmp/ws"));
        assert_eq!(
            client
                .reply_permission("per_1", PermissionReply::Once, None)
                .expect_err("a stalled response must time out"),
            PermissionError::Transport(TransportError::Timeout),
            "a response withheld past the deadline must map to Timeout"
        );

        let raw = captured.lock().expect("capture mutex").clone();
        let requests = raw
            .windows(b"POST ".len())
            .filter(|window| *window == b"POST ")
            .count();
        assert_eq!(
            requests, 1,
            "the reply must be sent once, with no automatic retry"
        );
        assert!(
            raw.ends_with(br#"{"reply":"once"}"#),
            "the full POST body must have reached the server before the timeout"
        );
    }

    #[test]
    fn permission_types_never_render_content_workspace_or_credentials() {
        let (port, _captured) = spawn_server(|_request| ok_json(b"[]"));
        let (dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
        let workspace_text = client.workspace().to_string_lossy().into_owned();
        let _keep = dir;

        let permissions = parse_permission_list(
            br#"[{"id":"per_secret","sessionID":"ses_secret","permission":"bash","patterns":["secret-pattern"],"metadata":{"directories":["/secret/dir"]},"always":["secret-always"],"tool":{"messageID":"msg_secret","callID":"call_secret"}}]"#,
        )
        .expect("must parse");
        let permission = &permissions[0];
        assert_eq!(
            permission.metadata(),
            &serde_json::json!({"directories": ["/secret/dir"]}),
            "the explicit raw accessor must expose the metadata"
        );
        let tool = permission.tool().expect("tool must parse");

        for rendered in [
            format!("{client:?}"),
            format!("{permission:?}"),
            format!("{permission}"),
            format!("{tool:?}"),
            format!("{tool}"),
        ] {
            assert!(!rendered.contains(&workspace_text));
            assert!(!rendered.contains("per_secret"));
            assert!(!rendered.contains("ses_secret"));
            assert!(!rendered.contains("secret-pattern"));
            assert!(!rendered.contains("/secret/dir"));
            assert!(!rendered.contains("secret-always"));
            assert!(!rendered.contains("msg_secret"));
            assert!(!rendered.contains("call_secret"));
            assert!(!rendered.contains(PASSWORD));
            assert!(!rendered.contains(PASSWORD_BASE64));
        }

        for error in [
            PermissionError::InvalidRequestId,
            PermissionError::Malformed,
            PermissionError::Transport(TransportError::Unauthorized),
            PermissionError::Transport(TransportError::Timeout),
            PermissionError::Transport(TransportError::Unavailable),
        ] {
            let rendered = format!("{error} {error:?}");
            assert!(!rendered.contains(&workspace_text));
            assert!(!rendered.contains("per_secret"));
            assert!(!rendered.contains(PASSWORD));
            assert!(!rendered.contains(PASSWORD_BASE64));
        }
    }

    // --- 6.8 questions and blockers ----------------------------------------

    /// A reference-compatible `QuestionRequest` array (OpenCode SDK shape).
    fn reference_question_fixture() -> serde_json::Value {
        serde_json::json!([
            {
                "id": "que_1",
                "sessionID": "ses_1",
                "questions": [
                    {
                        "question": "Which database?",
                        "header": "Database",
                        "options": [
                            {"label": "Postgres", "description": "Relational"},
                            {"label": "SQLite", "description": "Embedded"}
                        ],
                        "multiple": false,
                        "custom": true
                    }
                ],
                "tool": {"messageID": "msg_1", "callID": "call_1"}
            },
            {
                "id": "que_2",
                "sessionID": "ses_2",
                "questions": []
            }
        ])
    }

    #[test]
    fn list_questions_success_is_scoped_and_parses_reference_fixture() {
        let fixture = reference_question_fixture();
        let (port, captured) = spawn_server(move |_request| ok_json_value(&fixture));
        let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
        let questions = client.list_questions().expect("list must parse");
        assert_eq!(questions.len(), 2);

        let first = &questions[0];
        assert_eq!(first.id(), Some("que_1"));
        assert_eq!(first.session_id(), Some("ses_1"));
        assert!(first.belongs_to_session("ses_1"));
        assert!(!first.belongs_to_session("ses_2"));
        let info = &first.questions()[0];
        assert_eq!(info.question(), "Which database?");
        assert_eq!(info.header(), "Database");
        assert_eq!(info.options().len(), 2);
        assert_eq!(info.options()[0].label(), "Postgres");
        assert_eq!(info.options()[0].description(), "Relational");
        assert_eq!(info.multiple(), Some(false));
        assert_eq!(info.custom(), Some(true));
        let tool = first.tool().expect("tool must parse");
        assert_eq!(tool.message_id(), Some("msg_1"));
        assert_eq!(tool.call_id(), Some("call_1"));

        let second = &questions[1];
        assert_eq!(second.id(), Some("que_2"));
        assert!(second.questions().is_empty());
        assert!(second.tool().is_none());

        let raw = captured.lock().expect("capture mutex").clone();
        assert_eq!(
            request_target(&raw),
            "GET /question?directory=%2Ftmp%2Fws HTTP/1.1"
        );
        let text = String::from_utf8_lossy(&raw);
        assert!(text.contains(&format!("authorization: Basic {PASSWORD_BASE64}\r\n")));
        assert!(text.contains("accept: application/json\r\n"));
    }

    #[test]
    fn list_questions_keeps_valid_empty_questions_and_optional_defaults() {
        let questions =
            parse_question_list(br#"[{"id":"que_1","sessionID":"ses_1","questions":[]}]"#)
                .expect("a valid empty questions array must parse");
        assert!(questions[0].questions().is_empty());
        assert!(questions[0].tool().is_none());

        let nulls = parse_question_list(
            br#"[{"id":"que_2","sessionID":"ses_2","questions":[{"question":"q","header":"h","options":[],"multiple":null,"custom":null}],"tool":null}]"#,
        )
        .expect("null optional fields keep the reference defaults");
        let info = &nulls[0].questions()[0];
        assert!(info.options().is_empty());
        assert_eq!(info.multiple(), None);
        assert_eq!(info.custom(), None);
        assert!(nulls[0].tool().is_none());

        let absent = parse_question_list(
            br#"[{"id":"que_3","sessionID":"ses_3","questions":[{"question":"q","header":"h","options":[]}]}]"#,
        )
        .expect("absent optional flags keep the reference defaults");
        assert_eq!(absent[0].questions()[0].multiple(), None);
        assert_eq!(absent[0].questions()[0].custom(), None);
    }

    #[test]
    fn list_questions_valid_empty_top_level_list_succeeds_without_blockers() {
        let (port, captured) = spawn_server(|_request| ok_json(b"[]"));
        let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
        let questions = client
            .list_questions()
            .expect("a valid empty top-level array must parse");
        assert!(questions.is_empty());
        assert!(SessionBlockers::detect(&[], &questions, "ses_1").is_empty());

        let raw = captured.lock().expect("capture mutex").clone();
        assert_eq!(
            request_target(&raw),
            "GET /question?directory=%2Ftmp%2Fws HTTP/1.1"
        );
        let text = String::from_utf8_lossy(&raw);
        assert!(text.contains(&format!("authorization: Basic {PASSWORD_BASE64}\r\n")));
    }

    #[test]
    fn list_questions_rejects_missing_or_null_required_identity_and_questions() {
        let bodies: [&[u8]; 7] = [
            br#"[{"sessionID":"ses_1","questions":[]}]"#,
            br#"[{"id":null,"sessionID":"ses_1","questions":[]}]"#,
            br#"[{"id":"que_1","questions":[]}]"#,
            br#"[{"id":"que_1","sessionID":null,"questions":[]}]"#,
            br#"[{"id":"que_1","sessionID":"ses_1"}]"#,
            br#"[{"id":"que_1","sessionID":"ses_1","questions":null}]"#,
            br#"[{"id":"que_1","sessionID":"ses_1","questions":{}}]"#,
        ];
        for body in bodies {
            assert_eq!(
                parse_question_list(body).expect_err("must fail closed"),
                QuestionError::Malformed,
                "body {body:?} must be malformed rather than a missing/empty default"
            );
        }
    }

    #[test]
    fn list_questions_does_not_skip_a_malformed_question_between_valid_ones() {
        let body = br#"[
            {"id":"que_1","sessionID":"ses_1","questions":[]},
            {"id":"que_2","sessionID":"ses_2"},
            {"id":"que_3","sessionID":"ses_3","questions":[]}
        ]"#;
        assert_eq!(
            parse_question_list(body).expect_err("must not skip the damaged element"),
            QuestionError::Malformed
        );
    }

    #[test]
    fn list_questions_malformed_response_fails_closed() {
        let bodies: [&[u8]; 5] = [b"not-json", b"{}", b"null", b"\"x\"", b"1"];
        for body in bodies {
            let (port, _captured) = spawn_server(move |_request| ok_json(body));
            let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
            assert_eq!(
                client.list_questions().expect_err("must fail closed"),
                QuestionError::Malformed,
                "body {body:?} must be malformed"
            );
        }
    }

    #[test]
    fn list_questions_fails_closed_on_malformed_elements_and_fields() {
        let bodies: [&[u8]; 14] = [
            br#"[42]"#,
            br#"[{"id":42,"sessionID":"ses_1","questions":[]}]"#,
            br#"[{"id":"que_1","sessionID":42,"questions":[]}]"#,
            br#"[{"id":"que_1","sessionID":"ses_1","questions":[42]}]"#,
            br#"[{"id":"que_1","sessionID":"ses_1","questions":[{"header":"h","options":[]}]}]"#,
            br#"[{"id":"que_1","sessionID":"ses_1","questions":[{"question":"q","options":[]}]}]"#,
            br#"[{"id":"que_1","sessionID":"ses_1","questions":[{"question":"q","header":"h"}]}]"#,
            br#"[{"id":"que_1","sessionID":"ses_1","questions":[{"question":"q","header":"h","options":null}]}]"#,
            br#"[{"id":"que_1","sessionID":"ses_1","questions":[{"question":"q","header":"h","options":[42]}]}]"#,
            br#"[{"id":"que_1","sessionID":"ses_1","questions":[{"question":"q","header":"h","options":[{"description":"d"}]}]}]"#,
            br#"[{"id":"que_1","sessionID":"ses_1","questions":[{"question":"q","header":"h","options":[{"label":"l"}]}]}]"#,
            br#"[{"id":"que_1","sessionID":"ses_1","questions":[{"question":"q","header":"h","options":[],"multiple":"yes"}]}]"#,
            br#"[{"id":"que_1","sessionID":"ses_1","questions":[{"question":"q","header":"h","options":[],"custom":1}]}]"#,
            br#"[{"id":"que_1","sessionID":"ses_1","questions":[],"tool":"x"}]"#,
        ];
        for body in bodies {
            assert_eq!(
                parse_question_list(body).expect_err("must fail closed"),
                QuestionError::Malformed,
                "body {body:?} must be malformed rather than skipped"
            );
        }
    }

    #[test]
    fn list_questions_preserves_transport_errors() {
        let cases = [
            (401_u16, TransportError::Unauthorized),
            (404, TransportError::NotFound),
            (500, TransportError::HttpStatus(500)),
        ];
        for (status, expected) in cases {
            let (port, _captured) = spawn_server(move |_request| error_response(status));
            let (_dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
            assert_eq!(
                client.list_questions().expect_err("must fail"),
                QuestionError::Transport(expected)
            );
        }
    }

    #[test]
    fn list_questions_preserves_unavailable_and_timeout() {
        let port = {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
                .expect("server must bind an ephemeral port");
            listener
                .local_addr()
                .expect("bound address must be available")
                .port()
        };
        let (_dir, endpoint, secret) = project_material(port);
        let transport =
            HttpTransport::new(endpoint, BasicAuth::new(secret), Duration::from_secs(2));
        let client = OpenCodeClient::new(transport, PathBuf::from("/tmp/ws"));
        assert_eq!(
            client.list_questions().expect_err("must fail"),
            QuestionError::Transport(TransportError::Unavailable)
        );

        let (port, captured) = spawn_server(|_request| ok_json(b"[]"));
        let (_dir2, endpoint, secret) = project_material(port);
        let transport = HttpTransport::new(endpoint, BasicAuth::new(secret), Duration::ZERO);
        let client = OpenCodeClient::new(transport, PathBuf::from("/tmp/ws"));
        assert_eq!(
            client.list_questions().expect_err("must fail"),
            QuestionError::Transport(TransportError::Timeout)
        );
        assert!(
            captured.lock().expect("capture mutex").is_empty(),
            "a zero timeout must fail before sending"
        );
    }

    #[test]
    fn session_blockers_filter_by_session_without_mixing() {
        let permissions = parse_permission_list(
            br#"[
                {"id":"per_1","sessionID":"ses_1","permission":"bash","patterns":["ls"]},
                {"id":"per_2","sessionID":"ses_2","permission":"read","patterns":[]}
            ]"#,
        )
        .expect("permissions must parse");
        let questions = parse_question_list(
            br#"[
                {"id":"que_1","sessionID":"ses_1","questions":[{"question":"Which db?","header":"h","options":[]}]},
                {"id":"que_2","sessionID":"ses_2","questions":[{"question":"Other?","header":"h","options":[]}]},
                {"id":"que_3","sessionID":"ses_3","questions":[]}
            ]"#,
        )
        .expect("questions must parse");

        let first = SessionBlockers::detect(&permissions, &questions, "ses_1");
        assert!(!first.is_empty());
        assert_eq!(first.permissions().len(), 1);
        assert_eq!(first.permissions()[0].id(), Some("per_1"));
        assert!(first.permissions()[0].belongs_to_session("ses_1"));
        assert_eq!(first.questions().len(), 1);
        assert_eq!(first.questions()[0].kind(), "question");
        assert_eq!(first.questions()[0].text(), "Which db?");
        assert_eq!(
            first.to_string(),
            "OpenCode session blockers (permissions: 1, questions: 1)"
        );

        let second = SessionBlockers::detect(&permissions, &questions, "ses_2");
        assert_eq!(second.permissions().len(), 1);
        assert_eq!(second.permissions()[0].id(), Some("per_2"));
        assert_eq!(second.questions().len(), 1);
        assert_eq!(second.questions()[0].text(), "Other?");

        // A matching request is a blocker even when its nested `questions` is
        // empty (reference `_pending_questions` appends the empty text), while
        // an unknown session is empty: no cross-session mixing.
        let third = SessionBlockers::detect(&permissions, &questions, "ses_3");
        assert!(!third.is_empty());
        assert!(third.permissions().is_empty());
        assert_eq!(third.questions().len(), 1);
        assert_eq!(third.questions()[0].text(), "");
        assert!(SessionBlockers::detect(&permissions, &questions, "ses_unknown").is_empty());
    }

    #[test]
    fn question_blocker_uses_first_question_and_truncates_like_reference() {
        let long = "x".repeat(350);
        let body = serde_json::json!([
            {
                "id": "que_1",
                "sessionID": "ses_1",
                "questions": [
                    {"question": long, "header": "h", "options": []},
                    {"question": "second", "header": "h", "options": []}
                ]
            }
        ]);
        let questions = parse_question_list(body.to_string().as_bytes()).expect("must parse");
        let blocker = questions[0].blocker();
        assert_eq!(blocker.text().len(), QUESTION_BLOCKER_TEXT_LIMIT);
        assert_eq!(blocker.text().chars().count(), 300);
        assert_eq!(blocker.text(), "x".repeat(300));

        // The reference slices Python strings by code point, so a 301-character
        // multi-byte string truncates to exactly 300 code points (600 bytes).
        let unicode = "é".repeat(301);
        let body = serde_json::json!([
            {"id":"que_2","sessionID":"ses_2","questions":[{"question":unicode,"header":"h","options":[]}]}
        ]);
        let questions = parse_question_list(body.to_string().as_bytes()).expect("must parse");
        let blocker = questions[0].blocker();
        assert_eq!(blocker.text().chars().count(), 300);
        assert_eq!(blocker.text().len(), 600);

        // An empty nested question list yields the empty text, like the
        // reference `text = ""` default.
        let empty = parse_question_list(br#"[{"id":"que_3","sessionID":"ses_3","questions":[]}]"#)
            .expect("must parse");
        assert_eq!(empty[0].blocker().text(), "");
    }

    #[test]
    fn question_and_blocker_types_never_render_content_workspace_or_credentials() {
        let (port, _captured) = spawn_server(|_request| ok_json(b"[]"));
        let (dir, client) = client_for(port, PathBuf::from("/tmp/ws"));
        let workspace_text = client.workspace().to_string_lossy().into_owned();
        let _keep = dir;

        let questions = parse_question_list(
            br#"[{"id":"que_secret","sessionID":"ses_secret","questions":[{"question":"secret-question","header":"secret-header","options":[{"label":"secret-label","description":"secret-description"}],"multiple":true,"custom":false}],"tool":{"messageID":"msg_secret","callID":"call_secret"}}]"#,
        )
        .expect("must parse");
        let question = &questions[0];
        let info = &question.questions()[0];
        let option = &info.options()[0];
        let tool = question.tool().expect("tool must parse");
        let blocker = question.blocker();

        for rendered in [
            format!("{client:?}"),
            format!("{question:?}"),
            format!("{question}"),
            format!("{info:?}"),
            format!("{info}"),
            format!("{option:?}"),
            format!("{option}"),
            format!("{tool:?}"),
            format!("{tool}"),
            format!("{blocker:?}"),
            format!("{blocker}"),
        ] {
            assert!(!rendered.contains(&workspace_text));
            assert!(!rendered.contains("que_secret"));
            assert!(!rendered.contains("ses_secret"));
            assert!(!rendered.contains("secret-question"));
            assert!(!rendered.contains("secret-header"));
            assert!(!rendered.contains("secret-label"));
            assert!(!rendered.contains("secret-description"));
            assert!(!rendered.contains("msg_secret"));
            assert!(!rendered.contains("call_secret"));
            assert!(!rendered.contains(PASSWORD));
            assert!(!rendered.contains(PASSWORD_BASE64));
        }

        let blockers = SessionBlockers::detect(&[], &questions, "ses_secret");
        for rendered in [format!("{blockers:?}"), format!("{blockers}")] {
            assert!(!rendered.contains("secret-question"));
            assert!(!rendered.contains(PASSWORD));
            assert!(!rendered.contains(PASSWORD_BASE64));
        }

        for error in [
            QuestionError::Malformed,
            QuestionError::Transport(TransportError::Unauthorized),
            QuestionError::Transport(TransportError::Timeout),
            QuestionError::Transport(TransportError::Unavailable),
        ] {
            let rendered = format!("{error} {error:?}");
            assert!(!rendered.contains(&workspace_text));
            assert!(!rendered.contains("que_secret"));
            assert!(!rendered.contains(PASSWORD));
            assert!(!rendered.contains(PASSWORD_BASE64));
        }
    }
}
