//! Descriptor-driven JSON/HTTP to gRPC transcoding.

use actix_web::{
    error::ErrorInternalServerError,
    http::{header, Method, StatusCode},
    web, HttpRequest, HttpResponse,
};
use futures::TryStreamExt;
use http::uri::PathAndQuery;
use prost::Message;
use prost_reflect::{DescriptorPool, DynamicMessage, Kind, MessageDescriptor, MethodDescriptor};
use std::{collections::BTreeMap, fmt, sync::Arc};
use tonic::{
    client::Grpc,
    codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder},
    metadata::{AsciiMetadataKey, AsciiMetadataValue},
    transport::Channel,
    Code, Request, Status,
};

/// HTTP verbs supported by protobuf HTTP bindings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpVerb {
    Get,
    Put,
    Post,
    Delete,
    Patch,
}

impl HttpVerb {
    fn matches(self, method: &Method) -> bool {
        matches!(
            (self, method.as_str()),
            (Self::Get, "GET")
                | (Self::Put, "PUT")
                | (Self::Post, "POST")
                | (Self::Delete, "DELETE")
                | (Self::Patch, "PATCH")
        )
    }
}

/// An explicit HTTP binding for a fully-qualified protobuf method.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpBinding {
    pub verb: HttpVerb,
    pub path: String,
    pub rpc: String,
    pub body: Option<String>,
}

impl HttpBinding {
    pub fn new(verb: HttpVerb, path: impl Into<String>, rpc: impl Into<String>) -> Self {
        Self {
            verb,
            path: path.into(),
            rpc: rpc.into(),
            body: None,
        }
    }

    /// Maps the complete JSON body (`"*"`) or one request field to the HTTP body.
    pub fn with_body(mut self, field: impl Into<String>) -> Self {
        self.body = Some(field.into());
        self
    }
}

#[derive(Clone)]
struct Route {
    binding: HttpBinding,
    method: MethodDescriptor,
}

/// Builds a descriptor-driven transcoder from a compiled `FileDescriptorSet`.
pub struct TranscoderBuilder {
    pool: DescriptorPool,
    channel: Channel,
    bindings: Vec<HttpBinding>,
}

impl TranscoderBuilder {
    pub fn from_descriptor_set(
        bytes: impl AsRef<[u8]>,
        channel: Channel,
    ) -> Result<Self, TranscodeError> {
        let pool = DescriptorPool::decode(bytes.as_ref()).map_err(TranscodeError::Descriptor)?;
        Ok(Self {
            pool,
            channel,
            bindings: Vec::new(),
        })
    }

    /// Loads all service descriptors advertised by the standard gRPC v1 reflection service.
    pub async fn from_reflection(channel: Channel) -> Result<Self, TranscodeError> {
        use tonic_reflection::pb::v1::{
            server_reflection_client::ServerReflectionClient,
            server_reflection_request::MessageRequest, server_reflection_response::MessageResponse,
            ServerReflectionRequest,
        };

        async fn request(
            client: &mut ServerReflectionClient<Channel>,
            message_request: MessageRequest,
        ) -> Result<MessageResponse, TranscodeError> {
            let response = client
                .server_reflection_info(futures::stream::iter([ServerReflectionRequest {
                    host: String::new(),
                    message_request: Some(message_request),
                }]))
                .await
                .map_err(|status| TranscodeError::Status(Box::new(status)))?;
            response
                .into_inner()
                .message()
                .await
                .map_err(|status| TranscodeError::Status(Box::new(status)))?
                .and_then(|response| response.message_response)
                .ok_or(TranscodeError::InvalidReflectionResponse)
        }

        let mut client = ServerReflectionClient::new(channel.clone());
        let services =
            match request(&mut client, MessageRequest::ListServices(String::new())).await? {
                MessageResponse::ListServicesResponse(response) => response.service,
                MessageResponse::ErrorResponse(error) => {
                    return Err(TranscodeError::Reflection(error.error_message));
                }
                _ => return Err(TranscodeError::InvalidReflectionResponse),
            };
        let mut files = BTreeMap::new();
        for service in services {
            if service.name.starts_with("grpc.reflection.") {
                continue;
            }
            match request(
                &mut client,
                MessageRequest::FileContainingSymbol(service.name),
            )
            .await?
            {
                MessageResponse::FileDescriptorResponse(response) => {
                    for bytes in response.file_descriptor_proto {
                        let file = prost_types::FileDescriptorProto::decode(bytes.as_slice())
                            .map_err(TranscodeError::ReflectionDescriptor)?;
                        files.insert(file.name.clone().unwrap_or_default(), file);
                    }
                }
                MessageResponse::ErrorResponse(error) => {
                    return Err(TranscodeError::Reflection(error.error_message));
                }
                _ => return Err(TranscodeError::InvalidReflectionResponse),
            }
        }
        let pool = DescriptorPool::from_file_descriptor_set(prost_types::FileDescriptorSet {
            file: files.into_values().collect(),
        })
        .map_err(TranscodeError::Descriptor)?;
        Ok(Self {
            pool,
            channel,
            bindings: Vec::new(),
        })
    }

