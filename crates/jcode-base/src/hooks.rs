//! User-configurable lifecycle hooks.
//!
//! Hooks are external commands that jcode runs at well-defined lifecycle
//! points so other programs can observe or gate agent behavior without
//! forking jcode. They are configured in `[hooks]` in config.toml (or
//! `JCODE_HOOK_*` env vars) and follow the same command-line conventions as
//! `[terminal] spawn_hook`: the command is parsed shell-style but executed
//! directly (no shell), with `JCODE_HOOK_*` metadata env vars describing the
//! event.
//!
//! Two dispatch styles:
//!
//! - **Observers** (`turn_start`, `turn_end`, `session_start`, `session_end`,
//!   `post_tool`): spawned detached, fire-and-forget. Failures are logged and
//!   never affect the agent.
//! - **Gate** (`pre_tool`): jcode waits (with a timeout) for the hook to
//!   exit. Exit 0 allows the tool call, exit 2 blocks it and the hook's
//!   stderr is fed back to the model as the tool error. Any other outcome
//!   (other exit codes, timeout, spawn failure) fails open with a warning.
//!
//! Hook processes get `JCODE_HOOKS_DISABLED=1` in their environment so a
//! hook that itself invokes jcode does not recursively trigger hooks.

use std::path::PathBuf;

tokio::task_local! {
    /// Terminal identity for the client whose request is currently executing.
    /// Task-local storage keeps concurrent clients isolated without mutating
    /// the daemon's process-wide environment.
    static CLIENT_TERMINAL_ENV: Vec<(String, String)>;
}

/// Maximum bytes of JSON payload exported via `JCODE_HOOK_PAYLOAD`.
const PAYLOAD_ENV_LIMIT: usize = 16 * 1024;
/// Maximum bytes of tool input JSON exported to the pre_tool gate.
const TOOL_INPUT_ENV_LIMIT: usize = 16 * 1024;
/// Maximum chars of hook stderr used as a block reason.
const BLOCK_REASON_LIMIT: usize = 2000;

/// Decision returned by the `pre_tool` gate hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateDecision {
    Allow,
    Block { reason: String },
}

/// A lifecycle event to deliver to a hook.
#[derive(Debug, Clone)]
pub struct HookEvent {
    /// Event name: "turn_start", "turn_end", "session_start", "session_end",
    /// "post_tool".
    pub event: &'static str,
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    /// Extra env fields. Keys are suffixes: ("STATUS", "ok") becomes
    /// `JCODE_HOOK_STATUS=ok` and `"status": "ok"` in the JSON payload.
    pub fields: Vec<(&'static str, String)>,
}

impl HookEvent {
    pub fn new(event: &'static str) -> Self {
        Self {
            event,
            session_id: None,
            cwd: None,
            fields: Vec::new(),
        }
    }

