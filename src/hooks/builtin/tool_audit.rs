use async_trait::async_trait;
use serde_json::Value;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use crate::hooks::traits::{HookHandler, HookResult};
use crate::tools::traits::ToolResult;

const MAX_LOG_LEN: usize = 2000;

/// Logs full tool call arguments (before) and results (after) for audit purposes.
///
/// Always emits `tracing::info!` lines. When `log_file` is set, also appends
/// each entry as a JSON line to the specified file.
pub struct ToolAuditHook {
    log_file: Option<Mutex<PathBuf>>,
}

impl ToolAuditHook {
    pub fn new(log_file: Option<PathBuf>) -> Self {
        Self {
            log_file: log_file.map(Mutex::new),
        }
    }

    fn append_to_file(&self, line: &str) {
        let Some(path_lock) = &self.log_file else {
            return;
        };
        let path = path_lock.lock().unwrap();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let res = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&*path)
            .and_then(|mut f| writeln!(f, "{}", line));
        if let Err(e) = res {
            tracing::warn!(hook = "tool-audit", error = %e, "failed to write audit log file");
        }
    }
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    // Walk back from `max` to find a valid UTF-8 char boundary.
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[async_trait]
impl HookHandler for ToolAuditHook {
    fn name(&self) -> &str {
        "tool-audit"
    }

    fn priority(&self) -> i32 {
        -50
    }

    async fn before_tool_call(&self, name: String, args: Value) -> HookResult<(String, Value)> {
        let args_str = args.to_string();
        let args_truncated = truncate(&args_str, MAX_LOG_LEN);
        tracing::info!(
            hook = "tool-audit",
            tool = %name,
            args = %args_truncated,
            "tool call started"
        );
        self.append_to_file(&format!(
            "{{\"ts\":\"{}\",\"event\":\"start\",\"tool\":\"{}\",\"args\":{}}}",
            chrono::Utc::now().to_rfc3339(),
            name,
            truncate(&args_str, MAX_LOG_LEN),
        ));
        HookResult::Continue((name, args))
    }

    async fn on_after_tool_call(&self, tool: &str, result: &ToolResult, duration: Duration) {
        let output = truncate(&result.output, MAX_LOG_LEN);
        let duration_ms = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
        tracing::info!(
            hook = "tool-audit",
            tool = %tool,
            duration_ms,
            success = result.success,
            output = %output,
            error = ?result.error,
            "tool call finished"
        );
        self.append_to_file(&format!(
            "{{\"ts\":\"{}\",\"event\":\"finish\",\"tool\":\"{}\",\"duration_ms\":{},\"success\":{},\"output\":{},\"error\":{}}}",
            chrono::Utc::now().to_rfc3339(),
            tool,
            duration_ms,
            result.success,
            serde_json::to_string(output).unwrap_or_default(),
            serde_json::to_string(&result.error).unwrap_or_default(),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_short_string() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_long_string() {
        let long = "a".repeat(3000);
        assert_eq!(truncate(&long, MAX_LOG_LEN).len(), MAX_LOG_LEN);
    }

    #[test]
    fn truncate_multibyte_boundary() {
        // 3-byte UTF-8 chars; truncation must not split a char.
        let s = "\u{2603}".repeat(1000); // snowman, 3 bytes each
        let truncated = truncate(&s, 10);
        assert!(truncated.len() <= 10);
        assert!(truncated.is_char_boundary(truncated.len()));
    }

    #[tokio::test]
    async fn before_tool_call_passes_through() {
        let hook = ToolAuditHook::new(None);
        let args = serde_json::json!({"cmd": "ls"});
        match hook.before_tool_call("shell".into(), args.clone()).await {
            HookResult::Continue((name, returned_args)) => {
                assert_eq!(name, "shell");
                assert_eq!(returned_args, args);
            }
            HookResult::Cancel(_) => panic!("should not cancel"),
        }
    }

    #[tokio::test]
    async fn on_after_tool_call_does_not_panic() {
        let hook = ToolAuditHook::new(None);
        let result = ToolResult {
            success: true,
            output: "ok".into(),
            error: None,
        };
        hook.on_after_tool_call("shell", &result, Duration::from_millis(10))
            .await;
    }

    #[tokio::test]
    async fn on_after_tool_call_handles_error() {
        let hook = ToolAuditHook::new(None);
        let result = ToolResult {
            success: false,
            output: String::new(),
            error: Some("command not found".into()),
        };
        hook.on_after_tool_call("shell", &result, Duration::from_millis(5))
            .await;
    }

    #[tokio::test]
    async fn writes_to_log_file() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("audit.jsonl");
        let hook = ToolAuditHook::new(Some(log_path.clone()));

        let args = serde_json::json!({"cmd": "echo hi"});
        hook.before_tool_call("shell".into(), args).await;

        let result = ToolResult {
            success: true,
            output: "hi".into(),
            error: None,
        };
        hook.on_after_tool_call("shell", &result, Duration::from_millis(7))
            .await;

        let content = std::fs::read_to_string(&log_path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"event\":\"start\""));
        assert!(lines[1].contains("\"event\":\"finish\""));
        assert!(lines[1].contains("\"duration_ms\":7"));
    }
}
