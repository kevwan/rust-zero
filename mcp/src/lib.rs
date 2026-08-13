//! Model Context Protocol server support.
//!
//! Supports the 2025-03-26 Streamable HTTP transport and the legacy 2024-11-05
//! HTTP+SSE transport. A server can be mounted in any Actix application and
//! shares its normal middleware, listener, and graceful-shutdown lifecycle.

use actix_web::{
    http::{header, StatusCode},
    web, App, HttpRequest, HttpResponse, HttpServer,
};
use futures::{
    future::{AbortHandle, Abortable, BoxFuture},
    Stream,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    fmt,
    future::Future,
    io,
    net::{SocketAddr, TcpListener},
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex, RwLock,
    },
    time::{Duration, Instant},
};
use tokio::sync::broadcast;
use uuid::Uuid;

pub const LATEST_PROTOCOL_VERSION: &str = "2025-03-26";
pub const LEGACY_PROTOCOL_VERSION: &str = "2024-11-05";

/// HTTP transport endpoints installed by [`McpServer::configure`].
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpTransport {
    /// The current single-endpoint 2025-03-26 transport.
    #[default]
    StreamableHttp,
    /// The deprecated two-endpoint 2024-11-05 HTTP+SSE transport.
    LegacySse,
    /// Install both transports for old and new clients.
    Both,
}

impl McpTransport {
    fn streamable_http(self) -> bool {
        matches!(self, Self::StreamableHttp | Self::Both)
    }

    fn legacy_sse(self) -> bool {
        matches!(self, Self::LegacySse | Self::Both)
    }
}

/// Configuration for the MCP HTTP transports.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct McpServerConfig {
    pub address: SocketAddr,
    pub workers: usize,
    pub shutdown_timeout_ms: u64,
    pub endpoint: String,
    /// Selects which HTTP transport routes are installed.
    pub transport: McpTransport,
    /// Legacy HTTP+SSE connection endpoint.
    pub legacy_sse_endpoint: String,
    /// Legacy HTTP+SSE client-to-server message endpoint.
    pub legacy_message_endpoint: String,
    pub name: String,
    pub version: String,
    pub message_timeout_ms: u64,
    /// Enables MCP sessions and the GET/DELETE transport methods.
    pub stateful: bool,
    /// Idle sessions are rejected and lazily removed after this interval.
    pub session_idle_timeout_ms: u64,
    /// Number of SSE events retained per session for `Last-Event-ID` replay.
    pub event_replay_capacity: usize,
    /// Permitted browser origins. An empty list rejects requests carrying an
    /// `Origin` header while allowing non-browser clients.
    pub allowed_origins: Vec<String>,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            address: "127.0.0.1:8081".parse().unwrap(),
            workers: 1,
            shutdown_timeout_ms: 30_000,
            endpoint: "/mcp".into(),
            transport: McpTransport::StreamableHttp,
            legacy_sse_endpoint: "/sse".into(),
            legacy_message_endpoint: "/message".into(),
            name: "rust-zero-mcp".into(),
            version: "1.0.0".into(),
            message_timeout_ms: 30_000,
            stateful: false,
            session_idle_timeout_ms: 30 * 60 * 1_000,
            event_replay_capacity: 256,
            allowed_origins: Vec::new(),
        }
    }
}