    pub fn session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    pub fn cwd(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn field(mut self, key: &'static str, value: impl Into<String>) -> Self {
        self.fields.push((key, value.into()));
        self
    }
}

/// Run `future` with the terminal identity of the client that initiated it.
///
/// Shared-server request handlers use this to keep lifecycle hooks scoped to
/// the requesting pane instead of the environment inherited by the server.
pub async fn with_client_terminal_env<F>(env: Vec<(String, String)>, future: F) -> F::Output
where
    F: std::future::Future,
{
    CLIENT_TERMINAL_ENV.scope(env, future).await
}

/// The configured commands for `event`, in declaration order.
pub fn hook_commands(event: &str) -> Vec<String> {
    if hooks_suppressed() {
        return Vec::new();
    }
    let hooks = &crate::config::config().hooks;
    let raw = match event {
        "turn_start" => hooks.turn_start.as_ref(),
        "turn_end" => hooks.turn_end.as_ref(),
        "session_start" => hooks.session_start.as_ref(),
        "session_context" => hooks.session_context.as_ref(),
        "session_end" => hooks.session_end.as_ref(),
        "pre_tool" => hooks.pre_tool.as_ref(),
        "post_tool" => hooks.post_tool.as_ref(),
        _ => None,
    };
    raw.into_iter()
        .flat_map(|commands| commands.iter())
        .map(str::trim)
        .filter(|command| !command.is_empty())
        .map(str::to_owned)
        .collect()
}

const SESSION_CONTEXT_LIMIT: usize = 64 * 1024;

struct BoundedHookOutput {
    bytes: Vec<u8>,
    exceeded: bool,
}

fn read_bounded_hook_output<R: std::io::Read>(
    reader: R,
    limit: usize,
    exceeded_signal: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> std::io::Result<BoundedHookOutput> {
    let mut reader = std::io::BufReader::new(reader);
    let mut bytes = Vec::with_capacity(limit.min(8192));
    let mut buffer = [0u8; 8192];
    let mut exceeded = false;
    loop {
        let read = std::io::Read::read(&mut reader, &mut buffer)?;
        if read == 0 {
            break;
        }
        let keep = limit.saturating_sub(bytes.len()).min(read);
        bytes.extend_from_slice(&buffer[..keep]);
        if keep < read {
            exceeded = true;
            if let Some(signal) = &exceeded_signal {
                signal.store(true, std::sync::atomic::Ordering::Release);
            }
            break;
        }
    }
    Ok(BoundedHookOutput { bytes, exceeded })
}

fn terminate_session_context_process_group(pid: u32) {
    #[cfg(unix)]
    let signal = libc::SIGKILL;
    #[cfg(windows)]
    let signal = 0;
    let _ = crate::platform::signal_detached_process_group(pid, signal);
}

fn terminate_session_context_child(child: &mut std::process::Child) {
    terminate_session_context_process_group(child.id());
    let _ = child.kill();
    let _ = child.wait();
}

fn parse_session_context_output(output: Vec<u8>) -> anyhow::Result<String> {
    if output.len() > SESSION_CONTEXT_LIMIT {
        return Err(anyhow::anyhow!(
            "session_context hook output exceeded {} bytes",
            SESSION_CONTEXT_LIMIT
        ));
    }
    let text = String::from_utf8(output)
        .map_err(|_| anyhow::anyhow!("session_context hook returned non-UTF-8 output"))?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(anyhow::anyhow!("session_context hook returned no context"));
    }
    let context = match serde_json::from_str::<serde_json::Value>(trimmed) {
        Ok(value) => value
            .get("hookSpecificOutput")
            .and_then(|value| value.get("additionalContext"))
            .or_else(|| value.get("additionalContext"))
            .and_then(|value| value.as_str())
            .ok_or_else(|| {
                anyhow::anyhow!("session_context JSON output requires a string additionalContext")
            })?
            .trim()
            .to_string(),
        Err(_) => trimmed.to_string(),
    };
    if context.is_empty() {
        return Err(anyhow::anyhow!("session_context hook returned no context"));
    }
    if context.len() > SESSION_CONTEXT_LIMIT {
        return Err(anyhow::anyhow!(
            "session_context hook output exceeded {} bytes",
            SESSION_CONTEXT_LIMIT
        ));
    }
    Ok(context)
}

/// Run the fail-closed session context channel synchronously.
pub fn run_session_context(event: HookEvent) -> anyhow::Result<String> {
    let commands = hook_commands("session_context");
    if commands.is_empty() {
        return Ok(String::new());
    }
    let timeout = std::time::Duration::from_millis(
        crate::config::config()
            .hooks
            .session_context_timeout_ms
            .max(1),
    );
    let mut contexts = Vec::new();
    let mut combined_len = 0usize;
    for command_line in commands {
        let mut cmd = build_hook_process(&command_line, &event)
            .map_err(|e| anyhow::anyhow!("session_context hook is invalid: {e}"))?;
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = crate::platform::spawn_detached(&mut cmd)
            .map_err(|e| anyhow::anyhow!("session_context hook failed to start: {e}"))?;
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let stdout_exceeded = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stdout_exceeded_reader = stdout_exceeded.clone();
        let out_thread = std::thread::spawn(move || {
            read_bounded_hook_output(stdout, SESSION_CONTEXT_LIMIT, Some(stdout_exceeded_reader))
        });
        let stderr_exceeded = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stderr_exceeded_reader = stderr_exceeded.clone();
        let err_thread = std::thread::spawn(move || {
            read_bounded_hook_output(stderr, BLOCK_REASON_LIMIT, Some(stderr_exceeded_reader))
        });
        let started = std::time::Instant::now();
        enum WaitOutcome {
            Exited(std::process::ExitStatus),
            StdoutExceeded,
            StderrExceeded,
            TimedOut,
            WaitFailed(std::io::Error),
        }
        let outcome = loop {
            if stdout_exceeded.load(std::sync::atomic::Ordering::Acquire) {
                break WaitOutcome::StdoutExceeded;
            }
            if stderr_exceeded.load(std::sync::atomic::Ordering::Acquire) {
                break WaitOutcome::StderrExceeded;
            }
            match child.try_wait() {
                Ok(Some(status)) => break WaitOutcome::Exited(status),
                Ok(None) => {}
                Err(error) => break WaitOutcome::WaitFailed(error),
            }
            if started.elapsed() >= timeout {
                break WaitOutcome::TimedOut;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        if !matches!(outcome, WaitOutcome::Exited(_)) {
            terminate_session_context_child(&mut child);
        } else if !out_thread.is_finished() || !err_thread.is_finished() {
            let pipe_grace = std::time::Instant::now();
            while pipe_grace.elapsed() < std::time::Duration::from_millis(50)
                && (!out_thread.is_finished() || !err_thread.is_finished())
            {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            if !out_thread.is_finished() || !err_thread.is_finished() {
                // A shell leader may exit while descendants still inherit its
                // stdout/stderr pipes. Terminate the detached group before
                // joining readers so a completed hook cannot hang startup.
                terminate_session_context_process_group(child.id());
            }
        }
        let output = out_thread.join().map_err(|_| {
            anyhow::anyhow!("session_context hook stdout reader terminated unexpectedly")
        })?;
        let stderr = err_thread.join().map_err(|_| {
            anyhow::anyhow!("session_context hook stderr reader terminated unexpectedly")
        })?;
        match outcome {
            WaitOutcome::StdoutExceeded => {
                return Err(anyhow::anyhow!(
                    "session_context hook output exceeded {} bytes",
                    SESSION_CONTEXT_LIMIT
                ));
            }
            WaitOutcome::StderrExceeded => {
                return Err(anyhow::anyhow!(
                    "session_context hook stderr exceeded {} bytes",
                    BLOCK_REASON_LIMIT
                ));
            }
            WaitOutcome::TimedOut => {
                return Err(anyhow::anyhow!(
                    "session_context hook timed out after {}ms",
                    timeout.as_millis()
                ));
            }
            WaitOutcome::WaitFailed(error) => {
                return Err(anyhow::anyhow!("session_context hook wait failed: {error}"));
            }
            WaitOutcome::Exited(status) => {
                if output.as_ref().is_ok_and(|output| output.exceeded) {
                    return Err(anyhow::anyhow!(
                        "session_context hook output exceeded {} bytes",
                        SESSION_CONTEXT_LIMIT
                    ));
                }
                if stderr.as_ref().is_ok_and(|stderr| stderr.exceeded) {
                    return Err(anyhow::anyhow!(
                        "session_context hook stderr exceeded {} bytes",
                        BLOCK_REASON_LIMIT
                    ));
                }
                let output = output.map_err(|error| {
                    anyhow::anyhow!("session_context hook stdout read failed: {error}")
                })?;
                let stderr = stderr.map_err(|error| {
                    anyhow::anyhow!("session_context hook stderr read failed: {error}")
                })?;
                if !status.success() {
                    return Err(anyhow::anyhow!(
                        "session_context hook exited unsuccessfully (status={status}, stderr={})",
                        String::from_utf8_lossy(&stderr.bytes)
                    ));
                }
                let context = parse_session_context_output(output.bytes)?;
                let next_len = combined_len
                    .saturating_add(usize::from(!contexts.is_empty()))
                    .saturating_add(context.len());
                if next_len > SESSION_CONTEXT_LIMIT {
                    return Err(anyhow::anyhow!(
                        "combined session_context output exceeded {} bytes",
                        SESSION_CONTEXT_LIMIT
                    ));
                }
                combined_len = next_len;
                contexts.push(context);
            }
        }
    }
    let joined = contexts.join("\n");
    if joined.len() > SESSION_CONTEXT_LIMIT {
        return Err(anyhow::anyhow!(
            "combined session_context output exceeded {} bytes",
            SESSION_CONTEXT_LIMIT
        ));
    }
    Ok(joined)
}

/// The first configured command for `event`, retained for scalar callers.
pub fn hook_command(event: &str) -> Option<String> {
    hook_commands(event).into_iter().next()
}

/// Whether a hook is configured for `event`. Cheap; used by hot paths to
/// skip payload construction entirely when no hook is set.
pub fn hook_configured(event: &str) -> bool {
    !hook_commands(event).is_empty()
}

/// True when running inside a hook process (recursion guard).
fn hooks_suppressed() -> bool {
    std::env::var_os("JCODE_HOOKS_DISABLED").is_some()
}

fn expand_home(program: &str) -> PathBuf {
    if let Some(rest) = program.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(program)
}

fn truncate_bytes(value: &str, limit: usize) -> &str {
    if value.len() <= limit {
        return value;
    }
    let mut end = limit;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

/// JSON payload mirroring the env fields, exported as `JCODE_HOOK_PAYLOAD`.
fn payload_json(event: &HookEvent) -> String {
    let mut map = serde_json::Map::new();
    map.insert(
        "event".to_string(),
        serde_json::Value::String(event.event.to_string()),
    );
    if let Some(session_id) = &event.session_id {
        map.insert(
            "session_id".to_string(),
            serde_json::Value::String(session_id.clone()),
        );
    }
    if let Some(cwd) = &event.cwd {
        map.insert("cwd".to_string(), serde_json::Value::String(cwd.clone()));
    }
    for (key, value) in &event.fields {
        map.insert(
            key.to_ascii_lowercase(),
            serde_json::Value::String(value.clone()),
        );
    }
    let payload = serde_json::Value::Object(map).to_string();
    truncate_bytes(&payload, PAYLOAD_ENV_LIMIT).to_string()
}

fn apply_event_env(cmd: &mut std::process::Command, event: &HookEvent) {
    cmd.env("JCODE_HOOKS_DISABLED", "1");
    cmd.env("JCODE_HOOK_EVENT", event.event);
    if let Some(session_id) = &event.session_id {
        cmd.env("JCODE_HOOK_SESSION_ID", session_id);
    }
    if let Some(cwd) = &event.cwd {
        cmd.env("JCODE_HOOK_CWD", cwd);
    }
    for (key, value) in &event.fields {
        cmd.env(format!("JCODE_HOOK_{key}"), value);
    }
    cmd.env("JCODE_HOOK_PAYLOAD", payload_json(event));
}

fn build_hook_process(
    command_line: &str,
    event: &HookEvent,
) -> anyhow::Result<std::process::Command> {
    let parts = crate::terminal_launch::parse_hook_command(command_line)?;
    let (program, args) = parts
        .split_first()
        .expect("parse_hook_command guarantees at least one part");
    let mut cmd = std::process::Command::new(expand_home(program));
    cmd.args(args);
    if let Some(cwd) = event.cwd.as_deref().filter(|cwd| !cwd.is_empty())
        && std::path::Path::new(cwd).is_dir()
    {
        cmd.current_dir(cwd);
    }
    apply_event_env(&mut cmd, event);
    let _ = CLIENT_TERMINAL_ENV.try_with(|env| {
        crate::terminal_launch::apply_client_terminal_env(&mut cmd, env);
    });
    Ok(cmd)
}

/// Fire an observer hook for `event` if one is configured.
///
/// Detached and fire-and-forget: failures are logged, never propagated, and
/// the hook process cannot block the agent.
pub fn dispatch_observer(event: HookEvent) {
    let command_lines = hook_commands(event.event);
    if command_lines.is_empty() {
        return;
    }
    let event_name = event.event;
    for command_line in command_lines {
        match build_hook_process(&command_line, &event) {
            Ok(mut cmd) => {
                cmd.stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null());
                match crate::platform::spawn_detached(&mut cmd) {
                    Ok(child) => {
                        crate::platform::reap_detached(child);
                        crate::logging::debug(&format!(
                            "Hook '{event_name}' dispatched to '{command_line}' (session={:?})",
                            event.session_id
                        ));
                    }
                    Err(error) => crate::logging::warn(&format!(
                        "Hook '{event_name}' command '{command_line}' failed to start: {error}"
                    )),
                }
            }
            Err(error) => crate::logging::warn(&format!(
                "Hook '{event_name}' command '{command_line}' is invalid: {error}"
            )),
        }
    }
}

/// Run the `pre_tool` gate hook for a tool call, if configured.
///
/// The hook receives `JCODE_HOOK_TOOL_NAME` plus the full tool input JSON on
/// stdin (and truncated in `JCODE_HOOK_TOOL_INPUT`). Contract:
///
/// - exit 0: allow the tool call
/// - exit 2: block it; stderr becomes the error shown to the model
/// - anything else (other exits, timeout, spawn failure): fail open
pub async fn run_pre_tool_gate(
    session_id: &str,
    working_dir: Option<&str>,
    tool_name: &str,
    tool_input_json: &str,
) -> GateDecision {
    let command_lines = hook_commands("pre_tool");
    if command_lines.is_empty() {
        return GateDecision::Allow;
    }

    let mut event = HookEvent::new("pre_tool")
        .session_id(session_id)
        .field("TOOL_NAME", tool_name)
        .field(
            "TOOL_INPUT",
            truncate_bytes(tool_input_json, TOOL_INPUT_ENV_LIMIT),
        );
    if let Some(cwd) = working_dir {
        event = event.cwd(cwd);
    }

    let mut decision = GateDecision::Allow;
    for command_line in command_lines {
        let current = run_pre_tool_command(&command_line, &event, tool_name, tool_input_json).await;
        if matches!(current, GateDecision::Block { .. }) && decision == GateDecision::Allow {
            decision = current;
        }
    }
    decision
}

async fn run_pre_tool_command(
    command_line: &str,
    event: &HookEvent,
    tool_name: &str,
    tool_input_json: &str,
) -> GateDecision {
    let session_id = event.session_id.as_deref().unwrap_or("unknown");
    let std_cmd = match build_hook_process(command_line, event) {
        Ok(cmd) => cmd,
        Err(error) => {
            crate::logging::warn(&format!(
                "Hook 'pre_tool' command '{command_line}' is invalid: {error} (allowing tool call)"
            ));
            return GateDecision::Allow;
        }
    };

    let mut cmd = tokio::process::Command::from(std_cmd);
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(error) => {
            crate::logging::warn(&format!(
                "Hook 'pre_tool' command '{command_line}' failed to start: {error} (allowing tool call)"
            ));
            return GateDecision::Allow;
        }
    };

    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        let _ = stdin.write_all(tool_input_json.as_bytes()).await;
        // Closing stdin signals EOF to hooks that read the whole input.
        drop(stdin);
    }

    let timeout =
        std::time::Duration::from_millis(crate::config::config().hooks.pre_tool_timeout_ms.max(1));
    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            crate::logging::warn(&format!(
                "Hook 'pre_tool' command '{command_line}' failed: {error} (allowing tool call)"
            ));
            return GateDecision::Allow;
        }
        Err(_elapsed) => {
            crate::logging::warn(&format!(
                "Hook 'pre_tool' command '{command_line}' timed out after {}ms (allowing tool call)",
                timeout.as_millis()
            ));
            return GateDecision::Allow;
        }
    };

