use crate::util::pretty;
use quote::quote;

pub(crate) fn render() -> String {
    pretty(
        "// Code scaffolded by rust-zero. Safe to edit.\n\n",
        quote! {
            use rest::{RestServer, RestServerConfig};

            mod handlers;
            mod routes;
            mod types;

            #[actix_web::main]
            async fn main() -> std::io::Result<()> {
                RestServer::new(RestServerConfig {
                    route_groups: routes::route_groups(),
                    ..RestServerConfig::default()
                })?
                .run(routes::configure)?
                .await
            }
        },
    )
}