    pub fn add_binding(mut self, binding: HttpBinding) -> Self {
        self.bindings.push(binding);
        self
    }

    /// Loads `google.api.http` primary and additional bindings when those extensions are present
    /// in the descriptor set. Explicit bindings may be mixed with annotated bindings.
    pub fn load_annotated_bindings(mut self) -> Self {
        let Some(extension) = self.pool.get_extension_by_name("google.api.http") else {
            return self;
        };
        for service in self.pool.services() {
            for method in service.methods() {
                let options = method.options();
                if !options.has_extension(&extension) {
                    continue;
                }
                let value = options.get_extension(&extension);
                let Some(rule) = value.as_message() else {
                    continue;
                };
                collect_http_rules(rule, method.full_name(), &mut self.bindings);
            }
        }
        self
    }

    pub fn build(self) -> Result<Transcoder, TranscodeError> {
        let mut routes = Vec::with_capacity(self.bindings.len());
        for binding in self.bindings {
            validate_template(&binding.path)?;
            let method = find_method(&self.pool, &binding.rpc)
                .ok_or_else(|| TranscodeError::UnknownMethod(binding.rpc.clone()))?;
            if method.is_client_streaming() {
                return Err(TranscodeError::UnsupportedClientStreaming(binding.rpc));
            }
            routes.push(Route { binding, method });
        }
        routes.sort_by_key(|route| std::cmp::Reverse(literal_weight(&route.binding.path)));
        Ok(Transcoder {
            channel: self.channel,
            routes: Arc::new(routes),
        })
    }
}

/// A cloneable Actix handler state that dynamically invokes generated-independent gRPC methods.
#[derive(Clone)]
pub struct Transcoder {
    channel: Channel,
    routes: Arc<Vec<Route>>,
}

impl Transcoder {
    pub async fn handle(&self, request: HttpRequest, body: web::Bytes) -> HttpResponse {
        match self.invoke(&request, &body).await {
            Ok(response) => response,
            Err(TranscodeError::NoRoute) => HttpResponse::NotFound().json(error_json(
                Code::NotFound,
                "no HTTP-to-gRPC binding matched the request",
            )),
            Err(TranscodeError::InvalidJson(error)) => HttpResponse::BadRequest()
                .json(error_json(Code::InvalidArgument, &error.to_string())),
            Err(TranscodeError::Status(status)) => status_response(*status),
            Err(error) => HttpResponse::InternalServerError()
                .json(error_json(Code::Internal, &error.to_string())),
        }
    }

