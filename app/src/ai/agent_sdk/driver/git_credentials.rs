/// Git credentials management for cloud agent sandboxes.
///
/// This module handles:
/// - Seeding Git's in-memory credential cache so `git` can authenticate without
///   persisting tokens to disk.
/// - One-time git configuration (`credential.helper store`, SSH→HTTPS URL
///   rewrites).
/// - Configuring the git user identity from the server-returned username/email.
/// - An async refresh loop that periodically fetches a fresh token from the
///   server and refreshes the in-memory credentials, keeping long-running agents
///   authenticated for their entire duration.
use std::{io::Write as _, sync::Arc, time::Duration};

use anyhow::{Context, Result};

use crate::server::server_api::ai::{AIClient, GitCredential};

// Use the project's allowed Command wrapper (not std::process::Command, which is
// disallowed by clippy rules because it flashes a terminal window on Windows).
use command::blocking::Command as BlockingCommand;

/// How long to wait between credential refresh attempts (~50 minutes, staying
/// well ahead of the one-hour GitHub token expiry).
pub(crate) const GIT_CREDENTIALS_REFRESH_INTERVAL: Duration = Duration::from_secs(50 * 60);

const DEFAULT_GIT_NAME: &str = "Oz";
const DEFAULT_GIT_EMAIL: &str = "oz-agent@warp.dev";

