//! MCP (Model Context Protocol) client — connects to external tool servers.
//!
//! Supports multiple transports: stdio (spawn local process), HTTP, and SSE.
//! Servers are lazy-loaded: connected at startup for tool discovery, then
//! disconnected. Reconnected on-demand when a tool is called, and automatically
//! disconnected after an idle timeout.

use std::collections::HashMap;
use std::sync::Arc;
#[cfg(not(target_has_atomic = "64"))]
use std::sync::atomic::AtomicU32;
#[cfg(target_has_atomic = "64")]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::json;
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant, timeout};

use crate::config::schema::McpServerConfig;
use crate::tools::mcp_protocol::{
    JsonRpcRequest, MCP_PROTOCOL_VERSION, McpToolDef, McpToolsListResult,
};
use crate::tools::mcp_transport::{McpTransportConn, create_transport};

/// Timeout for receiving a response from an MCP server during init/list.
/// Prevents a hung server from blocking the daemon indefinitely.
const RECV_TIMEOUT_SECS: u64 = 30;

/// Default timeout for tool calls (seconds) when not configured per-server.
const DEFAULT_TOOL_TIMEOUT_SECS: u64 = 180;

/// Maximum allowed tool call timeout (seconds) — hard safety ceiling.
const MAX_TOOL_TIMEOUT_SECS: u64 = 600;

/// Default idle timeout (seconds) before disconnecting an MCP server.
const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 90;

/// How often the idle reaper checks for stale connections (seconds).
const IDLE_CHECK_INTERVAL_SECS: u64 = 15;

// ── Internal server state ──────────────────────────────────────────────────

struct McpServerInner {
    config: McpServerConfig,
    /// None when disconnected (lazy/idle), Some when connected.
    transport: Option<Box<dyn McpTransportConn>>,
    #[cfg(target_has_atomic = "64")]
    next_id: AtomicU64,
    #[cfg(not(target_has_atomic = "64"))]
    next_id: AtomicU32,
    /// Tool definitions — persisted across disconnect/reconnect cycles.
    tools: Vec<McpToolDef>,
    /// Last time a tool call completed. Used for idle detection.
    last_activity: Option<Instant>,
}

// ── McpServer ──────────────────────────────────────────────────────────────

/// A connection to one MCP server (any transport).
/// Supports lazy connect/disconnect lifecycle.
#[derive(Clone)]
pub struct McpServer {
    inner: Arc<Mutex<McpServerInner>>,
}

impl McpServer {
    /// Connect to the server, perform the initialize handshake, and fetch the tool list.
    pub async fn connect(config: McpServerConfig) -> Result<Self> {
        let (transport, tools) = Self::do_connect(&config).await?;
        let tool_count = tools.len();

        let inner = McpServerInner {
            config,
            transport: Some(transport),
            #[cfg(target_has_atomic = "64")]
            next_id: AtomicU64::new(3),
            #[cfg(not(target_has_atomic = "64"))]
            next_id: AtomicU32::new(3),
            tools,
            last_activity: Some(Instant::now()),
        };

        tracing::info!(
            "MCP server `{}` connected — {} tool(s) available",
            inner.config.name,
            tool_count
        );

        Ok(Self {
            inner: Arc::new(Mutex::new(inner)),
        })
    }

