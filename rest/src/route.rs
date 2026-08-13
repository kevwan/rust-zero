use crate::auth::{bearer_token, decode_hs256, JwtClaims, ProjectedClaims};
use actix_web::{
    body::{BoxBody, MessageBody},
    dev::{Service, ServiceRequest, ServiceResponse, Transform},
    http::{header, Method, StatusCode},
    Error, HttpMessage, HttpResponse,
};
use futures::future::{ok, LocalBoxFuture, Ready};
use rust_zero_core::{AuthFailure, JwtClaimProjection};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fmt,
    future::Future,
    rc::Rc,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

/// HS256 authentication inherited by every route in a declarative group unless a route is public.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteJwtConfig {
    pub secret: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_secret: Option<String>,
    #[serde(default)]
    pub leeway_seconds: u64,
    #[serde(default)]
    pub claim_projection: JwtClaimProjection,
}

impl fmt::Debug for RouteJwtConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RouteJwtConfig")
            .field("secret", &"[REDACTED]")
            .field(
                "previous_secret",
                &self.previous_secret.as_ref().map(|_| "[REDACTED]"),
            )
            .field("leeway_seconds", &self.leeway_seconds)
            .field("claim_projection", &self.claim_projection)
            .finish()
    }
}

/// Policy overrides for one method and route pattern.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutePolicyConfig {
    pub method: String,
    /// Path below the group prefix. An empty path targets the prefix itself.
    pub path: String,
    #[serde(default)]
    pub public: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jwt: Option<RouteJwtConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_body_bytes: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sse: Option<bool>,
}

/// A route group with a shared prefix and inheritable policy defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteGroupConfig {
    #[serde(default)]
    pub prefix: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jwt: Option<RouteJwtConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_body_bytes: Option<usize>,
    #[serde(default)]
    pub priority: bool,
    #[serde(default)]
    pub sse: bool,
    /// Named application middleware, applied in declaration order around every route in the group.
    #[serde(default)]
    pub middleware: Vec<String>,
    #[serde(default)]
    pub routes: Vec<RoutePolicyConfig>,
}

/// The boxed future returned by application-defined route middleware.
pub type RouteMiddlewareFuture = LocalBoxFuture<'static, Result<ServiceResponse<BoxBody>, Error>>;

/// The remainder of a declarative route's middleware and handler chain.
#[derive(Clone)]
pub struct RouteMiddlewareNext {
    inner: Rc<dyn Fn(ServiceRequest) -> RouteMiddlewareFuture>,
}

impl RouteMiddlewareNext {
    pub fn call(&self, request: ServiceRequest) -> RouteMiddlewareFuture {
        (self.inner)(request)
    }
}

/// Type-erased application middleware that can be registered by name on [`crate::RestServer`].
pub trait RouteMiddleware: Send + Sync + 'static {
    fn call(&self, request: ServiceRequest, next: RouteMiddlewareNext) -> RouteMiddlewareFuture;
}

impl<F, Fut> RouteMiddleware for F
where
    F: Fn(ServiceRequest, RouteMiddlewareNext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<ServiceResponse<BoxBody>, Error>> + 'static,
{
    fn call(&self, request: ServiceRequest, next: RouteMiddlewareNext) -> RouteMiddlewareFuture {
        Box::pin((self)(request, next))
    }
}

#[derive(Debug, Clone)]
struct CompiledPolicy {
    method: Method,
    pattern: String,
    jwt_secrets: Option<Vec<Arc<[u8]>>>,
    jwt_leeway_seconds: u64,
    jwt_claim_projection: JwtClaimProjection,
    timeout: Option<Duration>,
    max_body_bytes: Option<usize>,
    priority: bool,
    sse: bool,
    middleware: Arc<[String]>,
}

/// Effective route settings communicated to the standard transport middleware.
#[derive(Debug, Clone, Default)]
pub(crate) struct RequestPolicy {
    pub timeout: Option<Duration>,
    pub max_body_bytes: Option<usize>,
    pub priority: bool,
    pub sse: bool,
}

/// Applies compiled declarative route policies before the standard server middleware stack.
#[derive(Clone, Default)]
pub(crate) struct RoutePolicies {
    routes: Arc<[CompiledPolicy]>,
    middleware: Arc<HashMap<String, Arc<dyn RouteMiddleware>>>,
}

impl fmt::Debug for RoutePolicies {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RoutePolicies")
            .field("routes", &self.routes)
            .field("middleware_names", &self.middleware.keys())
            .finish()
    }
}

