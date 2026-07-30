use actix_web::{get, App, HttpRequest, HttpServer};
use rest::{ConcurrencyLimit, LoggingMiddleware, RateLimit, Timeout};
use std::time::Duration;

#[get("/")]
async fn index(_: HttpRequest) -> &'static str {
    "Hello world!\r\n"
}

#[actix_rt::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt::init();

    let concurrency_limit = ConcurrencyLimit::new(1_024);
    let rate_limit = RateLimit::new(1_000, 2_000);

    HttpServer::new(move || {
        App::new()
            .wrap(LoggingMiddleware)
            .wrap(Timeout::new(Duration::from_secs(10)))
            .wrap(concurrency_limit.clone())
            .wrap(rate_limit.clone())
            .service(index)
    })
    .bind(("127.0.0.1", 8080))?
    .run()
    .await
}
