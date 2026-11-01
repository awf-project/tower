//! Shared in-process RMCP transport harness.

use core_engine::adapters::mcp::rmcp_server::TowerMcpHandler;
use rmcp::{RoleClient, ServiceExt};

pub async fn start_server(
    handler: TowerMcpHandler,
) -> rmcp::service::RunningService<RoleClient, ()> {
    let (server_transport, client_transport) = tokio::io::duplex(65_536);
    tokio::spawn(async move {
        if let Ok(server) = handler.serve(server_transport).await {
            let _ = server.waiting().await;
        }
    });
    ().serve(client_transport)
        .await
        .expect("client connect failed")
}
