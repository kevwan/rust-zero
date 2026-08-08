use std::{net::SocketAddr, pin::Pin, time::Duration};

use futures::Stream;
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
    type ServerStreamStream =
        Pin<Box<dyn Stream<Item = Result<EchoResponse, Status>> + Send + 'static>>;
    type BidirectionalStreamStream = Self::ServerStreamStream;

    async fn echo(&self, request: Request<EchoRequest>) -> Result<Response<EchoResponse>, Status> {
        Ok(Response::new(EchoResponse {
            message: request.into_inner().message,
        }))
    }

    async fn server_stream(
        &self,
        request: Request<EchoRequest>,
    ) -> Result<Response<Self::ServerStreamStream>, Status> {
        Ok(Response::new(Box::pin(tokio_stream::once(Ok(
            EchoResponse {
                message: request.into_inner().message,
            },
        )))))
    }

    async fn client_stream(
        &self,
        request: Request<tonic::Streaming<EchoRequest>>,
    ) -> Result<Response<EchoResponse>, Status> {
        let mut input = request.into_inner();
        let mut messages = Vec::new();
        while let Some(request) = input.message().await? {
            messages.push(request.message);
        }
        Ok(Response::new(EchoResponse {
            message: messages.join(","),
        }))
    }

    async fn bidirectional_stream(
        &self,
        request: Request<tonic::Streaming<EchoRequest>>,
    ) -> Result<Response<Self::BidirectionalStreamStream>, Status> {
        let replies = futures::stream::unfold(request.into_inner(), |mut input| async move {
            match input.message().await {
                Ok(Some(request)) => Some((
                    Ok(EchoResponse {
                        message: request.message,
                    }),
                    input,
                )),
                Err(status) => Some((Err(status), input)),
                Ok(None) => None,
            }
        });
        Ok(Response::new(Box::pin(replies)))
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
