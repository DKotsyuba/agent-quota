//! Bounded execution of operator-owned quota commands, independent of language or provider.

use crate::process::OwnedProcess;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    os::unix::fs::{DirBuilderExt, MetadataExt},
    os::unix::process::ExitStatusExt,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

/// Operator-owned external quota command; credentials belong only to its stdin context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CollectorBinding {
    /// Absolute executable path; arguments are passed literally without expansion.
    #[serde(default, skip_serializing_if = "collector_command_empty")]
    pub command: PathBuf,
    /// Literal arguments; never include credentials here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Stable source label, or empty for a caller-derived command identity.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source: String,
    /// Execution deadline in seconds, including streams, from 1 to 300.
    #[serde(
        default = "collector_timeout",
        skip_serializing_if = "collector_default_timeout"
    )]
    pub timeout_seconds: u64,
    /// Additional host environment names explicitly granted to the command.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_from: Vec<String>,
    /// Native exhaustion window names mapped to physical pools by the operator.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub exhaustion_windows: BTreeMap<String, String>,
}

/// Returns the default collector execution deadline in seconds.
fn collector_timeout() -> u64 {
    30
}

/// Omits the default deadline when serializing a binding.
fn collector_default_timeout(value: &u64) -> bool {
    *value == collector_timeout()
}

/// Omits an absent executable path when serializing an unvalidated binding.
fn collector_command_empty(value: &Path) -> bool {
    value.as_os_str().is_empty()
}

impl CollectorBinding {
    /// Rejects unsafe paths, oversized arguments and invalid configuration before spawn.
    /// Errors contain fixed descriptions and never echo operator-supplied values.
    pub(crate) fn validate(&self) -> Result<(), String> {
        if !self.command.is_absolute() || self.command.as_os_str().len() > 4096 {
            return Err("collector command must be an absolute path".into());
        }
        if !(1..=300).contains(&self.timeout_seconds)
            || self.args.len() > 64
            || self
                .args
                .iter()
                .any(|arg| arg.len() > 4096 || arg.contains('\0'))
        {
            return Err("invalid collector arguments or timeout".into());
        }
        if self.source.len() > 128 || self.source.chars().any(char::is_control) {
            return Err("invalid collector source".into());
        }
        if self.env_from.len() > 32
            || self.env_from.iter().any(|name| {
                name.is_empty()
                    || name.len() > 128
                    || name.as_bytes()[0].is_ascii_digit()
                    || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            })
        {
            return Err("invalid collector environment name".into());
        }
        if self.exhaustion_windows.iter().any(|(window, pool)| {
            [window, pool]
                .into_iter()
                .any(|value| value.trim().is_empty() || value.contains('\0') || value.len() > 128)
        }) {
            return Err("invalid collector exhaustion mapping".into());
        }
        Ok(())
    }
}

/// Maximum context or quota JSON bytes; stderr has a smaller independent ceiling.
const MAX_BYTES: usize = 2 * 1024 * 1024;

/// Prepares an owner-private temporary directory inside the collector's service home.
fn temp_dir(home: &Path) -> Result<PathBuf, String> {
    let path = home.join("tmp");
    match std::fs::DirBuilder::new().mode(0o700).create(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return Err("collector_temp_unavailable".into()),
    }
    let metadata = std::fs::symlink_metadata(&path).map_err(|_| "collector_temp_unavailable")?;
    // SAFETY: geteuid has no pointer arguments or side effects.
    let uid = unsafe { libc::geteuid() };
    if !metadata.file_type().is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Err("collector_temp_unavailable".into());
    }
    Ok(path)
}

/// Reads one stream without exceeding its byte ceiling, including when EOF never arrives.
/// The caller owns the deadline and drops this future before cleaning up the process.
async fn bounded_read(reader: impl AsyncRead + Unpin, max: usize) -> Result<Vec<u8>, &'static str> {
    let mut bytes = Vec::new();
    reader
        .take(max as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| "collector_io_failed")?;
    if bytes.len() > max {
        return Err("collector_output_too_large");
    }
    Ok(bytes)
}