    async fn invoke(
        &self,
        request: &HttpRequest,
        body: &[u8],
    ) -> Result<HttpResponse, TranscodeError> {
        let (route, captures) = self
            .routes
            .iter()
            .filter(|route| route.binding.verb.matches(request.method()))
            .find_map(|route| {
                match_template(&route.binding.path, request.path()).map(|c| (route, c))
            })
            .ok_or(TranscodeError::NoRoute)?;

        let input = request_message(route, request, body, captures)?;
        let mut grpc_request = Request::new(input);
        forward_metadata(request, &mut grpc_request);
        let path: PathAndQuery = format!(
            "/{}/{}",
            route.method.parent_service().full_name(),
            route.method.name()
        )
        .parse()
        .expect("protobuf method names form a valid gRPC path");
        let codec = DynamicCodec::new(route.method.input(), route.method.output());
        let mut grpc = Grpc::new(self.channel.clone());
        grpc.ready().await.map_err(|error| {
            TranscodeError::Status(Box::new(Status::unavailable(format!(
                "gRPC transport unavailable: {error}"
            ))))
        })?;

        if route.method.is_server_streaming() {
            let response = grpc
                .server_streaming(grpc_request, path, codec)
                .await
                .map_err(|status| TranscodeError::Status(Box::new(status)))?;
            let metadata = response.metadata().clone();
            let stream =
                response
                    .into_inner()
                    .map_err(status_to_actix)
                    .and_then(|message| async move {
                        let mut json =
                            serde_json::to_vec(&message).map_err(ErrorInternalServerError)?;
                        json.push(b'\n');
                        Ok::<_, actix_web::Error>(web::Bytes::from(json))
                    });
            let mut response = HttpResponse::Ok();
            response.insert_header((header::CONTENT_TYPE, "application/json"));
            copy_response_metadata(&metadata, &mut response);
            return Ok(response.streaming(stream));
        }

        let response = grpc
            .unary(grpc_request, path, codec)
            .await
            .map_err(|status| TranscodeError::Status(Box::new(status)))?;
        let metadata = response.metadata().clone();
        let json = serde_json::to_vec(response.get_ref()).map_err(TranscodeError::InvalidJson)?;
        let mut downstream = HttpResponse::Ok();
        downstream.insert_header((header::CONTENT_TYPE, "application/json"));
        copy_response_metadata(&metadata, &mut downstream);
        Ok(downstream.body(json))
    }
}

/// Actix handler for a [`Transcoder`] stored in `web::Data`.
pub async fn transcode(
    transcoder: web::Data<Transcoder>,
    request: HttpRequest,
    body: web::Bytes,
) -> HttpResponse {
    transcoder.handle(request, body).await
}

fn request_message(
    route: &Route,
    request: &HttpRequest,
    body: &[u8],
    captures: BTreeMap<String, String>,
) -> Result<DynamicMessage, TranscodeError> {
    let mut value = if body.is_empty() {
        serde_json::Value::Object(Default::default())
    } else {
        serde_json::from_slice(body).map_err(TranscodeError::InvalidJson)?
    };
    if let Some(field) = route.binding.body.as_deref() {
        if field != "*" {
            let mut root = serde_json::Map::new();
            insert_json_path(&mut root, field, value);
            value = serde_json::Value::Object(root);
        }
    } else if !body.is_empty() {
        return Err(TranscodeError::UnexpectedBody);
    }
    let object = value
        .as_object_mut()
        .ok_or_else(|| TranscodeError::InvalidRequest("request JSON must be an object".into()))?;
    for (name, raw) in captures {
        insert_typed_value(object, &route.method.input(), &name, raw)?;
    }
    for pair in request
        .query_string()
        .split('&')
        .filter(|pair| !pair.is_empty())
    {
        let (name, raw) = pair.split_once('=').unwrap_or((pair, ""));
        let name = percent_decode(name)?;
        let raw = percent_decode(raw)?;
        insert_typed_value(object, &route.method.input(), &name, raw)?;
    }
    let serialized = serde_json::to_vec(&value).map_err(TranscodeError::InvalidJson)?;
    let mut deserializer = serde_json::Deserializer::from_slice(&serialized);
    DynamicMessage::deserialize(route.method.input(), &mut deserializer)
        .map_err(TranscodeError::InvalidJson)
}