    match output.status.code() {
        Some(0) => GateDecision::Allow,
        Some(2) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let reason = stderr.trim();
            let reason = if reason.is_empty() {
                "blocked by pre_tool hook".to_string()
            } else {
                truncate_bytes(reason, BLOCK_REASON_LIMIT).to_string()
            };
            crate::logging::info(&format!(
                "Hook 'pre_tool' blocked tool '{tool_name}' for session {session_id}: {reason}"
            ));
            GateDecision::Block { reason }
        }
        other => {
            crate::logging::warn(&format!(
                "Hook 'pre_tool' command '{command_line}' exited with {other:?} (expected 0=allow or 2=block; allowing tool call)"
            ));
            GateDecision::Allow
        }
    }
}

#[cfg(test)]
#[allow(clippy::await_holding_lock)]
mod tests {
    use super::*;

    #[test]
    fn session_context_event_fields_are_available_to_hooks() {
        let event = HookEvent::new("session_context")
            .session_id("ses_context")
            .field("SOURCE", "create")
            .field("MODEL", "test-model");
        assert_eq!(event.event, "session_context");
        assert_eq!(event.session_id.as_deref(), Some("ses_context"));
    }

    #[test]
    fn payload_json_includes_event_and_lowercased_fields() {
        let event = HookEvent::new("turn_end")
            .session_id("ses_x")
            .cwd("/work")
            .field("STATUS", "ok")
            .field("DURATION_MS", "1200");
        let payload: serde_json::Value = serde_json::from_str(&payload_json(&event)).unwrap();
        assert_eq!(payload["event"], "turn_end");
        assert_eq!(payload["session_id"], "ses_x");
        assert_eq!(payload["cwd"], "/work");
        assert_eq!(payload["status"], "ok");
        assert_eq!(payload["duration_ms"], "1200");
    }

