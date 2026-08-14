use std::{
    alloc::{GlobalAlloc, Layout, System},
    collections::BTreeMap,
    error::Error,
    net::TcpListener,
    pin::Pin,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use actix_web::{web, HttpResponse};
use futures::{stream, Stream};
use rest::{RestServer, RestServerConfig};
use rpc::echo::{
    echo_client::EchoClient,
    echo_server::{Echo as EchoApi, EchoServer},
    EchoRequest, EchoResponse,
};
use rust_zero_core::{
    AdaptiveShedder, CircuitBreaker, CircuitBreakerConfig, LoadShedderConfig, QueueRuntime,
    QueueRuntimeConfig, RollingCircuitBreakerConfig, ServiceRegistry,
};
use serde::{Deserialize, Serialize};
use tokio::{sync::oneshot, task::JoinSet};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{transport::Server, Request, Response, Status};

struct CountingAllocator;

static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        // SAFETY: delegation preserves the caller's layout contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        // SAFETY: delegation preserves the caller's layout contract.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: delegation preserves the caller's allocation contract.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(size as u64, Ordering::Relaxed);
        // SAFETY: delegation preserves the caller's allocation contract.
        unsafe { System.realloc(ptr, layout, size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

#[derive(Clone, Deserialize, Serialize)]
struct Config {
    version: u32,
    warmup_iterations: usize,
    measured_iterations: usize,
    concurrency: usize,
    payload_bytes: usize,
    breaker_failure_percent: u32,
    discovery_endpoints: usize,
    queue_capacity: usize,
    queue_messages: usize,
    queue_consumer_delay_us: u64,
    overload_concurrency: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    soak_duration_seconds: Option<u64>,
}

#[derive(Serialize)]
struct Report {
    schema_version: u32,
    framework: String,
    framework_version: String,
    unix_timestamp: u64,
    git_revision: String,
    rustc: String,
    target: String,
    config: Config,
    workloads: Vec<Measurement>,
    #[serde(skip_serializing_if = "Option::is_none")]
    soak: Option<SoakReport>,
}

#[derive(Serialize)]
struct Measurement {
    name: String,
    operations: usize,
    elapsed_ms: f64,
    operations_per_second: f64,
    p50_us: f64,
    p95_us: f64,
    p99_us: f64,
    allocations: u64,
    allocated_bytes: u64,
    peak_rss_kib: u64,
    counters: BTreeMap<String, u64>,
}

#[derive(Serialize)]
struct SoakReport {
    requested_seconds: u64,
    elapsed_seconds: f64,
    cycles: usize,
    total_operations: usize,
    invariant_failures: usize,
    initial_peak_rss_kib: u64,
    final_peak_rss_kib: u64,
    peak_rss_growth_kib: u64,
    workloads: BTreeMap<String, SoakWorkloadSummary>,
}

#[derive(Default, Serialize)]
struct SoakWorkloadSummary {
    cycles: usize,
    operations: usize,
    min_operations_per_second: f64,
    max_p99_us: f64,
    max_allocations: u64,
    max_allocated_bytes: u64,
    max_peak_rss_kib: u64,
}

struct SoakAccumulator {
    requested_seconds: u64,
    started: Instant,
    initial_peak_rss_kib: u64,
    cycles: usize,
    total_operations: usize,
    workloads: BTreeMap<String, SoakWorkloadSummary>,
}

impl SoakAccumulator {
    fn new(requested_seconds: u64) -> Self {
        Self {
            requested_seconds,
            started: Instant::now(),
            initial_peak_rss_kib: peak_rss_kib(),
            cycles: 0,
            total_operations: 0,
            workloads: BTreeMap::new(),
        }
    }

    fn record(
        &mut self,
        config: &Config,
        measurements: &[Measurement],
    ) -> Result<(), Box<dyn Error>> {
        validate_workload_invariants(config, measurements)?;
        self.cycles += 1;
        for measurement in measurements {
            self.total_operations += measurement.operations;
            let summary = self.workloads.entry(measurement.name.clone()).or_default();
            summary.cycles += 1;
            summary.operations += measurement.operations;
            if summary.cycles == 1 {
                summary.min_operations_per_second = measurement.operations_per_second;
            } else {
                summary.min_operations_per_second = summary
                    .min_operations_per_second
                    .min(measurement.operations_per_second);
            }
            summary.max_p99_us = summary.max_p99_us.max(measurement.p99_us);
            summary.max_allocations = summary.max_allocations.max(measurement.allocations);
            summary.max_allocated_bytes =
                summary.max_allocated_bytes.max(measurement.allocated_bytes);
            summary.max_peak_rss_kib = summary.max_peak_rss_kib.max(measurement.peak_rss_kib);
        }
        Ok(())
    }

    fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    fn finish(self) -> SoakReport {
        let elapsed_seconds = self.started.elapsed().as_secs_f64();
        let final_peak_rss_kib = peak_rss_kib();
        SoakReport {
            requested_seconds: self.requested_seconds,
            elapsed_seconds,
            cycles: self.cycles,
            total_operations: self.total_operations,
            invariant_failures: 0,
            initial_peak_rss_kib: self.initial_peak_rss_kib,
            final_peak_rss_kib,
            peak_rss_growth_kib: final_peak_rss_kib.saturating_sub(self.initial_peak_rss_kib),
            workloads: self.workloads,
        }
    }
}

#[derive(Default)]
struct Counters(BTreeMap<String, u64>);

impl Counters {
    fn with(mut self, key: &str, value: u64) -> Self {
        self.0.insert(key.to_owned(), value);
        self
    }
}

fn measure<F>(name: &str, operations: usize, operation: F) -> Measurement
where
    F: FnOnce(&mut Vec<Duration>) -> Counters,
{
    let before_allocations = ALLOCATIONS.load(Ordering::Relaxed);
    let before_bytes = ALLOCATED_BYTES.load(Ordering::Relaxed);
    let started = Instant::now();
    let mut latencies = Vec::with_capacity(operations);
    let counters = operation(&mut latencies);
    let elapsed = started.elapsed();
    latencies.sort_unstable();
    Measurement {
        name: name.to_owned(),
        operations,
        elapsed_ms: elapsed.as_secs_f64() * 1_000.0,
        operations_per_second: operations as f64 / elapsed.as_secs_f64(),
        p50_us: percentile(&latencies, 50),
        p95_us: percentile(&latencies, 95),
        p99_us: percentile(&latencies, 99),
        allocations: ALLOCATIONS.load(Ordering::Relaxed) - before_allocations,
        allocated_bytes: ALLOCATED_BYTES.load(Ordering::Relaxed) - before_bytes,
        peak_rss_kib: peak_rss_kib(),
        counters: counters.0,
    }
}

async fn measure_async<F, Fut>(name: &str, operations: usize, operation: F) -> Measurement
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = (Vec<Duration>, Counters)>,
{
    let before_allocations = ALLOCATIONS.load(Ordering::Relaxed);
    let before_bytes = ALLOCATED_BYTES.load(Ordering::Relaxed);
    let started = Instant::now();
    let (mut latencies, counters) = operation().await;
    let elapsed = started.elapsed();
    latencies.sort_unstable();
    Measurement {
        name: name.to_owned(),
        operations,
        elapsed_ms: elapsed.as_secs_f64() * 1_000.0,
        operations_per_second: operations as f64 / elapsed.as_secs_f64(),
        p50_us: percentile(&latencies, 50),
        p95_us: percentile(&latencies, 95),
        p99_us: percentile(&latencies, 99),
        allocations: ALLOCATIONS.load(Ordering::Relaxed) - before_allocations,
        allocated_bytes: ALLOCATED_BYTES.load(Ordering::Relaxed) - before_bytes,
        peak_rss_kib: peak_rss_kib(),
        counters: counters.0,
    }
}

fn percentile(values: &[Duration], percentile: usize) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let index = ((values.len() - 1) * percentile) / 100;
    values[index].as_secs_f64() * 1_000_000.0
}

fn peak_rss_kib() -> u64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: getrusage initializes the supplied rusage on success.
    let result = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    if result != 0 {
        return 0;
    }
    // macOS reports bytes; Linux and other supported Unix targets report KiB.
    let raw = unsafe { usage.assume_init() }.ru_maxrss as u64;
    if cfg!(target_os = "macos") {
        raw / 1024
    } else {
        raw
    }
}

#[derive(Default)]
struct EchoService;

#[tonic::async_trait]
impl EchoApi for EchoService {
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
        Ok(Response::new(Box::pin(stream::once(async move {
            Ok(EchoResponse {
                message: request.into_inner().message,
            })
        }))))
    }

    async fn client_stream(
        &self,
        request: Request<tonic::Streaming<EchoRequest>>,
    ) -> Result<Response<EchoResponse>, Status> {
        let mut input = request.into_inner();
        let mut message = String::new();
        while let Some(item) = input.message().await? {
            message.push_str(&item.message);
        }
        Ok(Response::new(EchoResponse { message }))
    }

    async fn bidirectional_stream(
        &self,
        request: Request<tonic::Streaming<EchoRequest>>,
    ) -> Result<Response<Self::BidirectionalStreamStream>, Status> {
        let output = stream::unfold(request.into_inner(), |mut input| async move {
            match input.message().await {
                Ok(Some(request)) => Some((
                    Ok(EchoResponse {
                        message: request.message,
                    }),
                    input,
                )),
                Err(error) => Some((Err(error), input)),
                Ok(None) => None,
            }
        });
        Ok(Response::new(Box::pin(output)))
    }
}

