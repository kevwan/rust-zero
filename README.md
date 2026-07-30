# rust-zero

A Rust web and RPC framework inspired by [go-zero](https://github.com/zeromicro/go-zero).

## Available features

- Actix Web middleware for structured request logging, timeout control, overload shedding, and
  token-bucket rate limiting.
- Tonic-based gRPC client and server builders with deadline, connection concurrency, and stream
  limits plus gRPC health reporting.
- A Tokio-based MapReduce primitive with bounded parallelism.

The REST middleware is composable and returns standard HTTP responses when protection activates:

| Middleware | Response |
| --- | --- |
| `Timeout` | `504 Gateway Timeout` |
| `ConcurrencyLimit` | `503 Service Unavailable` |
| `RateLimit` | `429 Too Many Requests` with `Retry-After` |

```rust
use actix_web::{App, HttpServer};
use rest::{ConcurrencyLimit, LoggingMiddleware, RateLimit, Timeout};
use std::time::Duration;

let concurrency_limit = ConcurrencyLimit::new(1_024);
let rate_limit = RateLimit::new(1_000, 2_000);

HttpServer::new(move || {
    App::new()
        .wrap(LoggingMiddleware)
        .wrap(Timeout::new(Duration::from_secs(10)))
        .wrap(concurrency_limit.clone())
        .wrap(rate_limit.clone())
})
```

## RPC

`rpc` provides common transport controls while leaving protobuf service implementations as normal
Tonic services. The crate includes a generated `rust_zero.echo` service and runnable server:

```bash
cargo run -p rpc --example echo_server
```

```rust
use rpc::{RpcClient, RpcClientConfig, RpcServer, RpcServerConfig};
use std::time::Duration;

let server = RpcServer::new(
    RpcServerConfig::new("127.0.0.1:50051".parse()?)
        .with_request_timeout(Duration::from_secs(10))
        .with_concurrency_limit(1_024),
);

let channel = RpcClient::new(
    RpcClientConfig::new("http://127.0.0.1:50051")
        .with_connect_timeout(Duration::from_secs(3))
        .with_request_timeout(Duration::from_secs(10)),
)
.connect()
.await?;
```
