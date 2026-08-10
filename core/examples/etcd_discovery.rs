use rust_zero_core::{EtcdClient, EtcdConfig};
use std::{env, time::Duration};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let endpoint = env::var("ETCD_ENDPOINT").unwrap_or_else(|_| "http://127.0.0.1:2379".to_owned());
    let advertised =
        env::var("SERVICE_ENDPOINT").unwrap_or_else(|_| "http://127.0.0.1:8080".to_owned());
    let client =
        EtcdClient::connect(EtcdConfig::new([endpoint]).with_namespace("/rust-zero/examples"))
            .await?;
    let subscription = client.subscribe("users").await?;
    let _lease = client
        .publish("users", "users-local", advertised, Duration::from_secs(10))
        .await?;

    println!(
        "published users-local; discovered {:?}",
        subscription.endpoints()
    );
    tokio::signal::ctrl_c().await?;
    Ok(())
}