    /// Perform the full connect + init + tools/list handshake.
    async fn do_connect(
        config: &McpServerConfig,
    ) -> Result<(Box<dyn McpTransportConn>, Vec<McpToolDef>)> {
        let mut transport = create_transport(config).with_context(|| {
            format!(
                "failed to create transport for MCP server `{}`",
                config.name
            )
        })?;

        let init_req = JsonRpcRequest::new(
            1u64,
            "initialize",
            json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {
                    "name": "zeroclaw",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
        );

        let init_resp = timeout(
            Duration::from_secs(RECV_TIMEOUT_SECS),
            transport.send_and_recv(&init_req),
        )
        .await
        .with_context(|| {
            format!(
                "MCP server `{}` timed out after {}s waiting for initialize response",
                config.name, RECV_TIMEOUT_SECS
            )
        })??;

        if init_resp.error.is_some() {
            bail!(
                "MCP server `{}` rejected initialize: {:?}",
                config.name,
                init_resp.error
            );
        }

        let notif = JsonRpcRequest::notification("notifications/initialized", json!({}));
        let _ = transport.send_and_recv(&notif).await;

        let list_req = JsonRpcRequest::new(2u64, "tools/list", json!({}));

        let list_resp = timeout(
            Duration::from_secs(RECV_TIMEOUT_SECS),
            transport.send_and_recv(&list_req),
        )
        .await
        .with_context(|| {
            format!(
                "MCP server `{}` timed out after {}s waiting for tools/list response",
                config.name, RECV_TIMEOUT_SECS
            )
        })??;

        let result = list_resp
            .result
            .ok_or_else(|| anyhow!("tools/list returned no result from `{}`", config.name))?;
        let tool_list: McpToolsListResult = serde_json::from_value(result)
            .with_context(|| format!("failed to parse tools/list from `{}`", config.name))?;

        Ok((transport, tool_list.tools))
    }

    /// Reconnect a disconnected server. Performs init handshake but reuses
    /// cached tool definitions.
    async fn ensure_connected(inner: &mut McpServerInner) -> Result<()> {
        if inner.transport.is_some() {
            return Ok(());
        }

        tracing::info!(
            "MCP server `{}` reconnecting on demand...",
            inner.config.name
        );

        let mut transport = create_transport(&inner.config).with_context(|| {
            format!(
                "failed to create transport for MCP server `{}`",
                inner.config.name
            )
        })?;

        let init_req = JsonRpcRequest::new(
            1u64,
            "initialize",
            json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {
                    "name": "zeroclaw",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
        );

        let init_resp = timeout(
            Duration::from_secs(RECV_TIMEOUT_SECS),
            transport.send_and_recv(&init_req),
        )
        .await
        .with_context(|| {
            format!(
                "MCP server `{}` timed out during reconnect init",
                inner.config.name
            )
        })??;

        if init_resp.error.is_some() {
            bail!(
                "MCP server `{}` rejected initialize on reconnect: {:?}",
                inner.config.name,
                init_resp.error
            );
        }

        let notif = JsonRpcRequest::notification("notifications/initialized", json!({}));
        let _ = transport.send_and_recv(&notif).await;

        inner.transport = Some(transport);
        #[cfg(target_has_atomic = "64")]
        inner.next_id.store(3, Ordering::Relaxed);
        #[cfg(not(target_has_atomic = "64"))]
        inner.next_id.store(3, Ordering::Relaxed);

        tracing::info!("MCP server `{}` reconnected", inner.config.name);
        Ok(())
    }

    /// Disconnect the transport (drop the process / close the connection).
    pub async fn disconnect(&self) {
        let mut inner = self.inner.lock().await;
        if let Some(mut transport) = inner.transport.take() {
            let name = inner.config.name.clone();
            let _ = transport.close().await;
            inner.last_activity = None;
            tracing::info!("MCP server `{name}` disconnected (idle)");
        }
    }

    /// Check whether this server has been idle longer than the given duration.
    pub async fn is_idle(&self, idle_timeout: Duration) -> bool {
        let inner = self.inner.lock().await;
        if inner.transport.is_none() {
            return false; // already disconnected
        }
        match inner.last_activity {
            Some(last) => last.elapsed() > idle_timeout,
            None => true, // connected but never used — disconnect
        }
    }

    /// Whether the server currently has a live transport.
    pub async fn is_connected(&self) -> bool {
        self.inner.lock().await.transport.is_some()
    }

    /// Tools advertised by this server.
    pub async fn tools(&self) -> Vec<McpToolDef> {
        self.inner.lock().await.tools.clone()
    }

    /// Server display name.
    pub async fn name(&self) -> String {
        self.inner.lock().await.config.name.clone()
    }

    /// Call a tool on this server. Returns the raw JSON result.
    /// Reconnects automatically if the server is disconnected.
    pub async fn call_tool(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let mut inner = self.inner.lock().await;

        // Lazy reconnect if disconnected
        Self::ensure_connected(&mut inner).await?;

        let id = inner.next_id.fetch_add(1, Ordering::Relaxed) as u64;
        let req = JsonRpcRequest::new(
            id,
            "tools/call",
            json!({ "name": tool_name, "arguments": arguments }),
        );

        let tool_timeout = inner
            .config
            .tool_timeout_secs
            .unwrap_or(DEFAULT_TOOL_TIMEOUT_SECS)
            .min(MAX_TOOL_TIMEOUT_SECS);

        let transport = inner
            .transport
            .as_mut()
            .expect("transport must be Some after ensure_connected");

        let resp = timeout(
            Duration::from_secs(tool_timeout),
            transport.send_and_recv(&req),
        )
        .await
        .map_err(|_| {
            anyhow!(
                "MCP server `{}` timed out after {}s during tool call `{tool_name}`",
                inner.config.name,
                tool_timeout
            )
        })?
        .with_context(|| {
            format!(
                "MCP server `{}` error during tool call `{tool_name}`",
                inner.config.name
            )
        })?;

        // Update activity timestamp after successful call
        inner.last_activity = Some(Instant::now());

        if let Some(err) = resp.error {
            bail!("MCP tool `{tool_name}` error {}: {}", err.code, err.message);
        }
        Ok(resp.result.unwrap_or(serde_json::Value::Null))
    }
}

// ── McpRegistry ───────────────────────────────────────────────────────────

/// Registry of all connected MCP servers, with a flat tool index.
/// Supports lazy lifecycle: servers are disconnected after idle timeout
/// and reconnected on-demand when a tool is called.
pub struct McpRegistry {
    servers: Vec<McpServer>,
    /// prefixed_name → (server_index, original_tool_name)
    tool_index: HashMap<String, (usize, String)>,
    /// Idle timeout duration. Zero means never disconnect.
    idle_timeout: Duration,
}

impl McpRegistry {
    /// Connect to all configured servers. Non-fatal: failures are logged and skipped.
    pub async fn connect_all(configs: &[McpServerConfig], idle_timeout_secs: u64) -> Result<Self> {
        let mut servers = Vec::new();
        let mut tool_index = HashMap::new();

        for config in configs {
            match McpServer::connect(config.clone()).await {
                Ok(server) => {
                    let server_idx = servers.len();
                    let tools = server.tools().await;
                    for tool in &tools {
                        let prefixed = format!("{}__{}", config.name, tool.name);
                        tool_index.insert(prefixed, (server_idx, tool.name.clone()));
                    }
                    servers.push(server);
                }
                Err(e) => {
                    tracing::error!(
                        "Failed to connect to MCP server `{}`: {:#}",
                        config.name,
                        e
                    );
                }
            }
        }

        let idle_timeout = Duration::from_secs(idle_timeout_secs);

        Ok(Self {
            servers,
            tool_index,
            idle_timeout,
        })
    }

    /// Disconnect all servers immediately. Used after initial tool discovery
    /// when lazy mode is active.
    pub async fn disconnect_all(&self) {
        for server in &self.servers {
            server.disconnect().await;
        }
        tracing::info!("MCP: all servers disconnected (lazy mode — will reconnect on demand)");
    }

    /// Disconnect servers that have been idle longer than the configured timeout.
    pub async fn disconnect_idle(&self) {
        if self.idle_timeout.is_zero() {
            return;
        }
        for server in &self.servers {
            if server.is_idle(self.idle_timeout).await {
                server.disconnect().await;
            }
        }
    }

    /// Spawn a background task that periodically checks for idle servers
    /// and disconnects them.
    pub fn spawn_idle_reaper(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let registry = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(Duration::from_secs(IDLE_CHECK_INTERVAL_SECS));
            loop {
                interval.tick().await;
                registry.disconnect_idle().await;
            }
        })
    }

