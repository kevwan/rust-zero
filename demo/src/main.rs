use actix_web::{get, App, HttpRequest, HttpServer};
use rest::log::LoggingMiddleware;

#[get("/")]
async fn index(_: HttpRequest) -> &'static str {
    "Hello world!\r\n"
}

#[actix_rt::main]
async fn main() -> std::io::Result<()> {
    HttpServer::new(|| App::new().wrap(LoggingMiddleware).service(index))
        .bind(("127.0.0.1", 8080))?
        .run()
        .await
}
