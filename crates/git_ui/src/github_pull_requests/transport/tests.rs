use super::*;
use std::{
    io::{Seek as _, Write as _},
    sync::Arc,
};

fn fixture_command(mode: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().expect("test executable"));
    let module = module_path!()
        .split_once("::")
        .expect("crate module path")
        .1;
    command
        .args([
            "--exact",
            &format!("{module}::transport_process_fixture"),
            "--nocapture",
        ])
        .env("ZED_GITHUB_TRANSPORT_FIXTURE", mode);
    command
}

#[test]
fn transport_process_fixture() {
    let Ok(mode) = std::env::var("ZED_GITHUB_TRANSPORT_FIXTURE") else {
        return;
    };
    if mode == "sleep" {
        thread::sleep(Duration::from_secs(10));
        std::process::exit(0);
    }

    // The harness writes a prologue before running this helper. The runner gives
    // this child a private regular output file, so remove that prologue before
    // emitting the exact CLI response and exiting without a harness epilogue.
    #[cfg(unix)]
    let mut stdout = {
        use std::os::fd::AsFd as _;
        File::from(
            std::io::stdout()
                .as_fd()
                .try_clone_to_owned()
                .expect("stdout handle"),
        )
    };
    #[cfg(windows)]
    let mut stdout = {
        use std::os::windows::io::AsHandle as _;
        File::from(
            std::io::stdout()
                .as_handle()
                .try_clone_to_owned()
                .expect("stdout handle"),
        )
    };
    stdout.set_len(0).expect("truncate harness output");
    stdout.rewind().expect("rewind fixture output");
    let exit_code = match mode.as_str() {
        "http_error" => {
            stdout
                .write_all(
                    std::env::var("ZED_GITHUB_TRANSPORT_RESPONSE")
                        .expect("fixture response")
                        .as_bytes(),
                )
                .expect("write HTTP response");
            1
        }
        "oversized" => {
            stdout
                .write_all(&[b'x'; 8192])
                .expect("write bounded-output fixture");
            0
        }
        "stderr" => {
            std::io::stderr()
                .write_all(b"PRIVATE_SECRET\n")
                .expect("write private diagnostic");
            1
        }
        _ => panic!("unknown subprocess fixture mode"),
    };
    stdout.flush().expect("flush fixture output");
    std::process::exit(exit_code);
}