#[actix_web::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "benchmarks/config/v1.toml".to_owned());
    let config: Config = toml::from_str(&std::fs::read_to_string(&path)?)?;
    if config.version != 1 {
        return Err(format!("unsupported benchmark config version {}", config.version).into());
    }
    validate(&config)?;

    let (workloads, soak) = if let Some(seconds) = config.soak_duration_seconds {
        let mut accumulator = SoakAccumulator::new(seconds);
        let workloads = run_workloads(&config).await?;
        accumulator.record(&config, &workloads)?;
        while accumulator.elapsed() < Duration::from_secs(seconds) {
            let cycle = run_workloads(&config).await?;
            accumulator.record(&config, &cycle)?;
        }
        (workloads, Some(accumulator.finish()))
    } else {
        let workloads = run_workloads(&config).await?;
        validate_workload_invariants(&config, &workloads)?;
        (workloads, None)
    };

    let report = Report {
        schema_version: 1,
        framework: "rust-zero".to_owned(),
        framework_version: env!("CARGO_PKG_VERSION").to_owned(),
        unix_timestamp: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
        git_revision: std::env::var("RUST_ZERO_GIT_REVISION")
            .unwrap_or_else(|_| "unknown".to_owned()),
        rustc: std::env::var("RUST_ZERO_RUSTC").unwrap_or_else(|_| "unknown".to_owned()),
        target: format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS),
        config,
        workloads,
        soak,
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