    #[test]
    fn truncate_bytes_respects_char_boundaries() {
        let text = "héllo wörld";
        let truncated = truncate_bytes(text, 3);
        assert!(truncated.len() <= 3);
        assert!(text.starts_with(truncated));
        assert_eq!(truncate_bytes("short", 100), "short");
    }

    #[cfg(unix)]
    fn write_executable_script(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, body).expect("write script");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod script");
        path
    }

    #[cfg(unix)]
    fn gate_test_config(hook: &str, timeout_ms: u64) -> impl Drop + use<> {
        struct EnvReset(Vec<(&'static str, Option<std::ffi::OsString>)>);
        impl Drop for EnvReset {
            fn drop(&mut self) {
                for (key, previous) in self.0.drain(..) {
                    match previous {
                        Some(value) => crate::env::set_var(key, value),
                        None => crate::env::remove_var(key),
                    }
                }
            }
        }
        let reset = EnvReset(vec![
            (
                "JCODE_HOOK_PRE_TOOL",
                std::env::var_os("JCODE_HOOK_PRE_TOOL"),
            ),
            (
                "JCODE_HOOK_PRE_TOOL_TIMEOUT_MS",
                std::env::var_os("JCODE_HOOK_PRE_TOOL_TIMEOUT_MS"),
            ),
        ]);
        crate::env::set_var("JCODE_HOOK_PRE_TOOL", hook);
        crate::env::set_var("JCODE_HOOK_PRE_TOOL_TIMEOUT_MS", timeout_ms.to_string());
        reset
    }

    #[cfg(unix)]
    fn session_context_test_config(hook: &str, timeout_ms: u64) -> impl Drop + use<> {
        struct EnvReset(Vec<(&'static str, Option<std::ffi::OsString>)>);
        impl Drop for EnvReset {
            fn drop(&mut self) {
                for (key, previous) in self.0.drain(..) {
                    match previous {
                        Some(value) => crate::env::set_var(key, value),
                        None => crate::env::remove_var(key),
                    }
                }
                crate::config::invalidate_config_cache();
            }
        }
        let reset = EnvReset(vec![
            (
                "JCODE_HOOK_SESSION_CONTEXT",
                std::env::var_os("JCODE_HOOK_SESSION_CONTEXT"),
            ),
            (
                "JCODE_HOOK_SESSION_CONTEXT_TIMEOUT_MS",
                std::env::var_os("JCODE_HOOK_SESSION_CONTEXT_TIMEOUT_MS"),
            ),
        ]);
        crate::env::set_var("JCODE_HOOK_SESSION_CONTEXT", hook);
        crate::env::set_var(
            "JCODE_HOOK_SESSION_CONTEXT_TIMEOUT_MS",
            timeout_ms.to_string(),
        );
        crate::config::invalidate_config_cache();
        reset
    }

    #[cfg(unix)]
    #[test]
    fn session_context_rejects_empty_configured_output() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::TempDir::new().expect("temp dir");
        let hook = write_executable_script(temp.path(), "empty.sh", "#!/bin/sh\nexit 0\n");
        let _env = session_context_test_config(&hook.to_string_lossy(), 5000);

        let error = run_session_context(
            HookEvent::new("session_context")
                .session_id("ses_empty")
                .field("SOURCE", "create"),
        )
        .expect_err("configured empty output must fail closed");

        assert!(error.to_string().contains("returned no context"));
    }

    #[cfg(unix)]
    #[test]
    fn session_context_rejects_json_without_a_string_context_field() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::TempDir::new().expect("temp dir");
        let hook = write_executable_script(
            temp.path(),
            "invalid-json-contract.sh",
            "#!/bin/sh\nprintf '%s' '{\"hookSpecificOutput\":{\"additionalContext\":42}}'\n",
        );
        let _env = session_context_test_config(&hook.to_string_lossy(), 5000);

        let error = run_session_context(
            HookEvent::new("session_context")
                .session_id("ses_invalid_json")
                .field("SOURCE", "attach"),
        )
        .expect_err("JSON output without a string context must fail closed");

        assert!(error.to_string().contains("string additionalContext"));
    }

    #[cfg(unix)]
    #[test]
    fn session_context_combines_plain_text_and_json_in_declaration_order() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::TempDir::new().expect("temp dir");
        let first = write_executable_script(
            temp.path(),
            "first.sh",
            "#!/bin/sh\nprintf 'plain:%s:%s' \"$JCODE_HOOK_SOURCE\" \"$JCODE_HOOK_SESSION_ID\"\n",
        );
        let second = write_executable_script(
            temp.path(),
            "second.sh",
            "#!/bin/sh\nprintf '%s' '{\"hookSpecificOutput\":{\"additionalContext\":\"json-second\"}}'\n",
        );
        let commands = serde_json::to_string(&vec![
            first.to_string_lossy().into_owned(),
            second.to_string_lossy().into_owned(),
        ])
        .expect("serialize hook command array");
        let _env = session_context_test_config(&commands, 5000);

        let context = run_session_context(
            HookEvent::new("session_context")
                .session_id("ses_ordered")
                .field("SOURCE", "resume"),
        )
        .expect("ordered hooks should succeed");

        assert_eq!(context, "plain:resume:ses_ordered\njson-second");
    }

    #[cfg(unix)]
    #[test]
    fn session_context_stops_before_running_hooks_beyond_the_combined_limit() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::TempDir::new().expect("temp dir");
        let marker = temp.path().join("third-hook-ran");
        let first = write_executable_script(
            temp.path(),
            "first-large.sh",
            "#!/bin/sh\nhead -c 40000 /dev/zero | tr '\\000' a\n",
        );
        let second = write_executable_script(
            temp.path(),
            "second-large.sh",
            "#!/bin/sh\nhead -c 30000 /dev/zero | tr '\\000' b\n",
        );
        let third = write_executable_script(
            temp.path(),
            "third-side-effect.sh",
            &format!("#!/bin/sh\ntouch '{}'\nprintf third\n", marker.display()),
        );
        let commands = serde_json::to_string(&vec![
            first.to_string_lossy().into_owned(),
            second.to_string_lossy().into_owned(),
            third.to_string_lossy().into_owned(),
        ])
        .expect("serialize hook command array");
        let _env = session_context_test_config(&commands, 5000);

        let error = run_session_context(HookEvent::new("session_context"))
            .expect_err("combined output over the cap must fail closed");

        assert!(
            error
                .to_string()
                .contains("combined session_context output exceeded")
        );
        assert!(
            !marker.exists(),
            "hooks after the cumulative output cap must not run"
        );
    }

    #[cfg(unix)]
    #[test]
    fn session_context_fails_closed_for_process_and_output_errors() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::TempDir::new().expect("temp dir");
        let event = || HookEvent::new("session_context").session_id("ses_fail_closed");

        {
            let _env = session_context_test_config("/nonexistent/session-context-hook", 5000);
            let error = run_session_context(event()).expect_err("spawn failure must block");
            assert!(error.to_string().contains("failed to start"));
        }

        let nonzero = write_executable_script(
            temp.path(),
            "nonzero.sh",
            "#!/bin/sh\necho 'dossier bootstrap failed' >&2\nexit 7\n",
        );
        {
            let _env = session_context_test_config(&nonzero.to_string_lossy(), 5000);
            let error = run_session_context(event()).expect_err("nonzero exit must block");
            assert!(error.to_string().contains("dossier bootstrap failed"));
        }

        let non_utf8 =
            write_executable_script(temp.path(), "non-utf8.sh", "#!/bin/sh\nprintf '\\377'\n");
        {
            let _env = session_context_test_config(&non_utf8.to_string_lossy(), 5000);
            let error = run_session_context(event()).expect_err("non-UTF-8 output must block");
            assert!(error.to_string().contains("non-UTF-8"));
        }

        let oversized = write_executable_script(
            temp.path(),
            "oversized.sh",
            "#!/bin/sh\nhead -c 65537 /dev/zero | tr '\\000' x\n",
        );
        {
            let _env = session_context_test_config(&oversized.to_string_lossy(), 5000);
            let error = run_session_context(event()).expect_err("oversized output must block");
            assert!(error.to_string().contains("exceeded 65536 bytes"));
        }

        let timeout = write_executable_script(temp.path(), "timeout.sh", "#!/bin/sh\nsleep 30\n");
        {
            let _env = session_context_test_config(&timeout.to_string_lossy(), 50);
            let started = std::time::Instant::now();
            let error = run_session_context(event()).expect_err("timeout must block");
            assert!(error.to_string().contains("timed out after 50ms"));
            assert!(started.elapsed() < std::time::Duration::from_secs(5));
        }
    }

    #[cfg(unix)]
    #[test]
    fn session_context_aborts_immediately_after_the_output_limit() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::TempDir::new().expect("temp dir");
        let hook = write_executable_script(
            temp.path(),
            "oversized-then-sleep.sh",
            "#!/bin/sh\nhead -c 65537 /dev/zero | tr '\\000' x\nsleep 30\n",
        );
        let _env = session_context_test_config(&hook.to_string_lossy(), 5000);

        let started = std::time::Instant::now();
        let error = run_session_context(HookEvent::new("session_context"))
            .expect_err("the output cap must terminate the hook immediately");

        assert!(error.to_string().contains("exceeded 65536 bytes"));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(3),
            "the output cap must not wait for the full hook timeout"
        );
    }

    #[cfg(unix)]
    #[test]
    fn session_context_aborts_immediately_after_the_stderr_limit() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::TempDir::new().expect("temp dir");
        let hook = write_executable_script(
            temp.path(),
            "oversized-stderr-then-sleep.sh",
            &format!(
                "#!/bin/sh\nhead -c {} /dev/zero | tr '\\000' e >&2\nsleep 30\n",
                BLOCK_REASON_LIMIT + 1
            ),
        );
        let _env = session_context_test_config(&hook.to_string_lossy(), 5000);

        let started = std::time::Instant::now();
        let error = run_session_context(HookEvent::new("session_context"))
            .expect_err("the stderr cap must terminate the hook immediately");

        let error_text = error.to_string();
        assert!(
            error_text.contains(&format!("stderr exceeded {BLOCK_REASON_LIMIT} bytes")),
            "unexpected stderr cap error: {error_text}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(3),
            "the stderr cap must not wait for the full hook timeout"
        );
    }

    #[cfg(unix)]
    #[test]
    fn session_context_reaps_descendants_that_keep_output_pipes_open() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::TempDir::new().expect("temp dir");
        let hook = write_executable_script(
            temp.path(),
            "leader-exits-descendant-sleeps.sh",
            "#!/bin/sh\nsleep 30 &\nprintf context-ready\n",
        );
        let _env = session_context_test_config(&hook.to_string_lossy(), 5000);

        let started = std::time::Instant::now();
        let context = run_session_context(HookEvent::new("session_context"))
            .expect("a completed hook must not wait for a descendant that inherited its pipes");

        assert_eq!(context, "context-ready");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(3),
            "the completed hook must reap descendants before joining output readers"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pre_tool_gate_allows_on_exit_zero_and_blocks_on_exit_two() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::TempDir::new().expect("temp dir");

        // Blocking hook: reads stdin, writes a reason to stderr, exits 2.
        let block = write_executable_script(
            temp.path(),
            "block.sh",
            "#!/bin/sh\ncat > /dev/null\necho \"dangerous tool: $JCODE_HOOK_TOOL_NAME\" >&2\nexit 2\n",
        );
        {
            let _env = gate_test_config(&block.to_string_lossy(), 5000);
            let decision =
                run_pre_tool_gate("ses_g", None, "bash", r#"{"command":"rm -rf /"}"#).await;
            assert_eq!(
                decision,
                GateDecision::Block {
                    reason: "dangerous tool: bash".to_string()
                }
            );
        }

        // Allowing hook: exit 0.
        let allow = write_executable_script(temp.path(), "allow.sh", "#!/bin/sh\nexit 0\n");
        {
            let _env = gate_test_config(&allow.to_string_lossy(), 5000);
            let decision = run_pre_tool_gate("ses_g", None, "read", "{}").await;
            assert_eq!(decision, GateDecision::Allow);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pre_tool_gate_runs_every_configured_command_and_preserves_first_block() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::TempDir::new().expect("temp dir");
        let first_marker = temp.path().join("first-ran.txt");
        let final_marker = temp.path().join("final-ran.txt");
        let allow = write_executable_script(
            temp.path(),
            "first-allow.sh",
            &format!(
                "#!/bin/sh\nprintf ran > {}\nexit 0\n",
                crate::terminal_launch::sh_escape(&first_marker.to_string_lossy())
            ),
        );
        let block = write_executable_script(
            temp.path(),
            "second-block.sh",
            "#!/bin/sh\necho 'blocked by second policy' >&2\nexit 2\n",
        );
        let final_allow = write_executable_script(
            temp.path(),
            "third-allow.sh",
            &format!(
                "#!/bin/sh\nprintf ran > {}\nexit 0\n",
                crate::terminal_launch::sh_escape(&final_marker.to_string_lossy())
            ),
        );
        let commands = serde_json::to_string(&vec![
            allow.to_string_lossy().into_owned(),
            block.to_string_lossy().into_owned(),
            final_allow.to_string_lossy().into_owned(),
        ])
        .expect("serialize hook command array");
        let _env = gate_test_config(&commands, 5000);

        let decision = run_pre_tool_gate("ses_multi", None, "bash", "{}").await;

        assert_eq!(
            decision,
            GateDecision::Block {
                reason: "blocked by second policy".to_string()
            }
        );
        assert_eq!(
            std::fs::read_to_string(first_marker).expect("first policy should execute"),
            "ran"
        );
        assert_eq!(
            std::fs::read_to_string(final_marker).expect("later policies should still execute"),
            "ran"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pre_tool_gate_fails_open_on_timeout_and_odd_exits() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::TempDir::new().expect("temp dir");

        // Hook that hangs: must fail open after the timeout.
        let hang = write_executable_script(temp.path(), "hang.sh", "#!/bin/sh\nsleep 30\n");
        {
            let _env = gate_test_config(&hang.to_string_lossy(), 200);
            let decision = run_pre_tool_gate("ses_g", None, "bash", "{}").await;
            assert_eq!(decision, GateDecision::Allow);
        }

        // Hook with an unexpected exit code: fail open.
        let odd = write_executable_script(temp.path(), "odd.sh", "#!/bin/sh\nexit 7\n");
        {
            let _env = gate_test_config(&odd.to_string_lossy(), 5000);
            let decision = run_pre_tool_gate("ses_g", None, "bash", "{}").await;
            assert_eq!(decision, GateDecision::Allow);
        }

        // Missing hook binary: fail open.
        {
            let _env = gate_test_config("/nonexistent/hook-binary", 5000);
            let decision = run_pre_tool_gate("ses_g", None, "bash", "{}").await;
            assert_eq!(decision, GateDecision::Allow);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pre_tool_gate_receives_input_on_stdin() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::TempDir::new().expect("temp dir");
        let record = temp.path().join("stdin.txt");
        let script = write_executable_script(
            temp.path(),
            "record.sh",
            &format!(
                "#!/bin/sh\ncat > {}\nexit 0\n",
                crate::terminal_launch::sh_escape(&record.to_string_lossy())
            ),
        );
        let _env = gate_test_config(&script.to_string_lossy(), 5000);
        let input = r#"{"file_path":"/tmp/x","content":"hello"}"#;
        let decision = run_pre_tool_gate("ses_g", None, "write", input).await;
        assert_eq!(decision, GateDecision::Allow);
        let recorded = std::fs::read_to_string(&record).expect("stdin should be recorded");
        assert_eq!(recorded, input);
    }

    #[cfg(unix)]
    #[test]
    fn observer_dispatch_runs_hook_with_event_env() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::TempDir::new().expect("temp dir");
        let record = temp.path().join("event.txt");
        let script = write_executable_script(
            temp.path(),
            "observe.sh",
            &format!(
                "#!/bin/sh\nprintf '%s|%s|%s|%s' \"$JCODE_HOOK_EVENT\" \"$JCODE_HOOK_SESSION_ID\" \"$JCODE_HOOK_STATUS\" \"$JCODE_HOOKS_DISABLED\" > {}\n",
                crate::terminal_launch::sh_escape(&record.to_string_lossy())
            ),
        );

        let prev = std::env::var_os("JCODE_HOOK_TURN_END");
        crate::env::set_var("JCODE_HOOK_TURN_END", script.to_string_lossy().to_string());

        dispatch_observer(
            HookEvent::new("turn_end")
                .session_id("ses_obs")
                .field("STATUS", "ok"),
        );

        let mut recorded = String::new();
        for _ in 0..100 {
            if let Ok(data) = std::fs::read_to_string(&record)
                && !data.is_empty()
            {
                recorded = data;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        match prev {
            Some(value) => crate::env::set_var("JCODE_HOOK_TURN_END", value),
            None => crate::env::remove_var("JCODE_HOOK_TURN_END"),
        }
        assert_eq!(recorded, "turn_end|ses_obs|ok|1");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn observer_dispatch_reaps_completed_hook() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::TempDir::new().expect("temp dir");
        let record = temp.path().join("pid.txt");
        let script = write_executable_script(
            temp.path(),
            "record-pid.sh",
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$$\" > {}\n",
                crate::terminal_launch::sh_escape(&record.to_string_lossy())
            ),
        );

        let previous = std::env::var_os("JCODE_HOOK_TURN_END");
        crate::env::set_var("JCODE_HOOK_TURN_END", script.to_string_lossy().to_string());
        dispatch_observer(HookEvent::new("turn_end").session_id("ses_reap"));

        let mut pid: Option<u32> = None;
        for _ in 0..100 {
            pid = std::fs::read_to_string(&record)
                .ok()
                .and_then(|value| value.strip_suffix('\n').and_then(|pid| pid.parse().ok()));
            if pid.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        match previous {
            Some(value) => crate::env::set_var("JCODE_HOOK_TURN_END", value),
            None => crate::env::remove_var("JCODE_HOOK_TURN_END"),
        }

        let pid = pid.expect("hook should record its pid");
        let process = std::path::PathBuf::from(format!("/proc/{pid}"));
        for _ in 0..100 {
            if !process.exists() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("completed hook process {pid} was not reaped");
    }

    #[cfg(unix)]
    #[test]
    fn observer_dispatch_runs_each_configured_command() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::TempDir::new().expect("temp dir");
        let first_record = temp.path().join("first.txt");
        let second_record = temp.path().join("second.txt");
        let first = write_executable_script(
            temp.path(),
            "first.sh",
            &format!(
                "#!/bin/sh\nprintf first > {}\n",
                crate::terminal_launch::sh_escape(&first_record.to_string_lossy())
            ),
        );
        let second = write_executable_script(
            temp.path(),
            "second.sh",
            &format!(
                "#!/bin/sh\nprintf second > {}\n",
                crate::terminal_launch::sh_escape(&second_record.to_string_lossy())
            ),
        );
        let previous = std::env::var_os("JCODE_HOOK_SESSION_START");
        crate::env::set_var(
            "JCODE_HOOK_SESSION_START",
            format!(
                "[{:?}, {:?}]",
                first.to_string_lossy(),
                second.to_string_lossy()
            ),
        );

        dispatch_observer(HookEvent::new("session_start").session_id("ses_multi"));
        for _ in 0..100 {
            if first_record.exists() && second_record.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        match previous {
            Some(value) => crate::env::set_var("JCODE_HOOK_SESSION_START", value),
            None => crate::env::remove_var("JCODE_HOOK_SESSION_START"),
        }
        assert_eq!(std::fs::read_to_string(first_record).unwrap(), "first");
        assert_eq!(std::fs::read_to_string(second_record).unwrap(), "second");
    }

    #[tokio::test]
    async fn concurrent_client_terminal_environments_remain_isolated() {
        async fn pane_id(env: Vec<(String, String)>) -> Option<String> {
            with_client_terminal_env(env, async {
                let event = HookEvent::new("session_start");
                let command = build_hook_process("hook", &event).unwrap();
                command.get_envs().find_map(|(key, value)| {
                    (key == "HERDR_PANE_ID")
                        .then(|| value.map(|value| value.to_string_lossy().into_owned()))
                        .flatten()
                })
            })
            .await
        }

        let (left, right) = tokio::join!(
            pane_id(vec![("HERDR_PANE_ID".to_string(), "pane-left".to_string())]),
            pane_id(vec![(
                "HERDR_PANE_ID".to_string(),
                "pane-right".to_string()
            )]),
        );
        assert_eq!(left.as_deref(), Some("pane-left"));
        assert_eq!(right.as_deref(), Some("pane-right"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hook_process_replaces_daemon_terminal_env_with_client_snapshot() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::TempDir::new().expect("temp dir");
        let script = write_executable_script(
            temp.path(),
            "env.sh",
            "#!/bin/sh\nprintf '%s|%s|%s|%s' \"$TMUX_PANE\" \"$HERDR_PANE_ID\" \"$JCODE_CLIENT_TMUX_PANE\" \"$JCODE_CLIENT_HERDR_PANE_ID\"\n",
        );
        let previous_tmux = std::env::var_os("TMUX_PANE");
        let previous_herdr = std::env::var_os("HERDR_PANE_ID");
        crate::env::set_var("TMUX_PANE", "daemon-pane");
        crate::env::set_var("HERDR_PANE_ID", "daemon-herdr");

        let run_for_pane = |tmux: &'static str, herdr: &'static str| {
            let script = script.clone();
            with_client_terminal_env(
                vec![
                    ("TMUX_PANE".to_string(), tmux.to_string()),
                    ("HERDR_PANE_ID".to_string(), herdr.to_string()),
                ],
                async move {
                    tokio::task::yield_now().await;
                    build_hook_process(&script.to_string_lossy(), &HookEvent::new("turn_start"))
                        .expect("hook command")
                        .output()
                        .expect("run hook")
                },
            )
        };
        let (first_output, second_output) = tokio::join!(
            run_for_pane("client-pane-a", "herdr-pane-a"),
            run_for_pane("client-pane-b", "herdr-pane-b")
        );

        match previous_tmux {
            Some(value) => crate::env::set_var("TMUX_PANE", value),
            None => crate::env::remove_var("TMUX_PANE"),
        }
        match previous_herdr {
            Some(value) => crate::env::set_var("HERDR_PANE_ID", value),
            None => crate::env::remove_var("HERDR_PANE_ID"),
        }
        assert!(first_output.status.success());
        assert!(second_output.status.success());
        assert_eq!(
            String::from_utf8_lossy(&first_output.stdout),
            "client-pane-a|herdr-pane-a|client-pane-a|herdr-pane-a"
        );
        assert_eq!(
            String::from_utf8_lossy(&second_output.stdout),
            "client-pane-b|herdr-pane-b|client-pane-b|herdr-pane-b"
        );
    }
}