#[test]
fn sanitized_http_errors_distinguish_limits_permissions_and_retry_metadata() {
    let error = GitHubHttpError::from_included_response(b"HTTP/2.0 403 Forbidden\r\nRetry-After: 30\r\nX-RateLimit-Reset: 1792568986\r\nX-Private: secret_token\r\n\r\n{\"message\":\"private payload\"}").unwrap();
    assert_eq!(error.category, GitHubFailureCategory::RateLimit);
    assert_eq!(error.retry_after_seconds, Some(30));
    assert_eq!(error.rate_reset_epoch, Some(1792568986));
    let visible = format!("{error:?} {error}");
    assert!(visible.contains("retry after 30 seconds"));
    assert!(!visible.contains("secret_token"));
    assert!(!visible.contains("private payload"));
    assert_eq!(
        GitHubHttpError::from_response(403, b"{}").category,
        GitHubFailureCategory::Permission
    );
    assert_eq!(
        GitHubHttpError::from_response(401, b"{}").category,
        GitHubFailureCategory::Authentication
    );
    assert_eq!(
        GitHubHttpError::from_response(
            403,
            br#"{"message":"You have exceeded a secondary rate limit. PRIVATE"}"#
        )
        .category,
        GitHubFailureCategory::RateLimit
    );
    assert!(
        GitHubHttpError::from_included_response(
            b"HTTP/2.0 403 Forbidden\nRetry-After: TOKEN\n\n{}"
        )
        .unwrap()
        .retry_after_seconds
        .is_none()
    );
}
#[test]
fn runner_keeps_graphql_error_type_without_leaking_error_message() {
    let response = "HTTP/2.0 200 OK\nContent-Type: application/json\n\n{\"errors\":[{\"type\":\"RATE_LIMITED\",\"message\":\"PRIVATE_TOKEN\"}]}";
    let mut command = fixture_command("http_error");
    command.env("ZED_GITHUB_TRANSPORT_RESPONSE", response);
    let error = run_command(
        command,
        None,
        &AtomicBool::new(false),
        Instant::now() + Duration::from_secs(3),
        4096,
    )
    .unwrap_err();
    let typed = error.downcast_ref::<GitHubHttpError>().unwrap();
    assert_eq!(typed.category, GitHubFailureCategory::RateLimit);
    // HTTP200 GraphQL partial errors can follow an applied mutation.
    assert!(!typed.authoritative_rejection());
    assert!(!format!("{error:?}").contains("PRIVATE_TOKEN"));
}
#[test]
fn deleted_node_diagnostics_keep_not_found_code_without_private_identity() {
    let error = GitHubHttpError::from_response(
        200,
        br#"{"data":{"node":null},"errors":[{"type":"NOT_FOUND","path":["node"],"message":"Could not resolve PRIVATE_NODE_ID in PRIVATE_REPOSITORY"},{"type":"PRIVATE_ERROR_TYPE","message":"PRIVATE_TOKEN"}]}"#,
    );
    let visible = format!("{error:?} {error}");
    assert!(visible.contains("GraphQL NOT_FOUND"));
    assert!(!visible.contains("PRIVATE_"));
    assert!(
        !visible.contains("check gh auth"),
        "Deleted-node errors are not authentication evidence"
    );
    assert!(
        !error.authoritative_rejection(),
        "A GraphQL field error cannot prove mutation non-delivery"
    );
}
#[test]
fn cli_diagnostics_are_classified_without_copying_sensitive_stderr() {
    for (stderr, category) in [
        (
            "gh auth login secret_token",
            GitHubFailureCategory::Authentication,
        ),
        (
            "dial tcp: lookup private.host: no such host TOKEN",
            GitHubFailureCategory::Connectivity,
        ),
        (
            "Resource not accessible by integration PRIVATE",
            GitHubFailureCategory::Permission,
        ),
        (
            "rate limit exceeded PRIVATE",
            GitHubFailureCategory::RateLimit,
        ),
        ("unexpected PRIVATE", GitHubFailureCategory::Unknown),
    ] {
        let error = cli_failure(stderr.as_bytes());
        assert_eq!(error.category, category);
        assert!(!error.authoritative_rejection());
        assert!(!format!("{error:?} {error}").contains("PRIVATE"));
        assert!(!format!("{error:?} {error}").contains("secret_token"));
    }
}
#[test]
fn actual_runner_preserves_only_safe_http_rejection_identifiers() {
    let response=b"HTTP/2.0 422 Unprocessable Entity\r\nX-Private-Header: PRIVATE_HEADER\r\n\r\n{\"message\":\"PRIVATE_BODY\",\"errors\":[{\"field\":\"line\",\"code\":\"invalid\",\"message\":\"PRIVATE_MESSAGE\"},{\"field\":\"PRIVATE_FIELD\",\"code\":\"custom\"}]}";
    let mut command = fixture_command("http_error");
    command.env(
        "ZED_GITHUB_TRANSPORT_RESPONSE",
        std::str::from_utf8(response).unwrap(),
    );
    let error = run_command(
        command,
        None,
        &AtomicBool::new(false),
        Instant::now() + Duration::from_secs(3),
        4096,
    )
    .unwrap_err();
    let typed = error
        .downcast_ref::<GitHubHttpError>()
        .expect("HTTP rejection must remain typed");
    assert_eq!(typed.status, 422);
    assert!(typed.authoritative_rejection());
    let visible = format!("{error:?}");
    assert!(visible.contains("line: invalid"));
    assert!(!visible.contains("PRIVATE_"));
    for status in [408, 409, 429, 500, 502, 503] {
        assert!(!GitHubHttpError::status(status).authoritative_rejection());
    }
    for status in [400, 401, 403, 404, 405, 410, 415, 422] {
        assert!(GitHubHttpError::status(status).authoritative_rejection());
    }
}
#[test]
fn included_http_handles_empty_success_interim_headers_and_malformed_status() {
    let (status, body) =
        included_http_response(b"HTTP/2.0 204 No Content\nContent-Length: 0\n\n").unwrap();
    assert_eq!(status, 204);
    assert!(body.is_empty());
    let (status,body)=included_http_response(b"HTTP/1.1 100 Continue\r\n\r\nHTTP/2.0 201 Created\nContent-Type: application/json\n\n{\"id\":42}").unwrap();
    assert_eq!(status, 201);
    assert_eq!(serde_json::from_slice::<Value>(body).unwrap()["id"], 42);
    assert!(included_http_response(b"HTTP/2.0 422 missing header delimiter").is_err());
    assert!(included_http_response(b"{\"message\":\"HTTP/2.0 422\"}").is_err());
}

#[test]
fn actual_runner_cancels_and_times_out_even_when_child_does_not_read_input() {
    let command = fixture_command("sleep");
    let start = Instant::now();
    let cancelled = Arc::new(AtomicBool::new(false));
    let trigger = cancelled.clone();
    let cancel_thread = thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));
        trigger.store(true, Ordering::Release);
    });
    let result = run_command(
        command,
        Some(&vec![b'x'; 1024 * 1024]),
        &cancelled,
        start + Duration::from_secs(3),
        4096,
    );
    cancel_thread.join().unwrap();
    assert!(result.unwrap_err().to_string().contains("cancelled"));
    assert!(start.elapsed() < Duration::from_secs(2));
    let command = fixture_command("sleep");
    let start = Instant::now();
    assert!(
        run_command(
            command,
            Some(&vec![b'x'; 1024 * 1024]),
            &AtomicBool::new(false),
            start + Duration::from_millis(100),
            4096
        )
        .unwrap_err()
        .to_string()
        .contains("timed out")
    );
    assert!(start.elapsed() < Duration::from_secs(2));
}
#[test]
fn actual_runner_bounds_output_and_does_not_expose_private_errors() {
    let command = fixture_command("oversized");
    assert!(
        run_command(
            command,
            None,
            &AtomicBool::new(false),
            Instant::now() + Duration::from_secs(3),
            4096
        )
        .unwrap_err()
        .to_string()
        .contains("safe page size")
    );
    let command = fixture_command("stderr");
    let error = run_command(
        command,
        None,
        &AtomicBool::new(false),
        Instant::now() + Duration::from_secs(3),
        4096,
    )
    .unwrap_err()
    .to_string();
    assert!(!error.contains("PRIVATE_SECRET"));
    assert!(error.contains("exit 1"));
}
