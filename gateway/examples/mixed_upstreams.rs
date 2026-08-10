//! Self-contained mixed-protocol gateway deployment.
//!
//! The public listener exposes an HTTP reverse-proxy route under `/http` and a
//! JSON-to-gRPC transcoding route under `/grpc`. The example starts both sample
//! upstreams so it can be run without any external services.

use actix_web::{web, App, HttpRequest, HttpResponse, HttpServer};
use futures::Stream;
use gateway::{
    proxy, transcode, GatewayProxy, GatewayRoute, GatewayRouter, HttpBinding, HttpVerb,
    TranscoderBuilder,
};
use std::{net::SocketAddr, pin::Pin, time::Duration};
use tokio::sync::watch;
use tonic::{transport::Server, Request, Response, Status};

pub mod proto {
    tonic::include_proto!("rust_zero.gateway_test");
}

use proto::{
    greeter_server::{Greeter, GreeterServer},
    GetRequest, GetResponse,
};

#[derive(Default)]
struct GreeterService;

#[tonic::async_trait]
impl Greeter for GreeterService {
    type WatchStream = Pin<Box<dyn Stream<Item = Result<GetResponse, Status>> + Send>>;

    async fn get(&self, request: Request<GetRequest>) -> Result<Response<GetResponse>, Status> {
        let request = request.into_inner();
        Ok(Response::new(GetResponse {
            id: request.id,
            message: format!("hello from gRPC ({})", request.view),
        }))
    }

    #[allow(clippy::result_large_err)] // The generated Tonic stream item uses `tonic::Status`.
    async fn watch(
        &self,
        request: Request<GetRequest>,
    ) -> Result<Response<Self::WatchStream>, Status> {
        let id = request.into_inner().id;
        let responses = futures::stream::iter(["first", "second"].map(move |message| {
            Ok(GetResponse {
                id,
                message: message.to_owned(),
            })
        }));
        Ok(Response::new(Box::pin(responses)))
    }

    async fn fail(&self, _: Request<GetRequest>) -> Result<Response<GetResponse>, Status> {
        Err(Status::not_found("sample gRPC resource was not found"))
    }
}

async fn http_upstream(request: HttpRequest) -> HttpResponse {
    HttpResponse::Ok().json(serde_json::json!({
        "protocol": "http",
        "path": request.path(),
        "query": request.query_string(),
    }))
}

fn address(variable: &str, fallback: &str) -> Result<SocketAddr, Box<dyn std::error::Error>> {
    Ok(std::env::var(variable)
        .unwrap_or_else(|_| fallback.to_owned())
        .parse()?)
}

#[actix_web::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let gateway_address = address("GATEWAY_ADDR", "127.0.0.1:8080")?;
    let http_address = address("HTTP_UPSTREAM_ADDR", "127.0.0.1:18080")?;
    let grpc_address = address("GRPC_UPSTREAM_ADDR", "127.0.0.1:50051")?;

    let http_server = HttpServer::new(|| App::new().default_service(web::to(http_upstream)))
        .bind(http_address)?
        .run();
    let http_handle = http_server.handle();
    let http_task = actix_web::rt::spawn(http_server);

    let (grpc_shutdown, mut grpc_shutdown_receiver) = watch::channel(false);
    let grpc_task = actix_web::rt::spawn(async move {
        Server::builder()
            .add_service(GreeterServer::new(GreeterService))
            .serve_with_shutdown(grpc_address, async move {
                let _ = grpc_shutdown_receiver.changed().await;
            })
            .await
    });

    let channel = tonic::transport::Endpoint::from_shared(format!("http://{grpc_address}"))?
        .connect_timeout(Duration::from_secs(2))
        .connect_lazy();
    let transcoder = TranscoderBuilder::from_descriptor_set(
        include_bytes!(concat!(env!("OUT_DIR"), "/gateway.bin")),
        channel,
    )?
    .add_binding(HttpBinding::new(
        HttpVerb::Get,
        "/grpc/greeters/{id}",
        "rust_zero.gateway_test.Greeter.Get",
    ))
    .add_binding(HttpBinding::new(
        HttpVerb::Get,
        "/grpc/greeters/{id}/watch",
        "rust_zero.gateway_test.Greeter.Watch",
    ))
    .build()?;

    let proxy_state = GatewayProxy::new(GatewayRouter::new([GatewayRoute::new(
        "/http",
        vec![format!("http://{http_address}")],
    )?])?);
    let gateway_server = HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(proxy_state.clone()))
            .app_data(web::Data::new(transcoder.clone()))
            .service(web::scope("/grpc").default_service(web::to(transcode)))
            .default_service(web::to(proxy))
    })
    .shutdown_timeout(30)
    .bind(gateway_address)?
    .run();
    let gateway_handle = gateway_server.handle();
    let gateway_task = actix_web::rt::spawn(gateway_server);

    println!("mixed-protocol gateway listening on http://{gateway_address}");
    println!("HTTP: curl 'http://{gateway_address}/http/orders?limit=2'");
    println!("gRPC: curl 'http://{gateway_address}/grpc/greeters/7?view=full'");

    rust_zero_core::wait_for_shutdown_signal().await?;
    gateway_handle.stop(true).await;
    http_handle.stop(true).await;
    let _ = grpc_shutdown.send(true);

    gateway_task.await??;
    http_task.await??;
    grpc_task.await??;
    Ok(())
}