    /// Whether lazy lifecycle is enabled (idle_timeout > 0).
    pub fn is_lazy(&self) -> bool {
        !self.idle_timeout.is_zero()
    }

    /// All prefixed tool names across all connected servers.
    pub fn tool_names(&self) -> Vec<String> {
        self.tool_index.keys().cloned().collect()
    }

    /// Tool definition for a given prefixed name (cloned).
    pub async fn get_tool_def(&self, prefixed_name: &str) -> Option<McpToolDef> {
        let (server_idx, original_name) = self.tool_index.get(prefixed_name)?;
        let inner = self.servers[*server_idx].inner.lock().await;
        inner
            .tools
            .iter()
            .find(|t| &t.name == original_name)
            .cloned()
    }

    /// Execute a tool by prefixed name.
    pub async fn call_tool(
        &self,
        prefixed_name: &str,
        arguments: serde_json::Value,
    ) -> Result<String> {
        let (server_idx, original_name) = self
            .tool_index
            .get(prefixed_name)
            .ok_or_else(|| anyhow!("unknown MCP tool `{prefixed_name}`"))?;
        let result = self.servers[*server_idx]
            .call_tool(original_name, arguments)
            .await?;
        serde_json::to_string_pretty(&result)
            .with_context(|| format!("failed to serialize result of MCP tool `{prefixed_name}`"))
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    pub fn server_count(&self) -> usize {
        self.servers.len()
    }

    pub fn tool_count(&self) -> usize {
        self.tool_index.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::McpTransport;

    #[test]
    fn tool_name_prefix_format() {
        let prefixed = format!("{}__{}", "filesystem", "read_file");
        assert_eq!(prefixed, "filesystem__read_file");
    }

    #[tokio::test]
    async fn connect_nonexistent_command_fails_cleanly() {
        // A command that doesn't exist should fail at spawn, not panic.
        let config = McpServerConfig {
            name: "nonexistent".to_string(),
            command: "/usr/bin/this_binary_does_not_exist_zeroclaw_test".to_string(),
            args: vec![],
            env: std::collections::HashMap::default(),
            tool_timeout_secs: None,
            transport: McpTransport::Stdio,
            url: None,
            headers: std::collections::HashMap::default(),
        };
        let result = McpServer::connect(config).await;
        assert!(result.is_err());
        let msg = result.err().unwrap().to_string();
        assert!(msg.contains("failed to create transport"), "got: {msg}");
    }

    #[tokio::test]
    async fn connect_all_nonfatal_on_single_failure() {
        // If one server config is bad, connect_all should succeed (with 0 servers).
        let configs = vec![McpServerConfig {
            name: "bad".to_string(),
            command: "/usr/bin/does_not_exist_zc_test".to_string(),
            args: vec![],
            env: std::collections::HashMap::default(),
            tool_timeout_secs: None,
            transport: McpTransport::Stdio,
            url: None,
            headers: std::collections::HashMap::default(),
        }];
        let registry = McpRegistry::connect_all(&configs, 0)
            .await
            .expect("connect_all should not fail");
        assert!(registry.is_empty());
        assert_eq!(registry.tool_count(), 0);
    }

    #[test]
    fn http_transport_requires_url() {
        let config = McpServerConfig {
            name: "test".into(),
            transport: McpTransport::Http,
            ..Default::default()
        };
        let result = create_transport(&config);
        assert!(result.is_err());
    }

    #[test]
    fn sse_transport_requires_url() {
        let config = McpServerConfig {
            name: "test".into(),
            transport: McpTransport::Sse,
            ..Default::default()
        };
        let result = create_transport(&config);
        assert!(result.is_err());
    }

    // ── Empty registry (no servers) ────────────────────────────────────────

    #[tokio::test]
    async fn empty_registry_is_empty() {
        let registry = McpRegistry::connect_all(&[], 0)
            .await
            .expect("connect_all on empty slice should succeed");
        assert!(registry.is_empty());
        assert_eq!(registry.server_count(), 0);
        assert_eq!(registry.tool_count(), 0);
    }

    #[tokio::test]
    async fn empty_registry_tool_names_is_empty() {
        let registry = McpRegistry::connect_all(&[], 0)
            .await
            .expect("connect_all should succeed");
        assert!(registry.tool_names().is_empty());
    }

    #[tokio::test]
    async fn empty_registry_get_tool_def_returns_none() {
        let registry = McpRegistry::connect_all(&[], 0)
            .await
            .expect("connect_all should succeed");
        let result = registry.get_tool_def("nonexistent__tool").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn empty_registry_call_tool_unknown_name_returns_error() {
        let registry = McpRegistry::connect_all(&[], 0)
            .await
            .expect("connect_all should succeed");
        let err = registry
            .call_tool("nonexistent__tool", serde_json::json!({}))
            .await
            .expect_err("should fail for unknown tool");
        assert!(err.to_string().contains("unknown MCP tool"), "got: {err}");
    }

    #[tokio::test]
    async fn connect_all_empty_gives_zero_servers() {
        let registry = McpRegistry::connect_all(&[], 0)
            .await
            .expect("connect_all should succeed");
        // Verify all three count methods agree on zero.
        assert_eq!(registry.server_count(), 0);
        assert_eq!(registry.tool_count(), 0);
        assert!(registry.is_empty());
    }
}