fn insert_typed_value(
    object: &mut serde_json::Map<String, serde_json::Value>,
    descriptor: &MessageDescriptor,
    path: &str,
    raw: String,
) -> Result<(), TranscodeError> {
    let top = path.split('.').next().unwrap_or(path);
    let field = descriptor
        .get_field_by_name(top)
        .or_else(|| descriptor.fields().find(|field| field.json_name() == top))
        .ok_or_else(|| TranscodeError::InvalidRequest(format!("unknown request field {path}")))?;
    let value = match field.kind() {
        Kind::Bool => serde_json::Value::Bool(raw.parse().map_err(|_| {
            TranscodeError::InvalidRequest(format!("field {path} must be a boolean"))
        })?),
        Kind::Int32 | Kind::Sint32 | Kind::Sfixed32 => serde_json::Value::Number(
            raw.parse::<i32>()
                .map_err(|_| {
                    TranscodeError::InvalidRequest(format!("field {path} must be an integer"))
                })?
                .into(),
        ),
        Kind::Uint32 | Kind::Fixed32 => serde_json::Value::Number(
            raw.parse::<u32>()
                .map_err(|_| {
                    TranscodeError::InvalidRequest(format!(
                        "field {path} must be an unsigned integer"
                    ))
                })?
                .into(),
        ),
        _ => serde_json::Value::String(raw),
    };
    insert_json_path(object, path, value);
    Ok(())
}

fn insert_json_path(
    object: &mut serde_json::Map<String, serde_json::Value>,
    path: &str,
    value: serde_json::Value,
) {
    let mut parts = path.split('.').peekable();
    let mut current = object;
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            current.insert(part.to_owned(), value);
            return;
        }
        current = current
            .entry(part)
            .or_insert_with(|| serde_json::Value::Object(Default::default()))
            .as_object_mut()
            .expect("path collisions are rejected by protobuf JSON decoding");
    }
}

fn forward_metadata(request: &HttpRequest, grpc: &mut Request<DynamicMessage>) {
    for (name, value) in request.headers() {
        let name = name.as_str();
        if matches!(
            name,
            "host" | "content-type" | "content-length" | "connection" | "transfer-encoding"
        ) || name.ends_with("-bin")
        {
            continue;
        }
        if let (Ok(key), Ok(value)) = (
            name.parse::<AsciiMetadataKey>(),
            AsciiMetadataValue::try_from(value.as_bytes()),
        ) {
            grpc.metadata_mut().append(key, value);
        }
    }
}

fn copy_response_metadata(
    metadata: &tonic::metadata::MetadataMap,
    response: &mut actix_web::HttpResponseBuilder,
) {
    for entry in metadata.iter() {
        if let tonic::metadata::KeyAndValueRef::Ascii(key, value) = entry {
            if let (Ok(name), Ok(value)) = (
                header::HeaderName::try_from(key.as_str()),
                header::HeaderValue::from_bytes(value.as_encoded_bytes()),
            ) {
                response.append_header((name, value));
            }
        }
    }
}

fn collect_http_rules(rule: &DynamicMessage, rpc: &str, bindings: &mut Vec<HttpBinding>) {
    let verb_and_path = [
        ("get", HttpVerb::Get),
        ("put", HttpVerb::Put),
        ("post", HttpVerb::Post),
        ("delete", HttpVerb::Delete),
        ("patch", HttpVerb::Patch),
    ]
    .into_iter()
    .find_map(|(field, verb)| {
        rule.get_field_by_name(field).and_then(|value| {
            value
                .as_str()
                .filter(|value| !value.is_empty())
                .map(|path| (verb, path.to_owned()))
        })
    });
    if let Some((verb, path)) = verb_and_path {
        let body = rule.get_field_by_name("body").and_then(|value| {
            value
                .as_str()
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        });
        bindings.push(HttpBinding {
            verb,
            path,
            rpc: rpc.to_owned(),
            body,
        });
    }
    let additional_bindings = rule.get_field_by_name("additional_bindings");
    if let Some(rules) = additional_bindings
        .as_deref()
        .and_then(|value| value.as_list())
    {
        for rule in rules.iter().filter_map(|value| value.as_message()) {
            collect_http_rules(rule, rpc, bindings);
        }
    }
}

fn find_method(pool: &DescriptorPool, name: &str) -> Option<MethodDescriptor> {
    let normalized = name.trim_start_matches('/').replace('/', ".");
    let (service, method) = normalized.rsplit_once('.')?;
    pool.get_service_by_name(service)?
        .methods()
        .find(|candidate| candidate.name() == method)
}