impl RoutePolicies {
    pub fn compile(groups: &[RouteGroupConfig]) -> Result<Self, String> {
        let mut routes = Vec::new();
        let mut unique = HashSet::new();

        for group in groups {
            validate_prefix(&group.prefix)?;
            validate_optional_limits(group.timeout_ms, group.max_body_bytes)?;
            validate_middleware_names(&group.middleware)?;
            if let Some(jwt) = &group.jwt {
                validate_jwt(jwt)?;
            }

            for route in &group.routes {
                if route.path.is_empty() && group.prefix.is_empty() {
                    return Err("an empty route path requires a non-empty group prefix".to_owned());
                }
                if !route.path.is_empty() && !route.path.starts_with('/') {
                    return Err(format!("route path must start with '/': {}", route.path));
                }
                validate_optional_limits(route.timeout_ms, route.max_body_bytes)?;
                if let Some(jwt) = &route.jwt {
                    validate_jwt(jwt)?;
                }
                if route.public && route.jwt.is_some() {
                    return Err(format!(
                        "route {} cannot be public and define JWT authentication",
                        route.path
                    ));
                }

                let method = Method::from_bytes(route.method.as_bytes())
                    .map_err(|_| format!("invalid route method: {}", route.method))?;
                let pattern = join_route(&group.prefix, &route.path);
                if !unique.insert((method.clone(), pattern.clone())) {
                    return Err(format!("duplicate route policy: {method} {pattern}"));
                }

                let jwt = if route.public {
                    None
                } else {
                    route.jwt.as_ref().or(group.jwt.as_ref())
                };
                let (jwt_secrets, jwt_leeway_seconds, jwt_claim_projection) = match jwt {
                    Some(jwt) => (
                        Some(jwt_secrets(jwt)),
                        jwt.leeway_seconds,
                        jwt.claim_projection.clone(),
                    ),
                    None => (None, 0, JwtClaimProjection::default()),
                };
                let sse = route.sse.unwrap_or(group.sse);
                routes.push(CompiledPolicy {
                    method,
                    pattern,
                    jwt_secrets,
                    jwt_leeway_seconds,
                    jwt_claim_projection,
                    timeout: if sse {
                        None
                    } else {
                        route
                            .timeout_ms
                            .or(group.timeout_ms)
                            .map(Duration::from_millis)
                    },
                    max_body_bytes: route.max_body_bytes.or(group.max_body_bytes),
                    priority: route.priority.unwrap_or(group.priority),
                    sse,
                    middleware: group.middleware.clone().into(),
                });
            }
        }

        Ok(Self {
            routes: routes.into(),
            middleware: Arc::new(HashMap::new()),
        })
    }

    pub fn with_middleware(
        mut self,
        middleware: HashMap<String, Arc<dyn RouteMiddleware>>,
    ) -> Result<Self, String> {
        for route in self.routes.iter() {
            for name in route.middleware.iter() {
                if !middleware.contains_key(name) {
                    return Err(format!("route middleware '{name}' is not registered"));
                }
            }
        }
        self.middleware = Arc::new(middleware);
        Ok(self)
    }

    fn find(&self, method: &Method, pattern: &str) -> Option<&CompiledPolicy> {
        self.routes
            .iter()
            .find(|route| route.method == *method && route.pattern == pattern)
    }
}

fn validate_middleware_names(names: &[String]) -> Result<(), String> {
    let mut unique = HashSet::new();
    for name in names {
        if name.trim().is_empty() {
            return Err("route middleware name must not be empty".to_owned());
        }
        if !unique.insert(name) {
            return Err(format!("duplicate route middleware name: {name}"));
        }
    }
    Ok(())
}

