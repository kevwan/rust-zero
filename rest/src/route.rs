use crate::auth::{bearer_token, decode_hs256, JwtClaims};
use actix_web::{
    body::{EitherBody, MessageBody},
    dev::{Service, ServiceRequest, ServiceResponse, Transform},
    http::{header, Method, StatusCode},
    Error, HttpMessage, HttpResponse,
};
use futures::future::{ok, LocalBoxFuture, Ready};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fmt,
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
            .finish()
    }
}

/// Policy overrides for one method and route pattern.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutePolicyConfig {
    pub method: String,
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
    #[serde(default)]
    pub routes: Vec<RoutePolicyConfig>,
}

#[derive(Debug, Clone)]
struct CompiledPolicy {
    method: Method,
    pattern: String,
    jwt_secrets: Option<Vec<Arc<[u8]>>>,
    jwt_leeway_seconds: u64,
    timeout: Option<Duration>,
    max_body_bytes: Option<usize>,
    priority: bool,
    sse: bool,
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
#[derive(Debug, Clone, Default)]
pub(crate) struct RoutePolicies {
    routes: Arc<[CompiledPolicy]>,
}

impl RoutePolicies {
    pub fn compile(groups: &[RouteGroupConfig]) -> Result<Self, String> {
        let mut routes = Vec::new();
        let mut unique = HashSet::new();

        for group in groups {
            validate_prefix(&group.prefix)?;
            validate_optional_limits(group.timeout_ms, group.max_body_bytes)?;
            if let Some(jwt) = &group.jwt {
                validate_jwt(jwt)?;
            }

            for route in &group.routes {
                if !route.path.starts_with('/') {
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
                let (jwt_secrets, jwt_leeway_seconds) = match jwt {
                    Some(jwt) => (Some(jwt_secrets(jwt)), jwt.leeway_seconds),
                    None => (None, 0),
                };
                let sse = route.sse.unwrap_or(group.sse);
                routes.push(CompiledPolicy {
                    method,
                    pattern,
                    jwt_secrets,
                    jwt_leeway_seconds,
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
                });
            }
        }

        Ok(Self {
            routes: routes.into(),
        })
    }

    fn find(&self, method: &Method, pattern: &str) -> Option<&CompiledPolicy> {
        self.routes
            .iter()
            .find(|route| route.method == *method && route.pattern == pattern)
    }
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
    type Response = ServiceResponse<EitherBody<B>>;
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
    type Response = ServiceResponse<EitherBody<B>>;
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
                let claims = request
                    .headers()
                    .get(header::AUTHORIZATION)
                    .and_then(|value| value.to_str().ok())
                    .and_then(bearer_token)
                    .and_then(|token| {
                        decode_hs256::<serde_json::Value>(token, secrets, policy.jwt_leeway_seconds)
                            .ok()
                    });
                let Some(claims) = claims else {
                    return Box::pin(async move {
                        Ok(request.into_response(
                            HttpResponse::build(StatusCode::UNAUTHORIZED)
                                .insert_header((header::WWW_AUTHENTICATE, "Bearer"))
                                .body("authentication required")
                                .map_into_right_body(),
                        ))
                    });
                };
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
        Box::pin(async move {
            let mut response = service.call(request).await?.map_into_left_body();
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
    use actix_web::{http::StatusCode, test, web, App, HttpRequest, HttpResponse};
    use std::sync::Arc;
    use tokio::sync::Notify;

    fn policy_set() -> RoutePolicies {
        RoutePolicies::compile(&[RouteGroupConfig {
            prefix: "/api".to_owned(),
            jwt: Some(RouteJwtConfig {
                secret: "secret".to_owned(),
                previous_secret: None,
                leeway_seconds: 0,
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
                        HttpResponse::Ok().json(JwtAuth::<serde_json::Value>::claims(&request))
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
        assert_eq!(claims["sub"], "42");

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
    async fn rejects_ambiguous_or_invalid_route_policies() {
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
    }
}
