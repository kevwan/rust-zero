use rust_zero_core::ServiceGroup;
use rust_zero_mcp::{McpServer, McpServerConfig, Prompt, PromptArgument, Resource, Tool};
use serde_json::json;
use std::{error::Error, time::Duration};

#[actix_web::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let server = McpServer::new(McpServerConfig {
        stateful: true,
        ..McpServerConfig::default()
    })?;

    server.add_tool(
        Tool::new(
            "echo",
            json!({
                "type": "object",
                "properties": {"text": {"type": "string"}},
                "required": ["text"]
            }),
        )
        .with_description("Echo text supplied by the client"),
        |metadata, arguments| async move {
            Ok(json!({
                "content": [{"type": "text", "text": arguments["text"]}],
                "_meta": {"requestId": metadata.header("x-request-id")}
            }))
        },
    );
    server.add_resource(
        Resource {
            uri: "config://service".into(),
            name: "service-config".into(),
            description: Some("Public service configuration".into()),
            mime_type: Some("application/json".into()),
        },
        |_, params| async move {
            Ok(json!({"contents": [{
                "uri": params["uri"],
                "mimeType": "application/json",
                "text": "{\"environment\":\"development\"}"
            }]}))
        },
    );
    server.add_prompt(
        Prompt {
            name: "summarize".into(),
            description: Some("Summarize supplied text".into()),
            arguments: vec![PromptArgument {
                name: "text".into(),
                description: Some("Text to summarize".into()),
                required: true,
            }],
        },
        |_, params| async move {
            Ok(json!({"messages": [{
                "role": "user",
                "content": {
                    "type": "text",
                    "text": format!("Summarize: {}", params["arguments"]["text"])
                }
            }]}))
        },
    );

    let mut services = ServiceGroup::new().with_shutdown_timeout(Duration::from_secs(35));
    services.add("mcp", move |mut shutdown| async move {
        server
            .serve_until(async move { shutdown.requested().await })
            .await
    });
    services.start()?.wait_for_signal().await?;
    Ok(())
}
