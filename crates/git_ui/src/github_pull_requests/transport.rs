use anyhow::{Context as _, Result, bail, ensure};
use serde_json::Value;
use std::{
    fs::{self, File, OpenOptions},
    io::Read,
    path::PathBuf,
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

use super::HOST;

const MAX_RESPONSE: u64 = 32 * 1024 * 1024;

/// Only bounded HTTP status and allowlisted schema identifiers survive an error.
/// Server messages, bodies, request content and tokens never enter diagnostics.
#[derive(Clone, Debug)]
pub(crate) struct GitHubHttpError {
    pub status: u16,
    validation: Vec<(String, String)>,
    category: GitHubFailureCategory,
    retry_after_seconds: Option<u64>,
    rate_reset_epoch: Option<u64>,
    exit_code: Option<i32>,
    graphql_codes: Vec<String>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GitHubFailureCategory {
    Authentication,
    Permission,
    RateLimit,
    Connectivity,
    Validation,
    Server,
    Unknown,
}
impl GitHubFailureCategory {
    fn guidance(self) -> &'static str {
        match self {
            Self::Authentication => {
                "Authentication failed; run gh auth status --hostname github.com and sign in again"
            }
            Self::Permission => {
                "GitHub denied access; check repository access and token permissions"
            }
            Self::RateLimit => "GitHub rate limit reached; wait before reconciling or retrying",
            Self::Connectivity => {
                "Could not connect to GitHub; check network connectivity before reconciling"
            }
            Self::Validation => {
                "GitHub rejected the request fields; refresh the PR and check the comment location"
            }
            Self::Server => {
                "GitHub is temporarily unavailable; reconcile delivery before retrying a write"
            }
            Self::Unknown => {
                "GitHub CLI failed; check gh auth status --hostname github.com and connectivity, then reconcile delivery"
            }
        }
    }
}
impl GitHubHttpError {
    pub(crate) fn status(status: u16) -> Self {
        Self {
            status,
            validation: Vec::new(),
            category: match status {
                401 => GitHubFailureCategory::Authentication,
                403 => GitHubFailureCategory::Permission,
                429 => GitHubFailureCategory::RateLimit,
                400 | 422 => GitHubFailureCategory::Validation,
                500..=599 => GitHubFailureCategory::Server,
                _ => GitHubFailureCategory::Unknown,
            },
            retry_after_seconds: None,
            rate_reset_epoch: None,
            exit_code: None,
            graphql_codes: Vec::new(),
        }
    }
    pub(crate) fn authoritative_rejection(&self) -> bool {
        matches!(self.status, 400 | 401 | 403 | 404 | 405 | 410 | 415 | 422)
    }
    fn from_included_response(response: &[u8]) -> Result<Self> {
        let (status, body) = included_http_response(response)?;
        let mut error = Self::from_response(status, body);
        // Retain only numeric retry metadata. Never retain arbitrary server headers.
        let header_length = response.len().saturating_sub(body.len());
        for line in String::from_utf8_lossy(&response[..header_length]).lines() {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim().parse::<u64>().ok();
            if name.eq_ignore_ascii_case("retry-after") {
                error.retry_after_seconds = value;
            } else if name.eq_ignore_ascii_case("x-ratelimit-reset") {
                error.rate_reset_epoch = value;
            } else if name.eq_ignore_ascii_case("x-ratelimit-remaining")
                && value == Some(0)
                && status == 403
            {
                error.category = GitHubFailureCategory::RateLimit;
            }
        }
        if error.retry_after_seconds.is_some() && matches!(status, 403 | 429) {
            error.category = GitHubFailureCategory::RateLimit;
        }
        Ok(error)
    }
    pub(super) fn from_response(status: u16, body: &[u8]) -> Self {
        let mut error = Self::status(status);
        if let Ok(value) = serde_json::from_slice::<Value>(body)
            && let Some(issues) = value.get("errors").and_then(Value::as_array)
        {
            for issue in issues.iter().take(16) {
                let field = issue.get("field").and_then(Value::as_str).unwrap_or("");
                let code = issue.get("code").and_then(Value::as_str).unwrap_or("");
                let field = field
                    .strip_prefix("pull_request_review_thread.")
                    .or_else(|| field.strip_prefix("pull_request_review_comment."))
                    .unwrap_or(field);
                if matches!(
                    field,
                    "body"
                        | "commit_id"
                        | "path"
                        | "line"
                        | "side"
                        | "start_line"
                        | "start_side"
                        | "subject_type"
                        | "review_id"
                        | "pull_request_review_id"
                        | "in_reply_to"
                        | "position"
                ) && matches!(
                    code,
                    "missing"
                        | "missing_field"
                        | "invalid"
                        | "already_exists"
                        | "unprocessable"
                        | "custom"
                        | "not_found"
                        | "unauthorized"
                        | "forbidden"
                ) {
                    let detail = (field.to_string(), code.to_string());
                    if !error.validation.contains(&detail) {
                        error.validation.push(detail);
                    }
                }
            }
        }
        if let Ok(value) = serde_json::from_slice::<Value>(body) {
            // Inspect known signatures without preserving the remote message.
            if matches!(status, 403 | 429)
                && value
                    .get("message")
                    .and_then(Value::as_str)
                    .is_some_and(|message| message.to_ascii_lowercase().contains("rate limit"))
            {
                error.category = GitHubFailureCategory::RateLimit;
            }
            if let Some(issues) = value.get("errors").and_then(Value::as_array) {
                for issue in issues {
                    let code = issue
                        .get("type")
                        .or_else(|| issue.pointer("/extensions/type"))
                        .and_then(Value::as_str);
                    if let Some(
                        code @ ("RATE_LIMITED"
                        | "FORBIDDEN"
                        | "UNAUTHORIZED"
                        | "NOT_FOUND"
                        | "UNPROCESSABLE"
                        | "INTERNAL"
                        | "MAX_NODE_LIMIT_EXCEEDED"),
                    ) = code
                    {
                        if !error.graphql_codes.iter().any(|existing| existing == code) {
                            error.graphql_codes.push(code.into());
                        }
                    }
                    match code {
                        Some("RATE_LIMITED") => error.category = GitHubFailureCategory::RateLimit,
                        Some("FORBIDDEN") if error.category != GitHubFailureCategory::RateLimit => {
                            error.category = GitHubFailureCategory::Permission
                        }
                        Some("UNAUTHORIZED") => {
                            error.category = GitHubFailureCategory::Authentication
                        }
                        _ => {}
                    }
                }
            }
        }
        error
    }
}
impl std::fmt::Display for GitHubHttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.status == 0 {
            write!(f, "GitHub CLI request failed")?;
            if let Some(code) = self.exit_code {
                write!(f, " (exit {code})")?;
            }
        } else if self.status == 200 {
            write!(
                f,
                "GitHub returned GraphQL errors; delivery must be reconciled"
            )?;
        } else if self.authoritative_rejection() {
            write!(f, "GitHub rejected the request (HTTP {})", self.status)?;
        } else {
            write!(f, "GitHub returned HTTP {}", self.status)?;
        }
        if self.status == 200 && self.category == GitHubFailureCategory::Unknown {
            write!(
                f,
                "; GitHub could not complete this query; refresh or reconcile before retrying"
            )?;
        } else {
            write!(f, "; {}", self.category.guidance())?;
        }
        for code in &self.graphql_codes {
            write!(f, "; GraphQL {code}")?;
        }
        if let Some(seconds) = self.retry_after_seconds {
            write!(f, "; retry after {seconds} seconds")?;
        }
        if self.category == GitHubFailureCategory::RateLimit {
            if let Some(epoch) = self.rate_reset_epoch {
                write!(f, "; rate limit resets at Unix time {epoch}")?;
            }
        }
        for (field, code) in &self.validation {
            write!(f, "; {field}: {code}")?;
        }
        Ok(())
    }
}
impl std::error::Error for GitHubHttpError {}