impl McpServerConfig {
    pub fn validate(&self) -> Result<(), McpConfigError> {
        if self.workers == 0 {
            return Err(McpConfigError("workers must be positive"));
        }
        if self.shutdown_timeout_ms == 0 {
            return Err(McpConfigError("shutdown_timeout_ms must be positive"));
        }
        if self.transport.streamable_http()
            && (!self.endpoint.starts_with('/') || self.endpoint.contains('?'))
        {
            return Err(McpConfigError("endpoint must be an absolute path"));
        }
        if self.transport.legacy_sse()
            && (!is_absolute_path(&self.legacy_sse_endpoint)
                || !is_absolute_path(&self.legacy_message_endpoint))
        {
            return Err(McpConfigError(
                "legacy endpoints must be absolute paths without queries",
            ));
        }
        if self.transport.legacy_sse() && self.legacy_sse_endpoint == self.legacy_message_endpoint {
            return Err(McpConfigError("legacy endpoints must be distinct"));
        }
        if self.transport == McpTransport::Both
            && (self.endpoint == self.legacy_sse_endpoint
                || self.endpoint == self.legacy_message_endpoint)
        {
            return Err(McpConfigError("transport endpoints must be distinct"));
        }
        if self.name.trim().is_empty() {
            return Err(McpConfigError("name must not be empty"));
        }
        if self.version.trim().is_empty() {
            return Err(McpConfigError("version must not be empty"));
        }
        if self.message_timeout_ms == 0 {
            return Err(McpConfigError("message_timeout_ms must be positive"));
        }
        if self.stateful && self.session_idle_timeout_ms == 0 {
            return Err(McpConfigError("session_idle_timeout_ms must be positive"));
        }
        if self.stateful && self.event_replay_capacity == 0 {
            return Err(McpConfigError("event_replay_capacity must be positive"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct McpConfigError(&'static str);

impl fmt::Display for McpConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for McpConfigError {}

/// Selected request metadata copied at the HTTP boundary for use by handlers.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RequestMetadata {
    pub headers: BTreeMap<String, String>,
    pub query: BTreeMap<String, String>,
    pub path: BTreeMap<String, String>,
}

impl RequestMetadata {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }

    pub fn query(&self, name: &str) -> Option<&str> {
        self.query.get(name).map(String::as_str)
    }

    pub fn path(&self, name: &str) -> Option<&str> {
        self.path.get(name).map(String::as_str)
    }

    fn from_request(request: &HttpRequest) -> Self {
        let headers = request
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (name.as_str().to_ascii_lowercase(), value.to_owned()))
            })
            .collect();
        let query = web::Query::<BTreeMap<String, String>>::from_query(request.query_string())
            .map(|query| query.into_inner())
            .unwrap_or_default();
        let path = request
            .match_info()
            .iter()
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
            .collect();
        Self {
            headers,
            query,
            path,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub input_schema: Value,
}

impl Tool {
    pub fn new(name: impl Into<String>, input_schema: Value) -> Self {
        Self {
            name: name.into(),
            description: None,
            input_schema,
        }
    }

    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Resource {
    pub uri: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Prompt {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arguments: Vec<PromptArgument>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PromptArgument {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub required: bool,
}

/// A handler failure returned as a protocol-compliant JSON-RPC error.
#[derive(Clone, Debug)]
pub struct McpError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl McpError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(-32602, message)
    }

    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
}

type Handler = Arc<
    dyn Fn(RequestMetadata, Value) -> BoxFuture<'static, Result<Value, McpError>> + Send + Sync,
>;

#[derive(Clone)]
struct Registered<T> {
    definition: T,
    handler: Handler,
}

#[derive(Default)]
struct Registry {
    tools: RwLock<BTreeMap<String, Registered<Tool>>>,
    resources: RwLock<BTreeMap<String, Registered<Resource>>>,
    prompts: RwLock<BTreeMap<String, Registered<Prompt>>>,
}

const SESSION_HEADER: &str = "mcp-session-id";
const LAST_EVENT_ID_HEADER: &str = "last-event-id";

#[derive(Clone)]
enum SessionEvent {
    Message(StoredEvent),
    Terminated,
}

#[derive(Clone)]
struct StoredEvent {
    id: u64,
    payload: Value,
}

struct Session {
    id: String,
    last_access: Mutex<Instant>,
    next_event_id: AtomicU64,
    events: Mutex<VecDeque<StoredEvent>>,
    sender: broadcast::Sender<SessionEvent>,
    requests: Mutex<HashMap<String, AbortHandle>>,
    terminated: AtomicBool,
    active_streams: AtomicUsize,
    replay_capacity: usize,
}

impl Session {
    fn new(replay_capacity: usize) -> Arc<Self> {
        let (sender, _) = broadcast::channel(replay_capacity.max(16));
        Arc::new(Self {
            id: Uuid::new_v4().to_string(),
            last_access: Mutex::new(Instant::now()),
            next_event_id: AtomicU64::new(1),
            events: Mutex::new(VecDeque::with_capacity(replay_capacity)),
            sender,
            requests: Mutex::new(HashMap::new()),
            terminated: AtomicBool::new(false),
            active_streams: AtomicUsize::new(0),
            replay_capacity,
        })
    }

    fn touch(&self) {
        *self.last_access.lock().unwrap() = Instant::now();
    }

    fn is_expired(&self, idle_timeout: Duration) -> bool {
        self.terminated.load(Ordering::Acquire)
            || (self.active_streams.load(Ordering::Acquire) == 0
                && self.requests.lock().unwrap().is_empty()
                && self.last_access.lock().unwrap().elapsed() >= idle_timeout)
    }

    fn publish(&self, payload: Value) -> StoredEvent {
        let event = StoredEvent {
            id: self.next_event_id.fetch_add(1, Ordering::Relaxed),
            payload,
        };
        let mut events = self.events.lock().unwrap();
        if events.len() == self.replay_capacity {
            events.pop_front();
        }
        events.push_back(event.clone());
        drop(events);
        let _ = self.sender.send(SessionEvent::Message(event.clone()));
        event
    }

    fn cancel(&self, id: &Value) -> bool {
        let key = request_key(id);
        self.requests
            .lock()
            .unwrap()
            .get(&key)
            .map(|handle| {
                handle.abort();
                true
            })
            .unwrap_or(false)
    }

    fn terminate(&self) {
        if !self.terminated.swap(true, Ordering::AcqRel) {
            for handle in self.requests.lock().unwrap().values() {
                handle.abort();
            }
            let _ = self.sender.send(SessionEvent::Terminated);
        }
    }

    fn close_event_streams(&self) {
        let _ = self.sender.send(SessionEvent::Terminated);
    }

    fn event_stream(
        self: &Arc<Self>,
        after: u64,
    ) -> Pin<Box<dyn Stream<Item = Result<web::Bytes, actix_web::Error>>>> {
        self.event_stream_after(after, VecDeque::new())
    }

    fn event_stream_after(
        self: &Arc<Self>,
        after: u64,
        prefix: VecDeque<web::Bytes>,
    ) -> Pin<Box<dyn Stream<Item = Result<web::Bytes, actix_web::Error>>>> {
        struct State {
            session: Arc<Session>,
            _active: ActiveStream,
            prefix: VecDeque<web::Bytes>,
            replay: VecDeque<StoredEvent>,
            receiver: broadcast::Receiver<SessionEvent>,
            last_id: u64,
        }

        let receiver = self.sender.subscribe();
        let replay = self
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.id > after)
            .cloned()
            .collect();
        self.active_streams.fetch_add(1, Ordering::AcqRel);
        let state = State {
            session: self.clone(),
            _active: ActiveStream(self.clone()),
            prefix,
            replay,
            receiver,
            last_id: after,
        };
        Box::pin(futures::stream::unfold(state, |mut state| async move {
            loop {
                if let Some(event) = state.prefix.pop_front() {
                    return Some((Ok(event), state));
                }
                if let Some(event) = state.replay.pop_front() {
                    state.last_id = event.id;
                    return Some((Ok(sse_event(&event)), state));
                }
                match state.receiver.recv().await {
                    Ok(SessionEvent::Message(event)) if event.id > state.last_id => {
                        state.last_id = event.id;
                        return Some((Ok(sse_event(&event)), state));
                    }
                    Ok(SessionEvent::Message(_)) => continue,
                    Ok(SessionEvent::Terminated) | Err(broadcast::error::RecvError::Closed) => {
                        return None;
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        state.replay = state
                            .session
                            .events
                            .lock()
                            .unwrap()
                            .iter()
                            .filter(|event| event.id > state.last_id)
                            .cloned()
                            .collect();
                    }
                }
            }
        }))
    }
}

struct ActiveStream(Arc<Session>);

impl Drop for ActiveStream {
    fn drop(&mut self) {
        self.0.active_streams.fetch_sub(1, Ordering::AcqRel);
        self.0.touch();
    }
}

#[derive(Default)]
struct Sessions {
    values: RwLock<HashMap<String, Arc<Session>>>,
}

/// Cloneable MCP protocol core and Actix HTTP transport handler.
#[derive(Clone)]
pub struct McpServer {
    config: McpServerConfig,
    registry: Arc<Registry>,
    sessions: Arc<Sessions>,
}

impl McpServer {
    pub fn new(config: McpServerConfig) -> Result<Self, McpConfigError> {
        config.validate()?;
        Ok(Self {
            config,
            registry: Arc::new(Registry::default()),
            sessions: Arc::new(Sessions::default()),
        })
    }

    pub fn add_tool<F, Fut>(&self, tool: Tool, handler: F)
    where
        F: Fn(RequestMetadata, Value) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<Value, McpError>> + Send + 'static,
    {
        self.registry.tools.write().unwrap().insert(
            tool.name.clone(),
            Registered {
                definition: tool,
                handler: Arc::new(move |metadata, params| Box::pin(handler(metadata, params))),
            },
        );
    }

    pub fn add_resource<F, Fut>(&self, resource: Resource, handler: F)
    where
        F: Fn(RequestMetadata, Value) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<Value, McpError>> + Send + 'static,
    {
        self.registry.resources.write().unwrap().insert(
            resource.uri.clone(),
            Registered {
                definition: resource,
                handler: Arc::new(move |metadata, params| Box::pin(handler(metadata, params))),
            },
        );
    }

    pub fn add_prompt<F, Fut>(&self, prompt: Prompt, handler: F)
    where
        F: Fn(RequestMetadata, Value) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<Value, McpError>> + Send + 'static,
    {
        self.registry.prompts.write().unwrap().insert(
            prompt.name.clone(),
            Registered {
                definition: prompt,
                handler: Arc::new(move |metadata, params| Box::pin(handler(metadata, params))),
            },
        );
    }

    /// Mounts the configured MCP endpoint on an Actix application.
    pub fn configure(&self, service: &mut web::ServiceConfig) {
        let data = web::Data::new(self.clone());
        if self.config.transport.streamable_http() {
            service.service(
                web::resource(self.config.endpoint.clone())
                    .app_data(data.clone())
                    .route(web::post().to(Self::http_post))
                    .route(web::get().to(Self::http_get))
                    .route(web::delete().to(Self::http_delete)),
            );
        }
        if self.config.transport.legacy_sse() {
            service.service(
                web::resource(self.config.legacy_sse_endpoint.clone())
                    .app_data(data.clone())
                    .route(web::get().to(Self::legacy_sse_get)),
            );
            service.service(
                web::resource(self.config.legacy_message_endpoint.clone())
                    .app_data(data)
                    .route(web::post().to(Self::legacy_message_post)),
            );
        }
    }

    /// Binds the configured address and starts a standalone MCP HTTP server.
    pub fn run(&self) -> io::Result<actix_web::dev::Server> {
        self.run_on(TcpListener::bind(self.config.address)?)
    }

    /// Starts a standalone MCP HTTP server on an existing listener.
    pub fn run_on(&self, listener: TcpListener) -> io::Result<actix_web::dev::Server> {
        let server = self.clone();
        let workers = self.config.workers;
        let shutdown_seconds = self.config.shutdown_timeout_ms.div_ceil(1_000);
        HttpServer::new(move || {
            let server = server.clone();
            App::new().configure(move |service| server.configure(service))
        })
        .workers(workers)
        .shutdown_timeout(shutdown_seconds)
        .listen(listener)
        .map(HttpServer::run)
    }

    /// Serves until the supplied signal resolves and then gracefully drains requests.
    pub async fn serve_until<F>(&self, shutdown: F) -> io::Result<()>
    where
        F: Future<Output = ()>,
    {
        let transport = self.run()?;
        self.drain_on_signal(transport, shutdown).await
    }

    /// Listener-based variant of [`McpServer::serve_until`].
    pub async fn serve_on_until<F>(&self, listener: TcpListener, shutdown: F) -> io::Result<()>
    where
        F: Future<Output = ()>,
    {
        let transport = self.run_on(listener)?;
        self.drain_on_signal(transport, shutdown).await
    }

    async fn drain_on_signal<F>(
        &self,
        transport: actix_web::dev::Server,
        shutdown: F,
    ) -> io::Result<()>
    where
        F: Future<Output = ()>,
    {
        use futures::future::{select, Either};

        let handle = transport.handle();
        match select(Box::pin(transport), Box::pin(shutdown)).await {
            Either::Left((result, _)) => result,
            Either::Right(((), transport)) => {
                self.close_event_streams();
                let (_, result) = futures::future::join(handle.stop(true), transport).await;
                self.terminate_sessions();
                result
            }
        }
    }

    fn close_event_streams(&self) {
        for session in self.sessions.values.read().unwrap().values() {
            session.close_event_streams();
        }
    }

    fn terminate_sessions(&self) {
        let mut sessions = self.sessions.values.write().unwrap();
        for session in sessions.values() {
            session.terminate();
        }
        sessions.clear();
    }

    fn create_session(&self) -> Arc<Session> {
        self.remove_expired_sessions();
        let session = Session::new(self.config.event_replay_capacity);
        self.sessions
            .values
            .write()
            .unwrap()
            .insert(session.id.clone(), session.clone());
        session
    }

    fn remove_expired_sessions(&self) {
        let idle_timeout = Duration::from_millis(self.config.session_idle_timeout_ms);
        let mut sessions = self.sessions.values.write().unwrap();
        sessions.retain(|_, session| {
            let retain = !session.is_expired(idle_timeout);
            if !retain {
                session.terminate();
            }
            retain
        });
    }

    fn request_session(&self, request: &HttpRequest) -> Result<Arc<Session>, HttpResponse> {
        if !self.config.stateful {
            return Err(HttpResponse::MethodNotAllowed().finish());
        }
        self.remove_expired_sessions();
        let Some(id) = request
            .headers()
            .get(SESSION_HEADER)
            .and_then(|value| value.to_str().ok())
        else {
            return Err(HttpResponse::BadRequest().json(error_response(
                Value::Null,
                McpError::new(-32600, "Mcp-Session-Id header is required"),
            )));
        };
        let session = self.sessions.values.read().unwrap().get(id).cloned();
        match session {
            Some(session) => {
                session.touch();
                Ok(session)
            }
            None => Err(HttpResponse::NotFound().json(error_response(
                Value::Null,
                McpError::new(-32002, "session not found or expired"),
            ))),
        }
    }

    fn legacy_request_session(&self, request: &HttpRequest) -> Result<Arc<Session>, HttpResponse> {
        self.remove_expired_sessions();
        let id = web::Query::<HashMap<String, String>>::from_query(request.query_string())
            .ok()
            .and_then(|query| query.get("sessionId").cloned());
        let Some(id) = id else {
            return Err(HttpResponse::BadRequest().json(error_response(
                Value::Null,
                McpError::new(-32600, "sessionId query parameter is required"),
            )));
        };
        match self.sessions.values.read().unwrap().get(&id).cloned() {
            Some(session) => {
                session.touch();
                Ok(session)
            }
            None => Err(HttpResponse::NotFound().json(error_response(
                Value::Null,
                McpError::new(-32002, "session not found or expired"),
            ))),
        }
    }

    async fn legacy_sse_get(server: web::Data<Self>, request: HttpRequest) -> HttpResponse {
        if let Some(response) = server.reject_origin(&request) {
            return response;
        }
        let accepts_sse = request
            .headers()
            .get(header::ACCEPT)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("text/event-stream"));
        if !accepts_sse {
            return HttpResponse::NotAcceptable().finish();
        }

        let session = server.create_session();
        let endpoint = format!(
            "{}?sessionId={}",
            server.config.legacy_message_endpoint, session.id
        );
        let prefix = VecDeque::from([web::Bytes::from(format!(
            "event: endpoint\ndata: {endpoint}\n\n"
        ))]);
        HttpResponse::Ok()
            .insert_header((header::CONTENT_TYPE, "text/event-stream"))
            .insert_header((header::CACHE_CONTROL, "no-cache"))
            .streaming(session.event_stream_after(0, prefix))
    }

    async fn legacy_message_post(
        server: web::Data<Self>,
        request: HttpRequest,
        body: web::Bytes,
    ) -> HttpResponse {
        if let Some(response) = server.reject_origin(&request) {
            return response;
        }
        if !has_json_content_type(&request) {
            return HttpResponse::UnsupportedMediaType().json(error_response(
                Value::Null,
                McpError::new(-32600, "Content-Type must be application/json"),
            ));
        }
        let message = match parse_json_rpc(&body) {
            Ok(message) => message,
            Err(response) => return response,
        };
        let session = match server.legacy_request_session(&request) {
            Ok(session) => session,
            Err(response) => return response,
        };

        if message.id.is_none() {
            if message.method == "notifications/cancelled" {
                if let Some(request_id) = message.params.get("requestId") {
                    session.cancel(request_id);
                }
            }
            return HttpResponse::Accepted().finish();
        }

        let id = message.id.unwrap();
        let metadata = RequestMetadata::from_request(&request);
        let (abort_handle, abort_registration) = AbortHandle::new_pair();
        session
            .requests
            .lock()
            .unwrap()
            .insert(request_key(&id), abort_handle);
        let server = server.get_ref().clone();
        actix_web::rt::spawn(async move {
            let outcome = actix_web::rt::time::timeout(
                Duration::from_millis(server.config.message_timeout_ms),
                Abortable::new(
                    server.dispatch(
                        &message.method,
                        metadata,
                        message.params,
                        LEGACY_PROTOCOL_VERSION,
                    ),
                    abort_registration,
                ),
            )
            .await;
            session.requests.lock().unwrap().remove(&request_key(&id));
            session.touch();
            let response = match outcome {
                Ok(Ok(Ok(result))) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                Ok(Ok(Err(error))) => error_response(id, error),
                Ok(Err(_)) => error_response(id, McpError::new(-32800, "request cancelled")),
                Err(_) => error_response(id, McpError::new(-32001, "request timed out")),
            };
            session.publish(response);
        });
        HttpResponse::Accepted().finish()
    }

    async fn http_get(server: web::Data<Self>, request: HttpRequest) -> HttpResponse {
        if let Some(response) = server.reject_origin(&request) {
            return response;
        }
        let accepts_sse = request
            .headers()
            .get(header::ACCEPT)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("text/event-stream"));
        if !accepts_sse {
            return HttpResponse::NotAcceptable().finish();
        }
        let session = match server.request_session(&request) {
            Ok(session) => session,
            Err(response) => return response,
        };
        let after = match request.headers().get(LAST_EVENT_ID_HEADER) {
            Some(value) => match value
                .to_str()
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
            {
                Some(value) => value,
                None => {
                    return HttpResponse::BadRequest().json(error_response(
                        Value::Null,
                        McpError::invalid_params("Last-Event-ID must be an unsigned integer"),
                    ));
                }
            },
            None => session
                .next_event_id
                .load(Ordering::Acquire)
                .saturating_sub(1),
        };
        HttpResponse::Ok()
            .insert_header((header::CONTENT_TYPE, "text/event-stream"))
            .insert_header((header::CACHE_CONTROL, "no-cache"))
            .insert_header((SESSION_HEADER, session.id.clone()))
            .streaming(session.event_stream(after))
    }

    async fn http_delete(server: web::Data<Self>, request: HttpRequest) -> HttpResponse {
        if let Some(response) = server.reject_origin(&request) {
            return response;
        }
        let session = match server.request_session(&request) {
            Ok(session) => session,
            Err(response) => return response,
        };
        server.sessions.values.write().unwrap().remove(&session.id);
        session.terminate();
        HttpResponse::NoContent().finish()
    }

    fn reject_origin(&self, request: &HttpRequest) -> Option<HttpResponse> {
        let origin = request.headers().get(header::ORIGIN)?;
        let allowed = origin.to_str().ok().is_some_and(|origin| {
            self.config
                .allowed_origins
                .iter()
                .any(|item| item == origin)
        });
        (!allowed).then(|| {
            HttpResponse::Forbidden().json(error_response(
                Value::Null,
                McpError::new(-32000, "origin is not allowed"),
            ))
        })
    }

    async fn http_post(
        server: web::Data<Self>,
        request: HttpRequest,
        body: web::Bytes,
    ) -> HttpResponse {
        if let Some(response) = server.reject_origin(&request) {
            return response;
        }

        if !has_json_content_type(&request) {
            return HttpResponse::UnsupportedMediaType().json(error_response(
                Value::Null,
                McpError::new(-32600, "Content-Type must be application/json"),
            ));
        }

        let message = match parse_json_rpc(&body) {
            Ok(message) => message,
            Err(response) => return response,
        };

        let session = if server.config.stateful {
            if message.method == "initialize" {
                Some(server.create_session())
            } else {
                match server.request_session(&request) {
                    Ok(session) => Some(session),
                    Err(response) => return response,
                }
            }
        } else {
            None
        };

        // Notifications deliberately have no JSON-RPC response. Cancellation
        // is still dispatched so it can abort a concurrent request.
        if message.id.is_none() {
            if message.method == "notifications/cancelled" {
                if let (Some(session), Some(request_id)) =
                    (session.as_ref(), message.params.get("requestId"))
                {
                    session.cancel(request_id);
                }
            }
            return HttpResponse::Accepted().finish();
        }
        let id = message.id.unwrap();
        let metadata = RequestMetadata::from_request(&request);
        let (abort_handle, abort_registration) = AbortHandle::new_pair();
        if let Some(session) = session.as_ref() {
            session
                .requests
                .lock()
                .unwrap()
                .insert(request_key(&id), abort_handle);
        }
        let outcome = actix_web::rt::time::timeout(
            Duration::from_millis(server.config.message_timeout_ms),
            Abortable::new(
                server.dispatch(
                    &message.method,
                    metadata,
                    message.params,
                    LATEST_PROTOCOL_VERSION,
                ),
                abort_registration,
            ),
        )
        .await;
        if let Some(session) = session.as_ref() {
            session.requests.lock().unwrap().remove(&request_key(&id));
            session.touch();
        }
        let response = match outcome {
            Ok(Ok(Ok(result))) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Ok(Ok(Err(error))) => error_response(id, error),
            Ok(Err(_)) => error_response(id, McpError::new(-32800, "request cancelled")),
            Err(_) => error_response(id, McpError::new(-32001, "request timed out")),
        };

        let accepts_json = request
            .headers()
            .get(header::ACCEPT)
            .and_then(|value| value.to_str().ok())
            .is_none_or(|value| value.contains("application/json") || value.contains("*/*"));
        let session_header = session.as_ref().map(|session| session.id.clone());
        if accepts_json {
            let mut builder = HttpResponse::Ok();
            if let Some(session_id) = session_header {
                builder.insert_header((SESSION_HEADER, session_id));
            }
            builder.json(response)
        } else if request
            .headers()
            .get(header::ACCEPT)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("text/event-stream"))
        {
            let event = session
                .as_ref()
                .map(|session| session.publish(response.clone()));
            let mut builder = HttpResponse::Ok();
            if let Some(session_id) = session_header {
                builder.insert_header((SESSION_HEADER, session_id));
            }
            builder
                .insert_header((header::CONTENT_TYPE, "text/event-stream"))
                .insert_header((header::CACHE_CONTROL, "no-cache"))
                .body(event.map_or_else(
                    || format!("event: message\ndata: {response}\n\n"),
                    |event| String::from_utf8_lossy(&sse_event(&event)).into_owned(),
                ))
        } else {
            HttpResponse::build(StatusCode::NOT_ACCEPTABLE).finish()
        }
    }