fn validate_prefix(prefix: &str) -> Result<(), String> {
    if !prefix.is_empty() && !prefix.starts_with('/') {
        return Err(format!("route group prefix must start with '/': {prefix}"));
    }
    Ok(())
}

fn validate_optional_limits(
    timeout_ms: Option<u64>,
    max_body_bytes: Option<usize>,
) -> Result<(), String> {
    if timeout_ms == Some(0) {
        return Err("route timeout_ms must be greater than zero".to_owned());
    }
    if max_body_bytes == Some(0) {
        return Err("route max_body_bytes must be greater than zero".to_owned());
    }
    Ok(())
}

fn validate_jwt(jwt: &RouteJwtConfig) -> Result<(), String> {
    if jwt.secret.is_empty() {
        return Err("route JWT secret must not be empty".to_owned());
    }
    if jwt.previous_secret.as_deref() == Some("") {
        return Err("route previous JWT secret must not be empty".to_owned());
    }
    Ok(())
}

fn jwt_secrets(jwt: &RouteJwtConfig) -> Vec<Arc<[u8]>> {
    let mut secrets = vec![Arc::from(jwt.secret.as_bytes())];
    if let Some(previous) = &jwt.previous_secret {
        secrets.push(Arc::from(previous.as_bytes()));
    }
    secrets
}

fn join_route(prefix: &str, path: &str) -> String {
    if path.is_empty() {
        return prefix.to_owned();
    }
    if prefix.is_empty() || prefix == "/" {
        return path.to_owned();
    }
    format!("{}{}", prefix.trim_end_matches('/'), path)
}

impl<S, B> Transform<S, ServiceRequest> for RoutePolicies
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<BoxBody>;
    type Error = Error;
    type Transform = RoutePoliciesMiddleware<S>;
    type InitError = ();
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ok(RoutePoliciesMiddleware {
            service: Rc::new(service),
            policies: self.clone(),
        })
    }
}

pub(crate) struct RoutePoliciesMiddleware<S> {
    service: Rc<S>,
    policies: RoutePolicies,
}