fn validate_template(template: &str) -> Result<(), TranscodeError> {
    if !template.starts_with('/')
        || template
            .split('/')
            .any(|part| part.contains('{') != part.contains('}'))
    {
        return Err(TranscodeError::InvalidTemplate(template.to_owned()));
    }
    Ok(())
}

fn literal_weight(template: &str) -> usize {
    template
        .split('/')
        .filter(|part| !part.starts_with('{'))
        .map(str::len)
        .sum()
}

fn match_template(template: &str, path: &str) -> Option<BTreeMap<String, String>> {
    let template: Vec<_> = template.trim_matches('/').split('/').collect();
    let path: Vec<_> = path.trim_matches('/').split('/').collect();
    if template.len() != path.len() {
        return None;
    }
    let mut captures = BTreeMap::new();
    for (expected, actual) in template.into_iter().zip(path) {
        if let Some(name) = expected
            .strip_prefix('{')
            .and_then(|part| part.strip_suffix('}'))
        {
            let name = name.split('=').next().unwrap_or(name);
            captures.insert(name.to_owned(), percent_decode(actual).ok()?);
        } else if expected != actual {
            return None;
        }
    }
    Some(captures)
}

fn percent_decode(value: &str) -> Result<String, TranscodeError> {
    let mut bytes = Vec::with_capacity(value.len());
    let input = value.as_bytes();
    let mut index = 0;
    while index < input.len() {
        match input[index] {
            b'%' if index + 2 < input.len() => {
                let hex = std::str::from_utf8(&input[index + 1..index + 3]).map_err(|_| {
                    TranscodeError::InvalidRequest("invalid percent encoding".into())
                })?;
                bytes.push(u8::from_str_radix(hex, 16).map_err(|_| {
                    TranscodeError::InvalidRequest("invalid percent encoding".into())
                })?);
                index += 3;
            }
            b'+' => {
                bytes.push(b' ');
                index += 1;
            }
            byte => {
                bytes.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(bytes)
        .map_err(|_| TranscodeError::InvalidRequest("path/query value is not UTF-8".into()))
}

/// Maps canonical gRPC status codes to their HTTP equivalents.
pub fn grpc_status_to_http(code: Code) -> StatusCode {
    match code {
        Code::Ok => StatusCode::OK,
        Code::Cancelled => StatusCode::from_u16(499).expect("499 is a valid extension status"),
        Code::Unknown | Code::Internal | Code::DataLoss => StatusCode::INTERNAL_SERVER_ERROR,
        Code::InvalidArgument | Code::FailedPrecondition | Code::OutOfRange => {
            StatusCode::BAD_REQUEST
        }
        Code::DeadlineExceeded => StatusCode::GATEWAY_TIMEOUT,
        Code::NotFound => StatusCode::NOT_FOUND,
        Code::AlreadyExists | Code::Aborted => StatusCode::CONFLICT,
        Code::PermissionDenied => StatusCode::FORBIDDEN,
        Code::Unauthenticated => StatusCode::UNAUTHORIZED,
        Code::ResourceExhausted => StatusCode::TOO_MANY_REQUESTS,
        Code::Unimplemented => StatusCode::NOT_IMPLEMENTED,
        Code::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
    }
}

fn status_response(status: Status) -> HttpResponse {
    HttpResponse::build(grpc_status_to_http(status.code()))
        .json(error_json(status.code(), status.message()))
}

fn error_json(code: Code, message: &str) -> serde_json::Value {
    serde_json::json!({
        "code": code as i32,
        "status": format!("{code:?}").to_ascii_uppercase(),
        "message": message,
    })
}

fn status_to_actix(status: Status) -> actix_web::Error {
    ErrorInternalServerError(status.to_string())
}

#[derive(Clone)]
struct DynamicCodec {
    output: MessageDescriptor,
}

impl DynamicCodec {
    fn new(_input: MessageDescriptor, output: MessageDescriptor) -> Self {
        Self { output }
    }
}

impl Codec for DynamicCodec {
    type Encode = DynamicMessage;
    type Decode = DynamicMessage;
    type Encoder = DynamicEncoder;
    type Decoder = DynamicDecoder;

    fn encoder(&mut self) -> Self::Encoder {
        DynamicEncoder
    }

    fn decoder(&mut self) -> Self::Decoder {
        DynamicDecoder(self.output.clone())
    }
}

struct DynamicEncoder;

impl Encoder for DynamicEncoder {
    type Item = DynamicMessage;
    type Error = Status;

    fn encode(&mut self, item: Self::Item, dst: &mut EncodeBuf<'_>) -> Result<(), Self::Error> {
        item.encode(dst).map_err(|error| {
            Status::internal(format!("failed to encode protobuf request: {error}"))
        })
    }
}

struct DynamicDecoder(MessageDescriptor);

impl Decoder for DynamicDecoder {
    type Item = DynamicMessage;
    type Error = Status;

    fn decode(&mut self, src: &mut DecodeBuf<'_>) -> Result<Option<Self::Item>, Self::Error> {
        DynamicMessage::decode(self.0.clone(), src)
            .map(Some)
            .map_err(|error| {
                Status::internal(format!("failed to decode protobuf response: {error}"))
            })
    }
}

/// Configuration, JSON conversion, route, or upstream failures from the transcoder.
#[derive(Debug)]
pub enum TranscodeError {
    Descriptor(prost_reflect::DescriptorError),
    ReflectionDescriptor(prost::DecodeError),
    Reflection(String),
    InvalidReflectionResponse,
    UnknownMethod(String),
    UnsupportedClientStreaming(String),
    InvalidTemplate(String),
    NoRoute,
    UnexpectedBody,
    InvalidRequest(String),
    InvalidJson(serde_json::Error),
    Status(Box<Status>),
}

impl fmt::Display for TranscodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Descriptor(error) => {
                write!(formatter, "invalid protobuf descriptor set: {error}")
            }
            Self::ReflectionDescriptor(error) => {
                write!(formatter, "invalid reflected protobuf descriptor: {error}")
            }
            Self::Reflection(message) => write!(formatter, "gRPC reflection failed: {message}"),
            Self::InvalidReflectionResponse => {
                formatter.write_str("gRPC reflection returned an unexpected response")
            }
            Self::UnknownMethod(method) => write!(formatter, "unknown protobuf method: {method}"),
            Self::UnsupportedClientStreaming(method) => write!(
                formatter,
                "HTTP transcoding does not support client-streaming method {method}"
            ),
            Self::InvalidTemplate(path) => write!(formatter, "invalid HTTP path template: {path}"),
            Self::NoRoute => formatter.write_str("no HTTP binding matched"),
            Self::UnexpectedBody => {
                formatter.write_str("this HTTP binding does not accept a request body")
            }
            Self::InvalidRequest(message) => formatter.write_str(message),
            Self::InvalidJson(error) => write!(formatter, "invalid protobuf JSON: {error}"),
            Self::Status(status) => write!(formatter, "gRPC request failed: {status}"),
        }
    }
}

