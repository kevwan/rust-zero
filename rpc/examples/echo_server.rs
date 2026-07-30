use std::{net::SocketAddr, time::Duration};

use rpc::{
    echo::{
        echo_server::{Echo, EchoServer},
        EchoRequest, EchoResponse,
    },
    health_reporter, RpcServer, RpcServerConfig,
};
use tonic::{Request, Response, Status};

#[derive(Default)]
struct EchoService;

#[tonic::async_trait]
impl Echo for EchoService {
    async fn echo(&self, request: Request<EchoRequest>) -> Result<Response<EchoResponse>, Status> {
        Ok(Response::new(EchoResponse {
            message: request.into_inner().message,
        }))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let address: SocketAddr = "[::1]:50051".parse()?;
    let server = RpcServer::new(
        RpcServerConfig::new(address)
            .with_request_timeout(Duration::from_secs(10))
            .with_concurrency_limit(1_024),
    );
    let (mut reporter, health_service) = health_reporter();
    reporter.set_serving::<EchoServer<EchoService>>().await;

    server
        .router()
        .add_service(health_service)
        .add_service(EchoServer::new(EchoService))
        .serve(server.config().address())
        .await?;
    Ok(())
}
