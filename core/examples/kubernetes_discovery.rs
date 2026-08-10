use rust_zero_core::{KubernetesDiscovery, KubernetesDiscoveryConfig};
use std::env;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let namespace = env::var("KUBERNETES_NAMESPACE").unwrap_or_else(|_| "default".to_owned());
    let service = env::var("KUBERNETES_SERVICE").unwrap_or_else(|_| "users".to_owned());
    let port = env::var("KUBERNETES_PORT_NAME").unwrap_or_else(|_| "grpc".to_owned());
    let discovery =
        KubernetesDiscovery::infer(KubernetesDiscoveryConfig::new(namespace).with_port_name(port))
            .await?;
    let mut subscription = discovery.subscribe(service).await?;

    println!("ready endpoints: {:?}", subscription.endpoints());
    loop {
        println!("updated endpoints: {:?}", subscription.changed().await?);
    }
}