    async fn dispatch(
        &self,
        method: &str,
        metadata: RequestMetadata,
        params: Value,
        protocol_version: &'static str,
    ) -> Result<Value, McpError> {
        match method {
            "initialize" => Ok(json!({
                "protocolVersion": protocol_version,
                "capabilities": {
                    "tools": {"listChanged": false},
                    "resources": {"subscribe": false, "listChanged": false},
                    "prompts": {"listChanged": false}
                },
                "serverInfo": {"name": self.config.name, "version": self.config.version}
            })),
            "ping" => Ok(json!({})),
            "tools/list" => {
                let tools = self
                    .registry
                    .tools
                    .read()
                    .unwrap()
                    .values()
                    .map(|entry| entry.definition.clone())
                    .collect::<Vec<_>>();
                Ok(json!({"tools": tools}))
            }
            "resources/list" => {
                let resources = self
                    .registry
                    .resources
                    .read()
                    .unwrap()
                    .values()
                    .map(|entry| entry.definition.clone())
                    .collect::<Vec<_>>();
                Ok(json!({"resources": resources}))
            }
            "prompts/list" => {
                let prompts = self
                    .registry
                    .prompts
                    .read()
                    .unwrap()
                    .values()
                    .map(|entry| entry.definition.clone())
                    .collect::<Vec<_>>();
                Ok(json!({"prompts": prompts}))
            }
            "tools/call" => {
                let name = required_string(&params, "name")?;
                let arguments = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let handler = self
                    .registry
                    .tools
                    .read()
                    .unwrap()
                    .get(name)
                    .map(|entry| entry.handler.clone())
                    .ok_or_else(|| McpError::new(-32602, format!("unknown tool: {name}")))?;
                handler(metadata, arguments).await
            }
            "resources/read" => {
                let uri = required_string(&params, "uri")?;
                let handler = self
                    .registry
                    .resources
                    .read()
                    .unwrap()
                    .get(uri)
                    .map(|entry| entry.handler.clone())
                    .ok_or_else(|| McpError::new(-32602, format!("unknown resource: {uri}")))?;
                handler(metadata, params).await
            }
            "prompts/get" => {
                let name = required_string(&params, "name")?;
                let handler = self
                    .registry
                    .prompts
                    .read()
                    .unwrap()
                    .get(name)
                    .map(|entry| entry.handler.clone())
                    .ok_or_else(|| McpError::new(-32602, format!("unknown prompt: {name}")))?;
                handler(metadata, params).await
            }
            _ => Err(McpError::new(-32601, "method not found")),
        }
    }
}