async fn run_workloads(config: &Config) -> Result<Vec<Measurement>, Box<dyn Error>> {
    Ok(vec![
        rest_transport(config).await?,
        grpc_transport(config).await?,
        breaker_failure(config),
        overload_recovery(config).await,
        discovery_snapshot(config)?,
        queue_saturation(config).await?,
    ])
}

fn validate_workload_invariants(
    config: &Config,
    measurements: &[Measurement],
) -> Result<(), Box<dyn Error>> {
    let by_name: BTreeMap<_, _> = measurements
        .iter()
        .map(|measurement| (measurement.name.as_str(), measurement))
        .collect();
    for transport in ["rest_transport", "grpc_transport"] {
        let measurement = by_name
            .get(transport)
            .ok_or_else(|| format!("missing soak workload {transport}"))?;
        if measurement.counters.get("completed") != Some(&(measurement.operations as u64)) {
            return Err(format!("{transport} did not complete every operation").into());
        }
    }
    let overload = by_name
        .get("overload_recovery")
        .ok_or("missing soak workload overload_recovery")?;
    if overload.counters.get("recovered") != Some(&1)
        || overload
            .counters
            .get("rejected")
            .copied()
            .unwrap_or_default()
            == 0
    {
        return Err("overload workload did not reject and recover".into());
    }
    let discovery = by_name
        .get("large_discovery_snapshot")
        .ok_or("missing soak workload large_discovery_snapshot")?;
    if discovery.counters.get("snapshot_endpoints") != Some(&(config.discovery_endpoints as u64)) {
        return Err("discovery soak snapshot was incomplete".into());
    }
    let queue = by_name
        .get("queue_saturation")
        .ok_or("missing soak workload queue_saturation")?;
    if queue.counters.get("processed") != Some(&(queue.operations as u64)) {
        return Err("queue soak workload lost messages".into());
    }
    Ok(())
}