pub(super) fn included_http_response(bytes: &[u8]) -> Result<(u16, &[u8])> {
    let mut response = bytes;
    loop {
        ensure!(
            response.starts_with(b"HTTP/"),
            "Missing GitHub HTTP response status"
        );
        let crlf = response
            .windows(4)
            .position(|part| part == b"\r\n\r\n")
            .map(|index| (index, 4));
        let lf = response
            .windows(2)
            .position(|part| part == b"\n\n")
            .map(|index| (index, 2));
        let (boundary, separator) = match (crlf, lf) {
            (Some(crlf), Some(lf)) => {
                if crlf.0 <= lf.0 {
                    Some(crlf)
                } else {
                    Some(lf)
                }
            }
            (Some(value), None) | (None, Some(value)) => Some(value),
            (None, None) => None,
        }
        .context("Incomplete GitHub HTTP response headers")?;
        ensure!(boundary <= 65536, "GitHub HTTP headers exceeded safe size");
        let line = response
            .split(|byte| *byte == b'\n')
            .next()
            .context("Missing GitHub HTTP status")?;
        let status = std::str::from_utf8(line)
            .ok()
            .and_then(|line| line.split_whitespace().nth(1))
            .filter(|status| status.len() == 3)
            .and_then(|status| status.parse::<u16>().ok())
            .filter(|status| (100..=599).contains(status))
            .context("Invalid GitHub HTTP status")?;
        let body = &response[boundary + separator..];
        if body.starts_with(b"HTTP/") {
            response = body;
            continue;
        }
        return Ok((status, body));
    }
}