impl std::error::Error for TranscodeError {}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{test, App};
    use std::time::Duration;
    use tokio::net::TcpListener;
    use tokio_stream::wrappers::TcpListenerStream;
    use tonic::{transport::Server, Response};

    mod fixture {
        tonic::include_proto!("rust_zero.gateway_test");
    }

    use fixture::{
        greeter_server::{Greeter, GreeterServer},
        GetRequest, GetResponse,
    };

    #[derive(Default)]
    struct GreeterService;

    #[tonic::async_trait]
    impl Greeter for GreeterService {
        type WatchStream = tokio_stream::wrappers::ReceiverStream<Result<GetResponse, Status>>;

        async fn get(&self, request: Request<GetRequest>) -> Result<Response<GetResponse>, Status> {
            let request = request.into_inner();
            let mut response = Response::new(GetResponse {
                id: request.id,
                message: request.view,
            });
            response
                .metadata_mut()
                .insert("x-backend", "grpc".parse().unwrap());
            Ok(response)
        }

        async fn watch(
            &self,
            request: Request<GetRequest>,
        ) -> Result<Response<Self::WatchStream>, Status> {
            let request = request.into_inner();
            let (sender, receiver) = tokio::sync::mpsc::channel(2);
            sender
                .send(Ok(GetResponse {
                    id: request.id,
                    message: "one".into(),
                }))
                .await
                .unwrap();
            sender
                .send(Ok(GetResponse {
                    id: request.id,
                    message: "two".into(),
                }))
                .await
                .unwrap();
            Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
                receiver,
            )))
        }

        async fn fail(&self, _: Request<GetRequest>) -> Result<Response<GetResponse>, Status> {
            Err(Status::not_found("missing greeter"))
        }
    }

    async fn fixture() -> (Transcoder, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let reflection = tonic_reflection::server::Builder::configure()
            .register_encoded_file_descriptor_set(include_bytes!(concat!(
                env!("OUT_DIR"),
                "/gateway.bin"
            )))
            .build_v1()
            .unwrap();
        let server = tokio::spawn(async move {
            Server::builder()
                .add_service(reflection)
                .add_service(GreeterServer::new(GreeterService))
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .unwrap();
        });
        let channel = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
            .unwrap()
            .connect_timeout(Duration::from_secs(1))
            .connect()
            .await
            .unwrap();
        let transcoder = TranscoderBuilder::from_descriptor_set(
            include_bytes!(concat!(env!("OUT_DIR"), "/gateway.bin")),
            channel,
        )
        .unwrap()
        .add_binding(HttpBinding::new(
            HttpVerb::Get,
            "/v1/greeters/{id}",
            "rust_zero.gateway_test.Greeter.Get",
        ))
        .add_binding(HttpBinding::new(
            HttpVerb::Get,
            "/v1/greeters/{id}/watch",
            "rust_zero.gateway_test.Greeter.Watch",
        ))
        .add_binding(HttpBinding::new(
            HttpVerb::Get,
            "/v1/missing/{id}",
            "rust_zero.gateway_test.Greeter.Fail",
        ))
        .build()
        .unwrap();
        (transcoder, server)
    }

    #[actix_web::test]
    async fn loads_descriptors_from_live_grpc_reflection() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let reflection = tonic_reflection::server::Builder::configure()
            .register_encoded_file_descriptor_set(include_bytes!(concat!(
                env!("OUT_DIR"),
                "/gateway.bin"
            )))
            .build_v1()
            .unwrap();
        let server = tokio::spawn(async move {
            Server::builder()
                .add_service(reflection)
                .add_service(GreeterServer::new(GreeterService))
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .unwrap();
        });
        let channel = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
            .unwrap()
            .connect()
            .await
            .unwrap();
        let transcoder = TranscoderBuilder::from_reflection(channel)
            .await
            .unwrap()
            .add_binding(HttpBinding::new(
                HttpVerb::Get,
                "/v1/reflected/{id}",
                "rust_zero.gateway_test.Greeter.Get",
            ))
            .build()
            .unwrap();
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(transcoder))
                .default_service(web::to(transcode)),
        )
        .await;
        let response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/v1/reflected/11?view=reflection")
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        server.abort();
    }

    #[actix_web::test]
    async fn transcodes_path_query_metadata_and_protobuf_json() {
        let (transcoder, server) = fixture().await;
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(transcoder))
                .default_service(web::to(transcode)),
        )
        .await;
        let response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/v1/greeters/7?view=full")
                .insert_header(("x-request-id", "request-1"))
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers().get("x-backend").unwrap(), "grpc");
        let body: serde_json::Value = test::read_body_json(response).await;
        assert_eq!(body, serde_json::json!({"id": 7, "message": "full"}));
        server.abort();
    }

    #[actix_web::test]
    async fn streams_newline_delimited_json_and_maps_grpc_statuses() {
        let (transcoder, server) = fixture().await;
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(transcoder))
                .default_service(web::to(transcode)),
        )
        .await;
        let response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/v1/greeters/9/watch")
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            test::read_body(response).await,
            "{\"id\":9,\"message\":\"one\"}\n{\"id\":9,\"message\":\"two\"}\n"
        );

        let response = test::call_service(
            &app,
            test::TestRequest::get().uri("/v1/missing/9").to_request(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body: serde_json::Value = test::read_body_json(response).await;
        assert_eq!(body["message"], "missing greeter");
        server.abort();
    }
}