#[derive(Deserialize)]
struct JsonRpcRequest {
    jsonrpc: String,
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default = "empty_object")]
    params: Value,
}

fn is_absolute_path(path: &str) -> bool {
    path.starts_with('/') && !path.contains('?')
}

fn has_json_content_type(request: &HttpRequest) -> bool {
    request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.split(';').next() == Some("application/json"))
}

fn parse_json_rpc(body: &[u8]) -> Result<JsonRpcRequest, HttpResponse> {
    let message: JsonRpcRequest = serde_json::from_slice(body).map_err(|error| {
        HttpResponse::BadRequest().json(error_response(
            Value::Null,
            McpError::new(-32700, "parse error").with_data(json!({"detail": error.to_string()})),
        ))
    })?;
    if message.jsonrpc != "2.0" || message.method.is_empty() {
        return Err(HttpResponse::BadRequest().json(error_response(
            message.id.unwrap_or(Value::Null),
            McpError::new(-32600, "invalid JSON-RPC request"),
        )));
    }
    Ok(message)
}

fn empty_object() -> Value {
    json!({})
}

fn required_string<'a>(params: &'a Value, key: &str) -> Result<&'a str, McpError> {
    params
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| McpError::invalid_params(format!("{key} must be a string")))
}

fn error_response(id: Value, error: McpError) -> Value {
    let mut payload = json!({"code": error.code, "message": error.message});
    if let Some(data) = error.data {
        payload["data"] = data;
    }
    json!({"jsonrpc": "2.0", "id": id, "error": payload})
}