/// Regular temporary files avoid blocking pipe writes/reader joins. The same
/// deadline covers input preparation, child execution and response collection.
/// Private files are removed on every exit, including cancellation and errors.
struct RequestFiles {
    directory: PathBuf,
}
impl RequestFiles {
    fn new() -> Result<Self> {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        for _ in 0..100 {
            let directory = std::env::temp_dir().join(format!(
                "zed-github-request-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt as _;
                builder.mode(0o700);
            }
            match builder.create(&directory) {
                Ok(()) => return Ok(Self { directory }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(_) => bail!("Could not create private GitHub request storage"),
            }
        }
        bail!("Could not allocate private GitHub request storage")
    }
    fn create(&self, name: &str) -> Result<File> {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        options
            .open(self.directory.join(name))
            .context("Could not create private GitHub request file")
    }
}
impl Drop for RequestFiles {
    fn drop(&mut self) {
        if fs::remove_dir_all(&self.directory).is_err() {
            log::warn!("Could not remove private GitHub request storage");
        }
    }
}
struct RunningChild(std::process::Child);
impl Drop for RunningChild {
    fn drop(&mut self) {
        if let Err(error) = self.0.kill()
            && error.kind() != std::io::ErrorKind::InvalidInput
        {
            log::debug!("Could not stop GitHub request process");
        }
        if self.0.wait().is_err() {
            log::warn!("Could not reap GitHub request process");
        }
    }
}
pub(super) fn run_gh(
    args: &[&str],
    input: Option<&[u8]>,
    cancelled: &AtomicBool,
    deadline: Instant,
) -> Result<Vec<u8>> {
    let mut command = Command::new("gh");
    command
        .arg("api")
        .args([
            "--hostname",
            HOST,
            "--header",
            "Accept: application/vnd.github+json",
            "--header",
            "X-GitHub-Api-Version: 2022-11-28",
        ])
        .args(args)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env_remove("GH_DEBUG");
    // Always capture status/rate metadata, while preserving each caller's body-only contract.
    let caller_includes_headers = args.contains(&"--include");
    if !caller_includes_headers {
        command.arg("--include");
    }
    static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let request_number = REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed) + 1;
    let started = Instant::now();
    let result = run_command(command, input, cancelled, deadline, MAX_RESPONSE);
    let api_kind = if args.first() == Some(&"graphql") {
        "graphql"
    } else {
        "rest"
    };
    let method = args
        .windows(2)
        .find(|pair| pair[0] == "--method")
        .map(|pair| pair[1])
        .unwrap_or(if api_kind == "graphql" { "POST" } else { "GET" });
    let method = match method {
        "GET" | "POST" | "PATCH" | "PUT" | "DELETE" => method,
        _ => "OTHER",
    };
    let status = match &result {
        Ok(bytes) => included_http_response(bytes).ok().map(|(status, _)| status),
        Err(error) => error
            .downcast_ref::<GitHubHttpError>()
            .map(|error| error.status)
            .filter(|status| *status != 0),
    }
    .unwrap_or(0);
    // Opt-in metadata only. Never log endpoints, headers, error text, bodies or account identity.
    log::debug!(target: "git_ui::github_requests", "github_api_request sequence={} api={} method={} elapsed_ms={} outcome={} status={}", request_number, api_kind, method, started.elapsed().as_millis(), if result.is_ok() { "ok" } else { "error" }, status);
    let response = result?;
    if caller_includes_headers {
        Ok(response)
    } else {
        let (_, body) = included_http_response(&response)?;
        Ok(body.to_vec())
    }
}
#[expect(
    clippy::disallowed_methods,
    reason = "ReviewTransport is synchronous and runs on a background executor; std::process preserves private-file stdio and cancellation/deadline cleanup without smol conversion losing stdio"
)]
fn run_command(
    mut command: Command,
    input: Option<&[u8]>,
    cancelled: &AtomicBool,
    deadline: Instant,
    max_response: u64,
) -> Result<Vec<u8>> {
    let check = || -> Result<()> {
        ensure!(
            !cancelled.load(Ordering::Acquire),
            "GitHub request cancelled"
        );
        ensure!(
            Instant::now() < deadline,
            "GitHub request timed out; retry after checking connectivity"
        );
        Ok(())
    };
    check()?;
    let files = RequestFiles::new()?;
    let stdout = files.create("response")?;
    let stderr = files.create("errors")?;
    if let Some(input) = input {
        ensure!(
            input.len() as u64 <= MAX_RESPONSE,
            "GitHub request body exceeded safe size"
        );
        use std::io::{Seek as _, Write as _};
        let mut file = files.create("input")?;
        file.write_all(input)
            .context("Could not prepare GitHub request")?;
        file.rewind().context("Could not prepare GitHub request")?;
        command.stdin(Stdio::from(file));
    } else {
        command.stdin(Stdio::null());
    }
    command
        .stdout(Stdio::from(stdout.try_clone()?))
        .stderr(Stdio::from(stderr.try_clone()?));
    check()?;
    let mut child =
        RunningChild(command.spawn().context(
            "Could not start GitHub CLI; install gh and authenticate with gh auth login",
        )?);
    let status = loop {
        check()?;
        ensure!(
            stdout.metadata()?.len() <= max_response && stderr.metadata()?.len() <= max_response,
            "GitHub response exceeded the safe page size"
        );
        if let Some(status) = child
            .0
            .try_wait()
            .context("Could not check GitHub request")?
        {
            break status;
        }
        thread::sleep(Duration::from_millis(10));
    };
    check()?;
    let mut output = Vec::new();
    // Re-open: the child and cloned file descriptors share the original offset.
    File::open(files.directory.join("response"))?
        .take(max_response + 1)
        .read_to_end(&mut output)
        .context("Could not read GitHub response")?;
    check()?;
    ensure!(
        output.len() as u64 <= max_response,
        "GitHub response exceeded the safe page size"
    );
    if !status.success() {
        if let Ok((status, body)) = included_http_response(&output) {
            if (400..=599).contains(&status) {
                return Err(GitHubHttpError::from_included_response(&output)?.into());
            }
            // gh exits 1 for GraphQL errors even when HTTP status is 200.
            if serde_json::from_slice::<Value>(body)
                .ok()
                .is_some_and(|value| {
                    value
                        .get("errors")
                        .and_then(Value::as_array)
                        .is_some_and(|errors| !errors.is_empty())
                })
            {
                return Err(GitHubHttpError::from_included_response(&output)?.into());
            }
        }
        let mut diagnostic = Vec::new();
        File::open(files.directory.join("errors"))?
            .take(65536)
            .read_to_end(&mut diagnostic)?;
        let mut failure = cli_failure(&diagnostic);
        failure.exit_code = status.code();
        return Err(failure.into());
    }
    Ok(output)
}

// Match diagnostic signatures, never display stderr: it may contain tokens or private URLs.
fn cli_failure(stderr: &[u8]) -> GitHubHttpError {
    let diagnostic = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    let mut error = GitHubHttpError::status(0);
    error.category = if diagnostic.contains("rate limit") || diagnostic.contains("rate_limit") {
        GitHubFailureCategory::RateLimit
    } else if diagnostic.contains("gh auth login")
        || diagnostic.contains("bad credentials")
        || diagnostic.contains("not logged into")
        || diagnostic.contains("authentication failed")
    {
        GitHubFailureCategory::Authentication
    } else if diagnostic.contains("could not resolve")
        || diagnostic.contains("no such host")
        || diagnostic.contains("connection refused")
        || diagnostic.contains("connection reset")
        || diagnostic.contains("tls handshake")
        || diagnostic.contains("network is unreachable")
        || diagnostic.contains("i/o timeout")
    {
        GitHubFailureCategory::Connectivity
    } else if diagnostic.contains("resource not accessible")
        || diagnostic.contains("insufficient scope")
    {
        GitHubFailureCategory::Permission
    } else {
        GitHubFailureCategory::Unknown
    };
    error
}

#[cfg(test)]
mod tests;
