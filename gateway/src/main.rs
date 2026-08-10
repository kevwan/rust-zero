use gateway::{GatewayConfig, GatewayServer};

#[actix_web::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: gateway <config.{json,json5,toml,yaml,yml}>")?;
    let server = GatewayServer::new(GatewayConfig::load(path)?)?;
    server
        .serve_until(async {
            let _ = rust_zero_core::wait_for_shutdown_signal().await;
        })
        .await?;
    Ok(())
}