impl<S, B> Service<ServiceRequest> for RoutePoliciesMiddleware<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<BoxBody>;
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&self, context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(context)
    }

    fn call(&self, request: ServiceRequest) -> Self::Future {
        let pattern = request
            .match_pattern()
            .unwrap_or_else(|| request.path().to_owned());
        let policy = self.policies.find(request.method(), &pattern).cloned();

        if let Some(policy) = &policy {
            if let Some(secrets) = &policy.jwt_secrets {
                let result = request
                    .headers()
                    .get(header::AUTHORIZATION)
                    .and_then(|value| value.to_str().ok())
                    .and_then(bearer_token)
                    .ok_or(AuthFailure::MissingCredentials)
                    .and_then(|token| {
                        decode_hs256::<serde_json::Value>(token, secrets, policy.jwt_leeway_seconds)
                            .map_err(AuthFailure::from)
                    });
                let claims = match result {
                    Ok(claims) => claims,
                    Err(failure) => {
                        return Box::pin(async move {
                            Ok(request.into_response(
                                HttpResponse::build(StatusCode::UNAUTHORIZED)
                                    .insert_header((header::WWW_AUTHENTICATE, "Bearer"))
                                    .json(serde_json::json!({
                                        "code": failure.code(),
                                        "message": failure.message(),
                                    }))
                                    .map_into_boxed_body(),
                            ))
                        });
                    }
                };
                request.extensions_mut().insert(ProjectedClaims(
                    policy.jwt_claim_projection.project(&claims),
                ));
                request.extensions_mut().insert(JwtClaims(claims));
            }

            request.extensions_mut().insert(RequestPolicy {
                timeout: policy.timeout,
                max_body_bytes: policy.max_body_bytes,
                priority: policy.priority,
                sse: policy.sse,
            });
        }

        let service = Rc::clone(&self.service);
        let middleware = policy
            .as_ref()
            .map(|policy| policy.middleware.clone())
            .unwrap_or_default();
        let registry = Arc::clone(&self.policies.middleware);
        Box::pin(async move {
            let terminal = RouteMiddlewareNext {
                inner: Rc::new(move |request| {
                    let service = Rc::clone(&service);
                    Box::pin(async move { Ok(service.call(request).await?.map_into_boxed_body()) })
                }),
            };
            let chain = middleware.iter().rev().fold(terminal, |next, name| {
                let route_middleware = Arc::clone(
                    registry
                        .get(name)
                        .expect("route middleware registry was validated"),
                );
                RouteMiddlewareNext {
                    inner: Rc::new(move |request| route_middleware.call(request, next.clone())),
                }
            });
            let mut response = chain.call(request).await?;
            if policy.is_some_and(|policy| policy.sse) {
                response.headers_mut().insert(
                    header::CONTENT_TYPE,
                    header::HeaderValue::from_static("text/event-stream"),
                );
                response.headers_mut().insert(
                    header::CACHE_CONTROL,
                    header::HeaderValue::from_static("no-cache, no-transform"),
                );
                response.headers_mut().insert(
                    header::HeaderName::from_static("x-accel-buffering"),
                    header::HeaderValue::from_static("no"),
                );
            }
            Ok(response)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{encode_hs256, ConcurrencyLimit, JwtAuth, RequestBodyLimit, Timeout};
    use actix_web::{
        http::{header::HeaderValue, StatusCode},
        test, web, App, HttpRequest, HttpResponse,
    };
    use std::sync::Arc;
    use tokio::sync::Notify;

    fn policy_set() -> RoutePolicies {
        RoutePolicies::compile(&[RouteGroupConfig {
            prefix: "/api".to_owned(),
            jwt: Some(RouteJwtConfig {
                secret: "secret".to_owned(),
                previous_secret: None,
                leeway_seconds: 0,
                claim_projection: JwtClaimProjection::new([(
                    "caller".to_owned(),
                    "sub".to_owned(),
                )]),
            }),
            timeout_ms: Some(50),
            max_body_bytes: Some(16),
            routes: vec![
                RoutePolicyConfig {
                    method: "GET".to_owned(),
                    path: "/private/{id}".to_owned(),
                    public: false,
                    jwt: None,
                    timeout_ms: None,
                    max_body_bytes: None,
                    priority: None,
                    sse: None,
                },
                RoutePolicyConfig {
                    method: "GET".to_owned(),
                    path: "/slow".to_owned(),
                    public: true,
                    jwt: None,
                    timeout_ms: Some(1),
                    max_body_bytes: None,
                    priority: None,
                    sse: None,
                },
                RoutePolicyConfig {
                    method: "POST".to_owned(),
                    path: "/body".to_owned(),
                    public: true,
                    jwt: None,
                    timeout_ms: None,
                    max_body_bytes: Some(2),
                    priority: None,
                    sse: None,
                },
                RoutePolicyConfig {
                    method: "GET".to_owned(),
                    path: "/events".to_owned(),
                    public: true,
                    jwt: None,
                    timeout_ms: Some(1),
                    max_body_bytes: None,
                    priority: None,
                    sse: Some(true),
                },
            ],
            ..RouteGroupConfig::default()
        }])
        .unwrap()
    }

    #[actix_web::test]
    async fn enforces_inherited_and_per_route_policies() {
        let app = test::init_service(
            App::new()
                .wrap(Timeout::new(Duration::from_secs(1)))
                .wrap(RequestBodyLimit::new(1_024))
                .wrap(policy_set())
                .route(
                    "/api/private/{id}",
                    web::get().to(|request: HttpRequest| async move {
                        HttpResponse::Ok().json(serde_json::json!({
                            "claims": JwtAuth::<serde_json::Value>::claims(&request),
                            "projected": JwtAuth::<serde_json::Value>::projected_claims(&request),
                        }))
                    }),
                )
                .route(
                    "/api/slow",
                    web::get().to(|| async {
                        actix_rt::time::sleep(Duration::from_millis(20)).await;
                        HttpResponse::Ok().finish()
                    }),
                )
                .route(
                    "/api/body",
                    web::post().to(|body: web::Bytes| async move { HttpResponse::Ok().body(body) }),
                )
                .route(
                    "/api/events",
                    web::get().to(|| async {
                        actix_rt::time::sleep(Duration::from_millis(10)).await;
                        HttpResponse::Ok().body("data: ready\n\n")
                    }),
                ),
        )
        .await;

        let unauthorized = test::call_service(
            &app,
            test::TestRequest::get().uri("/api/private/7").to_request(),
        )
        .await;
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        let token = encode_hs256(&serde_json::json!({ "sub": "42" }), b"secret").unwrap();
        let authorized = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/api/private/7")
                .insert_header((header::AUTHORIZATION, format!("Bearer {token}")))
                .to_request(),
        )
        .await;
        assert_eq!(authorized.status(), StatusCode::OK);
        let claims: serde_json::Value = test::read_body_json(authorized).await;
        assert_eq!(claims["claims"]["sub"], "42");
        assert_eq!(claims["projected"]["caller"], "42");

        let slow =
            test::try_call_service(&app, test::TestRequest::get().uri("/api/slow").to_request())
                .await
                .expect_err("route timeout should reject slow work");
        assert_eq!(
            actix_web::error::ResponseError::status_code(slow.as_response_error()),
            StatusCode::GATEWAY_TIMEOUT
        );

        let oversized = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/api/body")
                .set_payload("abc")
                .to_request(),
        )
        .await;
        assert_eq!(oversized.status(), StatusCode::PAYLOAD_TOO_LARGE);

        let events = test::call_service(
            &app,
            test::TestRequest::get().uri("/api/events").to_request(),
        )
        .await;
        assert_eq!(events.status(), StatusCode::OK);
        assert_eq!(
            events.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/event-stream"
        );
        assert_eq!(
            events.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-cache, no-transform"
        );
    }

    #[actix_web::test]
    async fn priority_routes_use_reserved_capacity() {
        let policies = RoutePolicies::compile(&[RouteGroupConfig {
            routes: vec![RoutePolicyConfig {
                method: "GET".to_owned(),
                path: "/priority".to_owned(),
                public: true,
                jwt: None,
                timeout_ms: None,
                max_body_bytes: None,
                priority: Some(true),
                sse: None,
            }],
            ..RouteGroupConfig::default()
        }])
        .unwrap();
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let app = test::init_service(
            App::new()
                .wrap(ConcurrencyLimit::new(1).with_priority_reserve(1))
                .wrap(policies)
                .route(
                    "/normal",
                    web::get().to({
                        let started = Arc::clone(&started);
                        let release = Arc::clone(&release);
                        move || {
                            let started = Arc::clone(&started);
                            let release = Arc::clone(&release);
                            async move {
                                started.notify_one();
                                release.notified().await;
                                HttpResponse::Ok().finish()
                            }
                        }
                    }),
                )
                .route("/priority", web::get().to(HttpResponse::Ok)),
        )
        .await;

        let normal = test::call_service(&app, test::TestRequest::get().uri("/normal").to_request());
        let priority = async {
            started.notified().await;
            let response =
                test::call_service(&app, test::TestRequest::get().uri("/priority").to_request())
                    .await;
            release.notify_one();
            response
        };
        let (normal, priority) = futures::future::join(normal, priority).await;
        assert_eq!(normal.status(), StatusCode::OK);
        assert_eq!(priority.status(), StatusCode::OK);
    }

    #[actix_web::test]
    async fn named_group_middleware_wraps_only_declared_routes() {
        let policies = RoutePolicies::compile(&[RouteGroupConfig {
            prefix: "/api".to_owned(),
            middleware: vec!["api-key".to_owned(), "response-header".to_owned()],
            routes: vec![RoutePolicyConfig {
                method: "GET".to_owned(),
                path: "/private".to_owned(),
                public: true,
                jwt: None,
                timeout_ms: None,
                max_body_bytes: None,
                priority: None,
                sse: None,
            }],
            ..RouteGroupConfig::default()
        }])
        .unwrap();
        let mut middleware: HashMap<String, Arc<dyn RouteMiddleware>> = HashMap::new();
        middleware.insert(
            "api-key".to_owned(),
            Arc::new(
                |request: ServiceRequest, next: RouteMiddlewareNext| async move {
                    if request
                        .headers()
                        .get("x-api-key")
                        .and_then(|value| value.to_str().ok())
                        != Some("valid")
                    {
                        return Ok(request
                            .into_response(HttpResponse::Forbidden().finish())
                            .map_into_boxed_body());
                    }
                    next.call(request).await
                },
            ),
        );
        middleware.insert(
            "response-header".to_owned(),
            Arc::new(
                |request: ServiceRequest, next: RouteMiddlewareNext| async move {
                    let mut response = next.call(request).await?;
                    response.headers_mut().insert(
                        "x-application-middleware".parse().unwrap(),
                        HeaderValue::from_static("yes"),
                    );
                    Ok(response)
                },
            ),
        );
        let app = test::init_service(
            App::new()
                .wrap(policies.with_middleware(middleware).unwrap())
                .route("/api/private", web::get().to(HttpResponse::Ok))
                .route("/public", web::get().to(HttpResponse::Ok)),
        )
        .await;

        let forbidden = test::call_service(
            &app,
            test::TestRequest::get().uri("/api/private").to_request(),
        )
        .await;
        assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);
        assert!(!forbidden.headers().contains_key("x-application-middleware"));

        let accepted = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/api/private")
                .insert_header(("x-api-key", "valid"))
                .to_request(),
        )
        .await;
        assert_eq!(accepted.status(), StatusCode::OK);
        assert_eq!(
            accepted.headers().get("x-application-middleware").unwrap(),
            "yes"
        );

        let public =
            test::call_service(&app, test::TestRequest::get().uri("/public").to_request()).await;
        assert_eq!(public.status(), StatusCode::OK);
        assert!(!public.headers().contains_key("x-application-middleware"));
    }

    #[actix_web::test]
    async fn rejects_ambiguous_or_invalid_route_policies() {
        let exact_prefix = RoutePolicies::compile(&[RouteGroupConfig {
            prefix: "/api".to_owned(),
            routes: vec![RoutePolicyConfig {
                method: "GET".to_owned(),
                path: String::new(),
                public: true,
                jwt: None,
                timeout_ms: None,
                max_body_bytes: None,
                priority: None,
                sse: None,
            }],
            ..RouteGroupConfig::default()
        }])
        .unwrap();
        assert!(exact_prefix.find(&Method::GET, "/api").is_some());

        let empty_root = RoutePolicies::compile(&[RouteGroupConfig {
            routes: vec![RoutePolicyConfig {
                method: "GET".to_owned(),
                path: String::new(),
                public: true,
                jwt: None,
                timeout_ms: None,
                max_body_bytes: None,
                priority: None,
                sse: None,
            }],
            ..RouteGroupConfig::default()
        }])
        .unwrap_err();
        assert!(empty_root.contains("non-empty group prefix"));

        let duplicate = RoutePolicyConfig {
            method: "GET".to_owned(),
            path: "/users/{id}".to_owned(),
            public: true,
            jwt: None,
            timeout_ms: None,
            max_body_bytes: None,
            priority: None,
            sse: None,
        };
        let error = RoutePolicies::compile(&[RouteGroupConfig {
            routes: vec![duplicate.clone(), duplicate],
            ..RouteGroupConfig::default()
        }])
        .unwrap_err();
        assert!(error.contains("duplicate"));

        let missing = RoutePolicies::compile(&[RouteGroupConfig {
            middleware: vec!["missing".to_owned()],
            routes: vec![RoutePolicyConfig {
                method: "GET".to_owned(),
                path: "/route".to_owned(),
                public: true,
                jwt: None,
                timeout_ms: None,
                max_body_bytes: None,
                priority: None,
                sse: None,
            }],
            ..RouteGroupConfig::default()
        }])
        .unwrap()
        .with_middleware(HashMap::new())
        .unwrap_err();
        assert!(missing.contains("not registered"));
    }
}