pub(crate) fn write_git_credentials(credentials: &[GitCredential]) -> Result<()> {
    for cred in credentials {
        let username = cred.username.as_deref().unwrap_or("x-access-token");
        let mut child = BlockingCommand::new("git")
            .args(["credential", "approve"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .context("Failed to start `git credential approve`")?;

        let stdin = child
            .stdin
            .as_mut()
            .context("Failed to open stdin for `git credential approve`")?;
        writeln!(stdin, "protocol=https").context("Failed to write git credential protocol")?;
        writeln!(stdin, "host={}", cred.host).context("Failed to write git credential host")?;
        writeln!(stdin, "username={username}")
            .context("Failed to write git credential username")?;
        writeln!(stdin, "password={}", cred.token)
            .context("Failed to write git credential token")?;
        writeln!(stdin).context("Failed to finalize git credential input")?;
        drop(child.stdin.take());

        let output = child
            .wait_with_output()
            .context("Failed waiting on `git credential approve`")?;
        if !output.status.success() {
            anyhow::bail!(
                "git credential approve failed for {}: {}",
                cred.host,
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
    Ok(())
}

/// Run a git config command, logging a warning on failure rather than
/// propagating the error (git may not be installed in all sandboxes).
fn run_git_config(key: &str, value: &str) {
    match BlockingCommand::new("git")
        .args(["config", "--global", key, value])
        .output()
    {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            log::warn!(
                "git config --global {key} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Err(e) => {
            log::warn!("Failed to run git config --global {key}: {e}");
        }
    }
}

/// Like [`run_git_config`] but passes `--add` so the new value is appended to
/// any existing values for `key` rather than replacing them.
fn run_git_config_add(key: &str, value: &str) {
    match BlockingCommand::new("git")
        .args(["config", "--global", "--add", key, value])
        .output()
    {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            log::warn!(
                "git config --global --add {key} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Err(e) => {
            log::warn!("Failed to run git config --global --add {key}: {e}");
        }
    }
}

/// Run one-time git configuration that is set at startup and never needs to
/// be refreshed:
/// - `credential.helper cache` so credentials remain in-memory instead of
///   persisting to disk
/// - SSH→HTTPS URL rewrites for each credential host, covering both the
///   scp-style (`git@{host}:`) and explicit-protocol (`ssh://git@{host}/`)
///   URL forms, so operations on either form use HTTPS credentials instead
///   of looking for an SSH key.
pub(crate) fn setup_git_config(credentials: &[GitCredential]) {
    // 70 minutes keeps credentials warm past the normal refresh cadence (50m)
    // while still naturally expiring if refreshes stop.
    run_git_config("credential.helper", "cache --timeout=4200");
    // Use --add for both forms per host so all values coexist as a
    // multi-value key rather than each entry overwriting the previous one.
    for cred in credentials {
        let host = &cred.host;
        run_git_config_add(
            &format!("url.https://{host}/.insteadOf"),
            &format!("ssh://git@{host}/"),
        );
        run_git_config_add(
            &format!("url.https://{host}/.insteadOf"),
            &format!("git@{host}:"),
        );
    }
}

/// Configure the git user identity from the server-returned credential.
///
/// Uses the first credential's `username`/`email` fields, falling back to the
/// Oz defaults when either is absent (e.g. service-account principals).
pub(crate) fn configure_git_identity(credentials: &[GitCredential]) {
    let (name, email) = credentials
        .first()
        .map(|c| {
            (
                c.username.as_deref().unwrap_or(DEFAULT_GIT_NAME),
                c.email.as_deref().unwrap_or(DEFAULT_GIT_EMAIL),
            )
        })
        .unwrap_or((DEFAULT_GIT_NAME, DEFAULT_GIT_EMAIL));

    run_git_config("user.name", name);
    run_git_config("user.email", email);
}

/// Perform one git credentials refresh attempt.
///
/// Returns `Ok(())` on success (including when the server returns no
/// credentials). Returns `Err` when the workload-token issuance or the server
/// API call fails — these are transient failures worth retrying.
async fn try_refresh(task_id: &str, ai_client: &Arc<dyn AIClient>) -> Result<()> {
    let workload_token =
        warp_isolation_platform::issue_workload_token(Some(Duration::from_mins(5)))
            .await
            .context("Failed to issue workload token for git credentials refresh")?
            .token;

    let credentials = ai_client
        .get_task_git_credentials(task_id.to_string(), workload_token)
        .await
        .context("Failed to fetch git credentials from server")?;

    if credentials.is_empty() {
        log::debug!("No git credentials returned during refresh; skipping file write");
        return Ok(());
    }

    if let Err(e) = write_git_credentials(&credentials) {
        log::warn!("Failed to write refreshed git credentials: {e:#}");
    } else {
        log::info!("Git credentials refreshed successfully");
    }
    Ok(())
}

/// Infinite async loop that refreshes git credentials every
/// [`GIT_CREDENTIALS_REFRESH_INTERVAL`].
///
/// On each iteration:
/// 1. Issue a short-lived workload token.
/// 2. Call `taskGitCredentials` to get a fresh token from the server.
/// 3. Overwrite `~/.git-credentials` and `~/.config/gh/hosts.yaml`.
///
/// On transient failure, the refresh is retried up to three times with
/// exponential backoff (1 min, 2 min, 4 min), keeping all retries within the
/// ~10-minute buffer before the one-hour token expires. If all retries fail,
/// a warning is logged and the next refresh is scheduled after the normal
/// interval.
///
/// This future never resolves — it is designed to be raced with the harness
/// execution future via `futures::select!` and dropped when the harness
/// completes.
pub(crate) async fn refresh_loop(task_id: String, ai_client: Arc<dyn AIClient>) {
    loop {
        warpui::r#async::Timer::after(GIT_CREDENTIALS_REFRESH_INTERVAL).await;

        log::info!("Refreshing git credentials for task {task_id}");

        let backoff_delays = [
            Duration::from_secs(60),
            Duration::from_secs(2 * 60),
            Duration::from_secs(4 * 60),
        ];
        let mut attempt = 0usize;
        loop {
            match try_refresh(&task_id, &ai_client).await {
                Ok(()) => break,
                Err(e) if attempt < backoff_delays.len() => {
                    let delay = backoff_delays[attempt];
                    log::warn!(
                        "Git credentials refresh failed (attempt {}): {e:#}; retrying in {}s",
                        attempt + 1,
                        delay.as_secs()
                    );
                    warpui::r#async::Timer::after(delay).await;
                    attempt += 1;
                }
                Err(e) => {
                    log::warn!(
                        "Git credentials refresh failed after {} attempts: {e:#}; \
                         credentials may expire before next refresh cycle",
                        attempt + 1
                    );
                    break;
                }
            }
        }
    }
}