fn request_key(id: &Value) -> String {
    serde_json::to_string(id).unwrap_or_else(|_| "null".into())
}

fn sse_event(event: &StoredEvent) -> web::Bytes {
    web::Bytes::from(format!(
        "id: {}\nevent: message\ndata: {}\n\n",
        event.id, event.payload
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{body::MessageBody, http::header, test, App};
    use futures::future::poll_fn;

    fn server() -> McpServer {
        let server = McpServer::new(McpServerConfig {
            allowed_origins: vec!["https://client.example".into()],
            ..McpServerConfig::default()
        })
        .unwrap();
        server.add_tool(
            Tool::new("echo", json!({"type": "object"})).with_description("echo input"),
            |metadata, arguments| async move {
                Ok(json!({
                    "content": [{"type": "text", "text": arguments["text"]}],
                    "_meta": {"tenant": metadata.header("x-tenant")}
                }))
            },
        );
        server
    }

    #[actix_web::test]
    async fn deserializes_and_validates_transport_selection() {
        let config: McpServerConfig = serde_json::from_value(json!({
            "transport": "both",
            "endpoint": "/mcp",
            "legacy_sse_endpoint": "/events",
            "legacy_message_endpoint": "/send"
        }))
        .unwrap();
        assert_eq!(config.transport, McpTransport::Both);
        config.validate().unwrap();

        let invalid = McpServerConfig {
            transport: McpTransport::Both,
            legacy_sse_endpoint: "/mcp".into(),
            ..McpServerConfig::default()
        };
        assert_eq!(
            invalid.validate().unwrap_err().to_string(),
            "transport endpoints must be distinct"
        );
    }

    #[actix_web::test]
    async fn initializes_and_advertises_capabilities() {
        let server = server();
        let app = test::init_service(App::new().configure(|cfg| server.configure(cfg))).await;
        let request = test::TestRequest::post()
            .uri("/mcp")
            .insert_header((header::CONTENT_TYPE, "application/json"))
            .set_payload(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
            .to_request();
        let response: Value = test::call_and_read_body_json(&app, request).await;
        assert_eq!(
            response["result"]["protocolVersion"],
            LATEST_PROTOCOL_VERSION
        );
        assert_eq!(response["result"]["serverInfo"]["name"], "rust-zero-mcp");
        assert!(response["result"]["capabilities"]["tools"].is_object());
    }

    #[actix_web::test]
    async fn lists_and_calls_tools_with_request_metadata() {
        let server = server();
        let app = test::init_service(App::new().configure(|cfg| server.configure(cfg))).await;
        let list = test::TestRequest::post()
            .uri("/mcp")
            .insert_header((header::CONTENT_TYPE, "application/json"))
            .set_json(json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}))
            .to_request();
        let listed: Value = test::call_and_read_body_json(&app, list).await;
        assert_eq!(listed["result"]["tools"][0]["name"], "echo");

        let call = test::TestRequest::post()
            .uri("/mcp?trace=abc")
            .insert_header((header::CONTENT_TYPE, "application/json"))
            .insert_header(("x-tenant", "acme"))
            .set_json(json!({
                "jsonrpc":"2.0", "id":"call-1", "method":"tools/call",
                "params":{"name":"echo", "arguments":{"text":"hello"}}
            }))
            .to_request();
        let called: Value = test::call_and_read_body_json(&app, call).await;
        assert_eq!(called["result"]["content"][0]["text"], "hello");
        assert_eq!(called["result"]["_meta"]["tenant"], "acme");
    }

    #[actix_web::test]
    async fn dispatches_resources_and_prompts() {
        let server = server();
        server.add_resource(
            Resource {
                uri: "file:///guide.md".into(),
                name: "guide".into(),
                description: Some("project guide".into()),
                mime_type: Some("text/markdown".into()),
            },
            |_, params| async move {
                Ok(json!({"contents": [{
                    "uri": params["uri"], "mimeType": "text/markdown", "text": "guide"
                }]}))
            },
        );
        server.add_prompt(
            Prompt {
                name: "review".into(),
                description: Some("review code".into()),
                arguments: vec![PromptArgument {
                    name: "code".into(),
                    description: None,
                    required: true,
                }],
            },
            |_, params| async move {
                Ok(json!({"messages": [{
                    "role": "user",
                    "content": {"type": "text", "text": params["arguments"]["code"]}
                }]}))
            },
        );
        let app = test::init_service(App::new().configure(|cfg| server.configure(cfg))).await;

        let resource = test::TestRequest::post()
            .uri("/mcp")
            .insert_header((header::CONTENT_TYPE, "application/json"))
            .set_json(json!({
                "jsonrpc":"2.0", "id":1, "method":"resources/read",
                "params":{"uri":"file:///guide.md"}
            }))
            .to_request();
        let resource: Value = test::call_and_read_body_json(&app, resource).await;
        assert_eq!(resource["result"]["contents"][0]["text"], "guide");

        let prompt = test::TestRequest::post()
            .uri("/mcp")
            .insert_header((header::CONTENT_TYPE, "application/json"))
            .set_json(json!({
                "jsonrpc":"2.0", "id":2, "method":"prompts/get",
                "params":{"name":"review", "arguments":{"code":"fn main() {}"}}
            }))
            .to_request();
        let prompt: Value = test::call_and_read_body_json(&app, prompt).await;
        assert_eq!(
            prompt["result"]["messages"][0]["content"]["text"],
            "fn main() {}"
        );
    }

    #[actix_web::test]
    async fn projects_header_query_and_path_metadata() {
        let server = McpServer::new(McpServerConfig {
            endpoint: "/mcp/{scope}".into(),
            ..McpServerConfig::default()
        })
        .unwrap();
        server.add_tool(Tool::new("metadata", json!({})), |metadata, _| async move {
            Ok(json!({"content": [{
                "type": "text",
                "text": format!(
                    "{}/{}/{}",
                    metadata.header("x-tenant").unwrap_or_default(),
                    metadata.query("trace").unwrap_or_default(),
                    metadata.path("scope").unwrap_or_default()
                )
            }]}))
        });
        let app = test::init_service(App::new().configure(|cfg| server.configure(cfg))).await;
        let request = test::TestRequest::post()
            .uri("/mcp/admin?trace=abc")
            .insert_header((header::CONTENT_TYPE, "application/json"))
            .insert_header(("x-tenant", "acme"))
            .set_json(json!({
                "jsonrpc":"2.0", "id":1, "method":"tools/call",
                "params":{"name":"metadata"}
            }))
            .to_request();
        let response: Value = test::call_and_read_body_json(&app, request).await;
        assert_eq!(response["result"]["content"][0]["text"], "acme/abc/admin");
    }

    #[actix_web::test]
    async fn returns_protocol_errors_and_accepts_notifications() {
        let server = server();
        let app = test::init_service(App::new().configure(|cfg| server.configure(cfg))).await;
        let missing = test::TestRequest::post()
            .uri("/mcp")
            .insert_header((header::CONTENT_TYPE, "application/json"))
            .set_json(json!({"jsonrpc":"2.0","id":7,"method":"missing"}))
            .to_request();
        let body: Value = test::call_and_read_body_json(&app, missing).await;
        assert_eq!(body["error"]["code"], -32601);

        let notification = test::TestRequest::post()
            .uri("/mcp")
            .insert_header((header::CONTENT_TYPE, "application/json"))
            .set_json(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
            .to_request();
        let response = test::call_service(&app, notification).await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);
    }

    #[actix_web::test]
    async fn supports_sse_response_and_origin_protection() {
        let server = server();
        let app = test::init_service(App::new().configure(|cfg| server.configure(cfg))).await;
        let sse = test::TestRequest::post()
            .uri("/mcp")
            .insert_header((header::CONTENT_TYPE, "application/json"))
            .insert_header((header::ACCEPT, "text/event-stream"))
            .insert_header((header::ORIGIN, "https://client.example"))
            .set_json(json!({"jsonrpc":"2.0","id":1,"method":"ping"}))
            .to_request();
        let response = test::call_service(&app, sse).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/event-stream"
        );
        let body = test::read_body(response).await;
        assert!(std::str::from_utf8(&body)
            .unwrap()
            .starts_with("event: message\ndata: "));

        let rejected = test::TestRequest::post()
            .uri("/mcp")
            .insert_header((header::CONTENT_TYPE, "application/json"))
            .insert_header((header::ORIGIN, "https://evil.example"))
            .set_json(json!({"jsonrpc":"2.0","id":1,"method":"ping"}))
            .to_request();
        let response = test::call_service(&app, rejected).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[actix_web::test]
    async fn legacy_sse_announces_message_endpoint_and_uses_legacy_protocol() {
        let server = McpServer::new(McpServerConfig {
            transport: McpTransport::Both,
            ..McpServerConfig::default()
        })
        .unwrap();
        let app = test::init_service(App::new().configure(|cfg| server.configure(cfg))).await;

        let connect = test::TestRequest::get()
            .uri("/sse")
            .insert_header((header::ACCEPT, "text/event-stream"))
            .to_request();
        let response = test::call_service(&app, connect).await;
        assert_eq!(response.status(), StatusCode::OK);
        let mut stream = response.into_body();
        let endpoint = poll_fn(|cx| Pin::new(&mut stream).poll_next(cx))
            .await
            .unwrap()
            .unwrap();
        let endpoint = std::str::from_utf8(&endpoint).unwrap();
        assert!(endpoint.starts_with("event: endpoint\ndata: /message?sessionId="));
        let message_uri = endpoint
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .unwrap();

        let initialize = test::TestRequest::post()
            .uri(message_uri)
            .insert_header((header::CONTENT_TYPE, "application/json"))
            .set_json(json!({"jsonrpc":"2.0","id":1,"method":"initialize"}))
            .to_request();
        assert_eq!(
            test::call_service(&app, initialize).await.status(),
            StatusCode::ACCEPTED
        );

        let message = poll_fn(|cx| Pin::new(&mut stream).poll_next(cx))
            .await
            .unwrap()
            .unwrap();
        let message = std::str::from_utf8(&message).unwrap();
        assert!(message.contains("event: message"));
        let payload: Value = serde_json::from_str(
            message
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            payload["result"]["protocolVersion"],
            LEGACY_PROTOCOL_VERSION
        );
    }

    #[actix_web::test]
    async fn transport_selection_only_installs_selected_routes() {
        let server = McpServer::new(McpServerConfig {
            transport: McpTransport::LegacySse,
            ..McpServerConfig::default()
        })
        .unwrap();
        let app = test::init_service(App::new().configure(|cfg| server.configure(cfg))).await;

        let streamable = test::TestRequest::post()
            .uri("/mcp")
            .insert_header((header::CONTENT_TYPE, "application/json"))
            .set_json(json!({"jsonrpc":"2.0","id":1,"method":"initialize"}))
            .to_request();
        assert_eq!(
            test::call_service(&app, streamable).await.status(),
            StatusCode::NOT_FOUND
        );

        let legacy = test::TestRequest::get()
            .uri("/sse")
            .insert_header((header::ACCEPT, "text/event-stream"))
            .to_request();
        assert_eq!(
            test::call_service(&app, legacy).await.status(),
            StatusCode::OK
        );
    }

    fn stateful_server() -> McpServer {
        McpServer::new(McpServerConfig {
            stateful: true,
            event_replay_capacity: 8,
            ..McpServerConfig::default()
        })
        .unwrap()
    }

    async fn initialize_session<S>(app: &S) -> String
    where
        S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
        >,
    {
        let request = test::TestRequest::post()
            .uri("/mcp")
            .insert_header((header::CONTENT_TYPE, "application/json"))
            .set_json(json!({"jsonrpc":"2.0","id":1,"method":"initialize"}))
            .to_request();
        let response = test::call_service(app, request).await;
        assert_eq!(response.status(), StatusCode::OK);
        response
            .headers()
            .get(SESSION_HEADER)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned()
    }

    #[actix_web::test]
    async fn stateful_sessions_are_required_and_can_be_terminated() {
        let server = stateful_server();
        let app = test::init_service(App::new().configure(|cfg| server.configure(cfg))).await;
        let session_id = initialize_session(&app).await;

        let missing = test::TestRequest::post()
            .uri("/mcp")
            .insert_header((header::CONTENT_TYPE, "application/json"))
            .set_json(json!({"jsonrpc":"2.0","id":2,"method":"ping"}))
            .to_request();
        assert_eq!(
            test::call_service(&app, missing).await.status(),
            StatusCode::BAD_REQUEST
        );

        let delete = test::TestRequest::delete()
            .uri("/mcp")
            .insert_header((SESSION_HEADER, session_id.clone()))
            .to_request();
        assert_eq!(
            test::call_service(&app, delete).await.status(),
            StatusCode::NO_CONTENT
        );

        let expired = test::TestRequest::post()
            .uri("/mcp")
            .insert_header((header::CONTENT_TYPE, "application/json"))
            .insert_header((SESSION_HEADER, session_id))
            .set_json(json!({"jsonrpc":"2.0","id":3,"method":"ping"}))
            .to_request();
        assert_eq!(
            test::call_service(&app, expired).await.status(),
            StatusCode::NOT_FOUND
        );
    }

    #[actix_web::test]
    async fn get_stream_replays_events_after_cursor_on_reconnect() {
        let server = stateful_server();
        let app = test::init_service(App::new().configure(|cfg| server.configure(cfg))).await;
        let session_id = initialize_session(&app).await;

        for id in [10, 11] {
            let request = test::TestRequest::post()
                .uri("/mcp")
                .insert_header((header::CONTENT_TYPE, "application/json"))
                .insert_header((header::ACCEPT, "text/event-stream"))
                .insert_header((SESSION_HEADER, session_id.clone()))
                .set_json(json!({"jsonrpc":"2.0","id":id,"method":"ping"}))
                .to_request();
            assert_eq!(
                test::call_service(&app, request).await.status(),
                StatusCode::OK
            );
        }

        let reconnect = test::TestRequest::get()
            .uri("/mcp")
            .insert_header((header::ACCEPT, "text/event-stream"))
            .insert_header((SESSION_HEADER, session_id))
            .insert_header((LAST_EVENT_ID_HEADER, "1"))
            .to_request();
        let response = test::call_service(&app, reconnect).await;
        assert_eq!(response.status(), StatusCode::OK);
        let mut body = response.into_body();
        let chunk = poll_fn(|cx| Pin::new(&mut body).poll_next(cx))
            .await
            .unwrap()
            .unwrap();
        let chunk = std::str::from_utf8(&chunk).unwrap();
        assert!(chunk.starts_with("id: 2\nevent: message\n"));
        assert!(chunk.contains(r#""id":11"#));
    }

    #[actix_web::test]
    async fn cancellation_notification_aborts_an_in_flight_request() {
        let server = stateful_server();
        let started = Arc::new(tokio::sync::Notify::new());
        server.add_tool(Tool::new("wait", json!({})), {
            let started = started.clone();
            move |_, _| {
                let started = started.clone();
                async move {
                    started.notify_one();
                    futures::future::pending::<Result<Value, McpError>>().await
                }
            }
        });
        let app = test::init_service(App::new().configure(|cfg| server.configure(cfg))).await;
        let session_id = initialize_session(&app).await;

        let call = test::TestRequest::post()
            .uri("/mcp")
            .insert_header((header::CONTENT_TYPE, "application/json"))
            .insert_header((SESSION_HEADER, session_id.clone()))
            .set_json(json!({
                "jsonrpc":"2.0", "id":"slow", "method":"tools/call",
                "params":{"name":"wait"}
            }))
            .to_request();
        let cancel = async {
            started.notified().await;
            let request = test::TestRequest::post()
                .uri("/mcp")
                .insert_header((header::CONTENT_TYPE, "application/json"))
                .insert_header((SESSION_HEADER, session_id))
                .set_json(json!({
                    "jsonrpc":"2.0", "method":"notifications/cancelled",
                    "params":{"requestId":"slow", "reason":"client disconnected"}
                }))
                .to_request();
            test::call_service(&app, request).await
        };
        let (response, cancellation) = futures::join!(test::call_service(&app, call), cancel);
        assert_eq!(cancellation.status(), StatusCode::ACCEPTED);
        let body: Value = test::read_body_json(response).await;
        assert_eq!(body["error"]["code"], -32800);
    }

    #[actix_web::test]
    async fn shutdown_gracefully_drains_an_in_flight_tool_call() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let server = McpServer::new(McpServerConfig {
            message_timeout_ms: 2_000,
            shutdown_timeout_ms: 2_000,
            ..McpServerConfig::default()
        })
        .unwrap();
        server.add_tool(Tool::new("slow", json!({})), {
            let started = started.clone();
            let release = release.clone();
            move |_, _| {
                let started = started.clone();
                let release = release.clone();
                async move {
                    started.notify_one();
                    release.notified().await;
                    Ok(json!({"content": [{"type": "text", "text": "finished"}]}))
                }
            }
        });
        let (shutdown_sender, shutdown_receiver) = tokio::sync::oneshot::channel();
        let server_task = actix_web::rt::spawn(async move {
            server
                .serve_on_until(listener, async move {
                    let _ = shutdown_receiver.await;
                })
                .await
        });
        let request_task = actix_web::rt::spawn(async move {
            reqwest::Client::new()
                .post(format!("http://{address}/mcp"))
                .json(&json!({
                    "jsonrpc":"2.0", "id":1, "method":"tools/call",
                    "params":{"name":"slow"}
                }))
                .send()
                .await
        });
        actix_web::rt::time::timeout(Duration::from_secs(1), started.notified())
            .await
            .unwrap();
        shutdown_sender.send(()).unwrap();
        actix_web::rt::time::sleep(Duration::from_millis(20)).await;
        assert!(!request_task.is_finished());

        release.notify_one();
        let response = request_task.await.unwrap().unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(
            response.json::<Value>().await.unwrap()["result"]["content"][0]["text"],
            "finished"
        );
        server_task.await.unwrap().unwrap();
    }
}
