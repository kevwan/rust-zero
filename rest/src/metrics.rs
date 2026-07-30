use actix_web::{
    dev::{Service, ServiceRequest, ServiceResponse, Transform},
    Error,
};
use futures::future::{ok, LocalBoxFuture, Ready};
use rust_zero_core::{
    CounterVec, HistogramOptions, HistogramVec, Metrics, MetricsError, VectorOptions,
};
use std::{
    task::{Context, Poll},
    time::Instant,
};

/// HTTP metrics registered in a shared [`Metrics`] registry.
#[derive(Clone)]
pub struct HttpMetrics {
    requests: CounterVec,
    duration: HistogramVec,
}

impl HttpMetrics {
    pub fn new(metrics: &Metrics, namespace: impl Into<String>) -> Result<Self, MetricsError> {
        let namespace = namespace.into();
        let request_options = VectorOptions::new("http_requests_total", "Completed HTTP requests")
            .with_namespace(namespace.clone())
            .with_labels(["method", "path", "status"]);
        let duration_options =
            VectorOptions::new("http_request_duration_seconds", "HTTP request duration")
                .with_namespace(namespace)
                .with_labels(["method", "path", "status"]);

        Ok(Self {
            requests: metrics.counter_vec(request_options)?,
            duration: metrics.histogram_vec(
                HistogramOptions::new("", "").with_vector_options(duration_options),
            )?,
        })
    }

    fn record(&self, method: &str, path: &str, status: u16, elapsed_seconds: f64) {
        let status = status.to_string();
        let labels = [method, path, status.as_str()];

        self.requests
            .inc(&labels)
            .expect("HTTP metric labels must match the registered metric");
        self.duration
            .observe(elapsed_seconds, &labels)
            .expect("HTTP metric labels and duration must be valid");
    }
}

/// Records request counts and execution durations in [`HttpMetrics`].
#[derive(Clone)]
pub struct MetricsMiddleware {
    metrics: HttpMetrics,
}

impl MetricsMiddleware {
    pub fn new(metrics: HttpMetrics) -> Self {
        Self { metrics }
    }
}

impl<S, B> Transform<S, ServiceRequest> for MetricsMiddleware
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    B: 'static,
    S::Future: 'static,
{
    type Response = ServiceResponse<B>;
    type Error = Error;
    type Transform = MetricsMiddlewareService<S>;
    type InitError = ();
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ok(MetricsMiddlewareService {
            service,
            metrics: self.metrics.clone(),
        })
    }
}

pub struct MetricsMiddlewareService<S> {
    service: S,
    metrics: HttpMetrics,
}

impl<S, B> Service<ServiceRequest> for MetricsMiddlewareService<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    B: 'static,
{
    type Response = ServiceResponse<B>;
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&self, context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(context)
    }

    fn call(&self, request: ServiceRequest) -> Self::Future {
        let method = request.method().to_string();
        let path = request
            .match_pattern()
            .unwrap_or_else(|| request.path().to_owned());
        let started_at = Instant::now();
        let metrics = self.metrics.clone();
        let future = self.service.call(request);

        Box::pin(async move {
            match future.await {
                Ok(response) => {
                    metrics.record(
                        &method,
                        &path,
                        response.status().as_u16(),
                        started_at.elapsed().as_secs_f64(),
                    );
                    Ok(response)
                }
                Err(error) => {
                    metrics.record(&method, &path, 500, started_at.elapsed().as_secs_f64());
                    Err(error)
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{HttpMetrics, MetricsMiddleware};
    use actix_web::{http::StatusCode, test, web, App, HttpResponse};
    use rust_zero_core::Metrics;

    #[actix_rt::test]
    async fn records_request_count_and_duration() {
        let metrics = Metrics::new();
        let http_metrics = HttpMetrics::new(&metrics, "users").unwrap();
        let app = test::init_service(App::new().wrap(MetricsMiddleware::new(http_metrics)).route(
            "/users/{id}",
            web::get().to(|| async { HttpResponse::Ok().finish() }),
        ))
        .await;

        let response =
            test::call_service(&app, test::TestRequest::get().uri("/users/42").to_request()).await;

        assert_eq!(response.status(), StatusCode::OK);
        let rendered = metrics.render();
        assert!(rendered.contains(
            "users_http_requests_total{method=\"GET\",path=\"/users/{id}\",status=\"200\"} 1"
        ));
        assert!(rendered.contains(
            "users_http_request_duration_seconds_count{method=\"GET\",path=\"/users/{id}\",status=\"200\"} 1"
        ));
    }
}