/// Ensures cancellation of the collector future also tears down its captured processes.
struct ProcessGuard {
    /// Exact leader and observed descendant identities.
    owner: OwnedProcess,
    /// True until the ordinary asynchronous cleanup has been attempted.
    armed: bool,
}
impl Drop for ProcessGuard {
    /// A cancelled future cannot await: use the existing bounded synchronous cleanup.
    fn drop(&mut self) {
        if self.armed {
            let _ = self.owner.cleanup_blocking(Duration::from_millis(250));
        }
    }
}

/// Executes exactly the configured command/args with private JSON stdin and bounded streams.
///
/// The context may contain credentials and is never persisted or put in argv. The
/// environment is cleared, retaining basic host paths and explicitly granted names.
/// A zero exit, one JSON document and confirmed process cleanup are all required.
/// Provider output and OS error text never escape this boundary. Nonzero termination
/// returns only a numeric `collector_exit_status:N` or `collector_exit_signal:N` code,
/// or `collector_exit_unknown` when the operating system provides neither;
/// the caller validates quota semantics and checks for echoes of a directly
/// supplied token before storage. Native-login directories provide no token to
/// that check, so collector code must be trusted.
pub(crate) async fn run(
    binding: &CollectorBinding,
    context: &Value,
    cwd: &Path,
) -> Result<Value, String> {
    binding
        .validate()
        .map_err(|_| "collector_config_invalid".to_owned())?;
    let input = serde_json::to_vec(context).map_err(|_| "collector_input_invalid".to_owned())?;
    if input.len() > MAX_BYTES {
        return Err("collector_input_too_large".into());
    }
    let deadline_at = tokio::time::Instant::now() + Duration::from_secs(binding.timeout_seconds);
    let mut environment: BTreeMap<String, String> =
        ["HOME", "PATH", "USER", "LOGNAME", "LANG", "LC_ALL"]
            .into_iter()
            .chain(binding.env_from.iter().map(String::as_str))
            .filter_map(|name| {
                std::env::var(name)
                    .ok()
                    .map(|value| (name.to_owned(), value))
            })
            .collect();
    environment
        .entry("PATH".into())
        .or_insert_with(|| "/usr/bin:/bin:/usr/sbin:/sbin".into());
    let temp = temp_dir(cwd)?;
    let temp = temp.to_str().ok_or("collector_temp_unavailable")?;
    for key in ["TMPDIR", "TMP", "TEMP"] {
        environment.insert(key.to_owned(), temp.to_owned());
    }
    let mut command = tokio::process::Command::new(&binding.command);
    command
        .args(&binding.args)
        .current_dir(cwd)
        .env_clear()
        .envs(environment)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    use std::os::unix::process::CommandExt;
    command.as_std_mut().process_group(0);
    let mut child = command.spawn().map_err(|_| "collector_spawn_failed")?;
    let pid = child
        .id()
        .and_then(|pid| i32::try_from(pid).ok())
        .ok_or("collector_process_identity_unavailable")?;
    let mut guard = ProcessGuard {
        owner: OwnedProcess::capture(pid),
        armed: true,
    };
    let mut stdin = child.stdin.take().ok_or("collector_pipe_unavailable")?;
    let stdout = child.stdout.take().ok_or("collector_pipe_unavailable")?;
    let stderr = child.stderr.take().ok_or("collector_pipe_unavailable")?;
    let write = async move {
        stdin
            .write_all(&input)
            .await
            .map_err(|_| "collector_io_failed")?;
        stdin.shutdown().await.map_err(|_| "collector_io_failed")?;
        drop(stdin);
        Ok::<_, &'static str>(())
    };
    let streams = async {
        tokio::try_join!(
            write,
            bounded_read(stdout, MAX_BYTES),
            bounded_read(stderr, 64 * 1024)
        )
    };
    tokio::pin!(streams);
    let deadline = tokio::time::sleep_until(deadline_at);
    tokio::pin!(deadline);
    let mut tick = tokio::time::interval(Duration::from_millis(25));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut output = None;
    // Observe the actual child independently of EOF: inherited pipes must never
    // keep a finished script alive until the whole collection deadline.
    let status = loop {
        tokio::select! {
            result = child.wait() => break result.map_err(|_| "collector_wait_failed"),
            result = &mut streams, if output.is_none() => {
                match result {
                    Ok(((), stdout, _stderr)) => output = Some(stdout),
                    Err(code) => break Err(code),
                }
            }
            _ = &mut deadline => break Err("collector_timeout"),
            _ = tick.tick() => guard.owner.refresh(),
        }
    };
    let cleanup = guard.owner.cleanup(Duration::from_millis(250)).await;
    guard.armed = false;
    let reaped = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
    if !cleanup.is_ok_and(|proof| proof.confirmed) || !matches!(reaped, Ok(Ok(_))) {
        return Err("collector_cleanup_unverified".into());
    }
    let status = status?;
    if !status.success() {
        let code = match status.code() {
            Some(code) => format!("collector_exit_status:{code}"),
            None => status.signal().map_or_else(
                || "collector_exit_unknown".to_owned(),
                |signal| format!("collector_exit_signal:{signal}"),
            ),
        };
        return Err(code);
    }
    let output = match output {
        Some(output) => output,
        None => {
            tokio::time::timeout_at(deadline_at, &mut streams)
                .await
                .map_err(|_| "collector_pipe_timeout")??
                .1
        }
    };
    serde_json::from_slice(&output).map_err(|_| "collector_output_invalid".to_owned())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Builds an isolated shell fixture without reading provider configuration or credentials.
    fn binding(script: &str) -> CollectorBinding {
        CollectorBinding {
            command: PathBuf::from("/bin/sh"),
            args: vec!["-c".into(), script.into()],
            source: "synthetic".into(),
            timeout_seconds: 3,
            env_from: Vec::new(),
            exhaustion_windows: BTreeMap::new(),
        }
    }

    /// Runs only a supplied synthetic command under an outer hang guard.
    async fn collect(binding: &CollectorBinding, input: &Value) -> Result<Value, String> {
        let directory = tempfile::tempdir().expect("private collector fixture");
        tokio::time::timeout(
            Duration::from_secs(12),
            run(binding, input, directory.path()),
        )
        .await
        .expect("collector must obey execution and cleanup deadlines")
    }

    #[test]
    fn binding_defaults_and_validation_reject_invalid_launches() {
        let mut config: CollectorBinding =
            serde_json::from_str(r#"{"command":"/bin/sh"}"#).unwrap();
        assert_eq!(config.timeout_seconds, 30);
        assert!(config.validate().is_ok());
        assert!(
            serde_json::from_str::<CollectorBinding>(
                r#"{"command":"/bin/sh","legacy":"rejected"}"#
            )
            .is_err()
        );
        config.timeout_seconds = 0;
        assert!(config.validate().is_err());
        config.timeout_seconds = 301;
        assert!(config.validate().is_err());
        config.timeout_seconds = 1;
        config.command = "relative".into();
        assert!(config.validate().is_err());
        config.command = "/bin/sh".into();
        config.env_from.push("1INVALID".into());
        assert!(config.validate().is_err());
        config.env_from.clear();
        config.args.push("NUL\0argument".into());
        assert!(config.validate().is_err());
        config.args.clear();
        config.exhaustion_windows.insert(" ".into(), "pool".into());
        assert!(config.validate().is_err());
    }

    #[tokio::test]
    async fn successful_collector_receives_private_input_and_returns_json() {
        let result = collect(
            &binding("/bin/cat >/dev/null; [ \"$TMPDIR\" = \"$TMP\" ] && [ \"$TMP\" = \"$TEMP\" ] || exit 9; /bin/sleep 0.1; printf '{\"ok\":true}'"),
            &serde_json::json!({"secret": "synthetic-private-canary"}),
        )
        .await
        .unwrap();
        assert_eq!(result, serde_json::json!({"ok": true}));
    }

    #[tokio::test]
    async fn exit_status_signal_and_invalid_json_are_safe_codes() {
        for (script, expected) in [
            (
                "/bin/cat >/dev/null; /bin/sleep 0.1; echo PRIVATE_CANARY >&2; exit 7",
                "collector_exit_status:7",
            ),
            (
                "/bin/cat >/dev/null; /bin/sleep 0.1; kill -TERM $$",
                "collector_exit_signal:15",
            ),
            (
                "/bin/cat >/dev/null; /bin/sleep 0.1; printf PRIVATE_CANARY",
                "collector_output_invalid",
            ),
        ] {
            let error = collect(&binding(script), &Value::Null).await.unwrap_err();
            assert_eq!(error, expected);
            assert!(!error.contains("PRIVATE_CANARY"));
        }
    }

    #[tokio::test]
    async fn bounded_stream_accepts_ceiling_and_rejects_next_byte() {
        for length in [63, 64] {
            assert_eq!(
                bounded_read(&vec![b'x'; length][..], 64)
                    .await
                    .unwrap()
                    .len(),
                length
            );
        }
        assert_eq!(
            bounded_read(&[b'x'; 65][..], 64).await.unwrap_err(),
            "collector_output_too_large"
        );
    }

    #[tokio::test]
    async fn oversized_input_is_rejected_before_spawn() {
        let mut config = binding("");
        config.command = "/this-synthetic-command-does-not-exist".into();
        let input = Value::String("x".repeat(MAX_BYTES));
        assert_eq!(
            collect(&config, &input).await.unwrap_err(),
            "collector_input_too_large"
        );
    }

    #[tokio::test]
    async fn overflowing_stdout_and_stderr_are_bounded_and_cleaned() {
        for script in [
            "/bin/cat >/dev/null; /bin/sleep 0.1; head -c 2097153 /dev/zero; /bin/sleep 30",
            "/bin/cat >/dev/null; /bin/sleep 0.1; head -c 65537 /dev/zero >&2; /bin/sleep 30",
        ] {
            assert_eq!(
                collect(&binding(script), &Value::Null).await.unwrap_err(),
                "collector_output_too_large"
            );
        }
    }

    #[tokio::test]
    async fn execution_and_blocked_stdin_share_the_deadline() {
        for input in [Value::Null, Value::String("x".repeat(MAX_BYTES - 3))] {
            let mut config = binding("exec /bin/sleep 30");
            config.timeout_seconds = 1;
            assert_eq!(
                collect(&config, &input).await.unwrap_err(),
                "collector_timeout"
            );
        }
    }

    #[tokio::test]
    async fn inherited_pipes_are_closed_by_observed_descendant_cleanup() {
        let started = std::time::Instant::now();
        let result = collect(
            &binding("/bin/cat >/dev/null; /bin/sleep 30 & /bin/sleep 0.3; printf '{\"ok\":true}'"),
            &Value::Null,
        )
        .await
        .unwrap();
        assert_eq!(result, serde_json::json!({"ok": true}));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn cancellation_cleans_the_captured_process() {
        let directory = tempfile::tempdir().expect("isolated cancellation fixture");
        let pid_file = directory.path().join("pid");
        let mut config = binding("printf '%s' $$ >\"$1\"; exec /bin/sleep 30");
        config
            .args
            .extend(["fixture".into(), pid_file.to_str().unwrap().into()]);
        let workdir = directory.path().to_path_buf();
        let task = tokio::spawn(async move { run(&config, &Value::Null, &workdir).await });
        let identity = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(contents) = std::fs::read_to_string(&pid_file)
                    && let Ok(pid) = contents.parse::<i32>()
                    && let Ok(identity) = crate::process::inspect(pid)
                {
                    break identity;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let identity = identity.expect("fixture must publish its PID");
        let state = crate::process::observe(
            Some(identity.pid),
            Some(&identity.token),
            Some(identity.birth),
        );
        assert_eq!(state, crate::process::ProcessState::Dead);
    }
}
