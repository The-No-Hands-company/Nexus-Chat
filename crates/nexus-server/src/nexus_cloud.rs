//! Nexus Cloud registration and heartbeat.
//!
//! Registers this Nexus server as a tool with the Nexus Cloud gateway
//! (POST /api/v1/tools) so it is visible in the routing table and can receive
//! subdomain-routed traffic.  All calls are non-blocking best-effort — startup
//! and normal operation continue even if Cloud is unreachable.

/// Register this server with Nexus Cloud.
///
/// # Environment variables consumed
/// - `PUBLIC_URL` — publicly reachable URL of this server (upstreamUrl in Cloud)
/// - `NEXUS_CLOUD_URL` — base URL of the Nexus Cloud instance
/// - `NEXUS_CLOUD_API_KEY` — API key for X-Api-Key header
pub async fn register_with_cloud(public_url: &str, cloud_url: &str, api_key: &str) {
    let client = reqwest::Client::new();
    let payload = serde_json::json!({
        "id": "nexus",
        "name": "Nexus",
        "description": "Privacy-first, community-owned chat and collaboration server",
        "upstreamUrl": public_url,
        "mode": "standalone",
        "exposed": true,
        "health": "healthy",
        "capabilities": ["chat", "voice", "federation", "e2ee", "websocket"]
    });

    let url = format!("{}/api/v1/tools", cloud_url.trim_end_matches('/'));
    match client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("X-Api-Key", api_key)
        .json(&payload)
        .send()
        .await
    {
        Ok(res) if res.status().is_success() => {
            tracing::info!(cloud_url, "[nexus-cloud] Registered with Nexus Cloud");
        }
        Ok(res) => {
            tracing::warn!(
                status = %res.status(),
                cloud_url,
                "[nexus-cloud] Registration rejected by Nexus Cloud"
            );
        }
        Err(e) => {
            tracing::warn!(
                error_kind = %nexus_common::logsafe::err_kind(&e),
                cloud_url,
                "[nexus-cloud] Could not reach Nexus Cloud \u{2014} continuing without registration"
            );
        }
    }
}

/// Send a liveness heartbeat so the Cloud routing table knows this node is up.
pub async fn send_heartbeat(cloud_url: &str, api_key: &str, public_url: &str) {
    let client = reqwest::Client::new();
    let url = format!(
        "{}/api/v1/tools/nexus/heartbeat",
        cloud_url.trim_end_matches('/')
    );
    if let Err(e) = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("X-Api-Key", api_key)
        .json(&serde_json::json!({ "health": "healthy", "upstreamUrl": public_url }))
        .send()
        .await
    {
        tracing::debug!(error_kind = %nexus_common::logsafe::err_kind(&e), "[nexus-cloud] Heartbeat failed (will retry)");
    }
}
