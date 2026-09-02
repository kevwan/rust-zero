use crate::util::pretty;
use ast::ApiFile;
use quote::quote;

pub(crate) fn render(ast: &ApiFile) -> String {
    let handlers_mod = crate::handlers::has_routes(ast).then(|| quote!(mod handlers;));
    pretty(
        "// Code scaffolded by rust-zero. Safe to edit.\n\n",
        quote! {
            #handlers_mod
            mod routes;
            mod types;

            use rest::{RestServer, RestServerConfig};

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