fn validate(config: &Config) -> Result<(), Box<dyn Error>> {
    if config.measured_iterations == 0
        || config.concurrency == 0
        || config.discovery_endpoints == 0
        || config.queue_capacity == 0
        || config.queue_messages == 0
        || config.overload_concurrency == 0
        || config.breaker_failure_percent > 100
        || config.soak_duration_seconds == Some(0)
    {
        return Err(
            "benchmark counts and optional soak duration must be positive, and failure percentage <= 100"
                .into(),
        );
    }
    Ok(())
}

async fn rest_transport(config: &Config) -> Result<Measurement, Box<dyn Error>> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    let server_config = RestServerConfig {
        address,
        workers: 1,
        logging: false,
        tracing: false,
        metrics: false,
        ..RestServerConfig::default()
    };
    let server = RestServer::new(server_config)?.run_on(listener, |routes| {
        routes.route(
            "/echo",
            web::post().to(|body: web::Bytes| async move { HttpResponse::Ok().body(body) }),
        );
    })?;
    let handle = server.handle();
    let task = tokio::spawn(server);
    let client = reqwest::Client::new();
    let url = format!("http://{address}/echo");
    let payload = vec![b'x'; config.payload_bytes];
    for _ in 0..config.warmup_iterations {
        client
            .post(&url)
            .body(payload.clone())
            .send()
            .await?
            .bytes()
            .await?;
    }

    let operations = config.measured_iterations;
    let concurrency = config.concurrency;
    let measurement = measure_async("rest_transport", operations, || async {
        let mut latencies = Vec::with_capacity(operations);
        let mut tasks = JoinSet::new();
        for worker in 0..concurrency {
            let client = client.clone();
            let url = url.clone();
            let payload = payload.clone();
            tasks.spawn(async move {
                let mut samples = Vec::new();
                for _ in (worker..operations).step_by(concurrency) {
                    let started = Instant::now();
                    let response = client
                        .post(&url)
                        .body(payload.clone())
                        .send()
                        .await
                        .expect("REST request failed");
                    assert!(response.status().is_success());
                    let body = response.bytes().await.expect("REST body failed");
                    assert_eq!(body.len(), payload.len());
                    samples.push(started.elapsed());
                }
                samples
            });
        }
        while let Some(result) = tasks.join_next().await {
            latencies.append(&mut result.expect("REST benchmark task panicked"));
        }
        let completed = latencies.len() as u64;
        (latencies, Counters::default().with("completed", completed))
    })
    .await;
    handle.stop(true).await;
    task.await??;
    Ok(measurement)
}

async fn grpc_transport(config: &Config) -> Result<Measurement, Box<dyn Error>> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let (stop, stopped) = oneshot::channel();
    let server = tokio::spawn(async move {
        Server::builder()
            .add_service(EchoServer::new(EchoService))
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                let _ = stopped.await;
            })
            .await
    });
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{address}"))?
        .connect()
        .await?;
    let payload = "x".repeat(config.payload_bytes);
    for _ in 0..config.warmup_iterations {
        EchoClient::new(channel.clone())
            .echo(EchoRequest {
                message: payload.clone(),
            })
            .await?;
    }

    let operations = config.measured_iterations;
    let concurrency = config.concurrency;
    let measurement = measure_async("grpc_transport", operations, || async {
        let mut latencies = Vec::with_capacity(operations);
        let mut tasks = JoinSet::new();
        for worker in 0..concurrency {
            let channel = channel.clone();
            let payload = payload.clone();
            tasks.spawn(async move {
                let mut client = EchoClient::new(channel);
                let mut samples = Vec::new();
                for _ in (worker..operations).step_by(concurrency) {
                    let started = Instant::now();
                    client
                        .echo(EchoRequest {
                            message: payload.clone(),
                        })
                        .await
                        .expect("gRPC request failed");
                    samples.push(started.elapsed());
                }
                samples
            });
        }
        while let Some(result) = tasks.join_next().await {
            latencies.append(&mut result.expect("gRPC benchmark task panicked"));
        }
        let completed = latencies.len() as u64;
        (latencies, Counters::default().with("completed", completed))
    })
    .await;
    let _ = stop.send(());
    server.await??;
    Ok(measurement)
}

fn breaker_failure(config: &Config) -> Measurement {
    let breaker = CircuitBreaker::new(CircuitBreakerConfig::rolling(
        RollingCircuitBreakerConfig::new()
            .with_minimum_requests(20)
            .with_random_seed(1),
    ));
    let operations = config.measured_iterations;
    measure("circuit_breaker_partial_failure", operations, |latencies| {
        let mut upstream_failures = 0_u64;
        let mut rejected = 0_u64;
        for index in 0..operations {
            let started = Instant::now();
            let failure = index % 100 < config.breaker_failure_percent as usize;
            let result = breaker.execute(|| if failure { Err(()) } else { Ok(()) });
            latencies.push(started.elapsed());
            match result {
                Err(rust_zero_core::CircuitBreakerError::Operation(())) => upstream_failures += 1,
                Err(rust_zero_core::CircuitBreakerError::Open) => rejected += 1,
                Ok(()) => {}
            }
        }
        Counters::default()
            .with("upstream_failures", upstream_failures)
            .with("rejected", rejected)
    })
}

async fn overload_recovery(config: &Config) -> Measurement {
    let shedder = AdaptiveShedder::new(
        LoadShedderConfig::new(config.overload_concurrency, Duration::from_millis(1))
            .with_sample_window(8),
    );
    let operations = config.measured_iterations;
    measure("overload_recovery", operations, |latencies| {
        let held: Vec<_> = (0..config.overload_concurrency)
            .filter_map(|_| shedder.try_acquire())
            .collect();
        let mut rejected = 0_u64;
        for _ in 0..operations / 2 {
            let started = Instant::now();
            if shedder.try_acquire().is_none() {
                rejected += 1;
            }
            latencies.push(started.elapsed());
        }
        drop(held);
        let recovery_started = Instant::now();
        let recovered = shedder.try_acquire().is_some();
        let recovery_us = recovery_started.elapsed().as_micros() as u64;
        for _ in operations / 2..operations {
            let started = Instant::now();
            drop(shedder.try_acquire());
            latencies.push(started.elapsed());
        }
        Counters::default()
            .with("rejected", rejected)
            .with("recovered", u64::from(recovered))
            .with("recovery_us", recovery_us)
    })
}

fn discovery_snapshot(config: &Config) -> Result<Measurement, Box<dyn Error>> {
    let registry = ServiceRegistry::new();
    let mut leases = Vec::with_capacity(config.discovery_endpoints);
    for index in 0..config.discovery_endpoints {
        leases.push(registry.publish("benchmark", format!("http://127.0.0.1:{}", 10000 + index))?);
    }
    let operations = config.measured_iterations.min(100);
    let measurement = measure("large_discovery_snapshot", operations, |latencies| {
        let mut endpoints = 0_u64;
        for _ in 0..operations {
            let started = Instant::now();
            endpoints = registry
                .endpoints("benchmark")
                .expect("snapshot failed")
                .len() as u64;
            latencies.push(started.elapsed());
        }
        Counters::default().with("snapshot_endpoints", endpoints)
    });
    drop(leases);
    Ok(measurement)
}

async fn queue_saturation(config: &Config) -> Result<Measurement, Box<dyn Error>> {
    let processed = Arc::new(AtomicUsize::new(0));
    let mut queue_config = QueueRuntimeConfig::new("benchmark", config.queue_capacity, 1);
    queue_config.shutdown_timeout = Duration::from_secs(10);
    let delay = Duration::from_micros(config.queue_consumer_delay_us);
    let (producer, running) = QueueRuntime::start(queue_config, {
        let processed = Arc::clone(&processed);
        move |_: usize| {
            let processed = Arc::clone(&processed);
            async move {
                tokio::time::sleep(delay).await;
                processed.fetch_add(1, Ordering::Relaxed);
                Ok::<_, &'static str>(())
            }
        }
    })?;
    let operations = config.queue_messages;
    let measurement = measure_async("queue_saturation", operations, || async {
        let mut latencies = Vec::with_capacity(operations);
        for index in 0..operations {
            let started = Instant::now();
            producer.push(index).await.expect("queue closed");
            latencies.push(started.elapsed());
        }
        while processed.load(Ordering::Relaxed) < operations {
            tokio::task::yield_now().await;
        }
        let counters = Counters::default()
            .with("processed", processed.load(Ordering::Relaxed) as u64)
            .with("capacity", config.queue_capacity as u64);
        (latencies, counters)
    })
    .await;
    drop(producer);
    running.wait().await?;
    Ok(measurement)
}
