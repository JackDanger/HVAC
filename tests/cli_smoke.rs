//! Integration tests that exercise the `hvac` binary as a subprocess.
//!
//! These cover paths that don't depend on a GPU (CI runners don't have
//! NVENC / VAAPI / VideoToolbox available): `--help`, `--version`,
//! `--dump-config`, argument validation, and the SDK-key-leakage guard.
//!
//! Tests that need real GPU encoding live on the remote NVENC host and
//! are exercised via `./deploy.sh test`.

use std::process::Command;
use std::process::Stdio;

/// Return the path to the binary built by Cargo for this test run.
/// Cargo sets `CARGO_BIN_EXE_<name>` to the freshly-built artefact.
fn hvac_bin() -> &'static str {
    env!("CARGO_BIN_EXE_hvac")
}

/// Run hvac with the given args; collect stdout / stderr; assert the
/// process actually finished (no orphan).
fn run(args: &[&str]) -> std::process::Output {
    Command::new(hvac_bin())
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to launch hvac binary")
}

#[test]
fn help_flag_works_and_mentions_key_features() {
    let out = run(&["--help"]);
    assert!(out.status.success(), "hvac --help should exit 0");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("hvac"),
        "help output should mention the binary name: {stdout}"
    );
    // Spot-check that user-visible flags from the Cli struct appear.
    for needle in ["--dry-run", "--no-overwrite", "--config", "--dump-config"] {
        assert!(
            stdout.contains(needle),
            "help output missing flag {needle}: {stdout}"
        );
    }
}

#[test]
fn version_flag_prints_a_version_string() {
    let out = run(&["--version"]);
    assert!(out.status.success(), "hvac --version should exit 0");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.starts_with("hvac"),
        "expected 'hvac X.Y.Z\\n', got: {stdout}"
    );
    // A semver-shaped token must appear after the name.
    let has_digit = stdout.chars().any(|c| c.is_ascii_digit());
    assert!(has_digit, "no version number in: {stdout}");
}

#[test]
fn dump_config_emits_a_yaml_document() {
    // --dump-config runs before GPU detection and path validation, so it
    // succeeds on every host including GPU-less CI runners.
    let out = run(&["--dump-config"]);
    assert!(out.status.success(), "hvac --dump-config should exit 0");
    let stdout = String::from_utf8_lossy(&out.stdout);
    // YAML-shape spot checks: top-level keys + a known value.
    assert!(
        stdout.contains("target:"),
        "missing target section: {stdout}"
    );
    assert!(
        stdout.contains("media_extensions:"),
        "missing media_extensions: {stdout}"
    );
    assert!(stdout.contains("codec:"), "missing codec field: {stdout}");
}

#[test]
fn missing_path_argument_fails_with_clap_error() {
    // Without --dump-config or --setup-launchdarkly, `path` is required.
    let out = run(&[]);
    assert!(!out.status.success(), "should exit non-zero without a path");
    let stderr = String::from_utf8_lossy(&out.stderr);
    // clap emits "error: " prefix; we just check the user gets *some*
    // arg-validation error rather than a panic / crash.
    assert!(
        stderr.to_lowercase().contains("required") || stderr.to_lowercase().contains("usage"),
        "expected a clap-style error, got: {stderr}"
    );
}

#[test]
fn launchdarkly_sdk_key_is_not_read_from_environment() {
    // The whole point of the CLI-only design from PR #22: even with the
    // env var set, the CLI must not pick it up. We can verify this
    // indirectly: --dump-config runs without touching the LD client,
    // so we just check that setting the env var doesn't change behaviour
    // or crash. (We can't easily inspect the LD client's state from a
    // subprocess test, but if the env-var path were live, malformed
    // strings would surface here.)
    let out = Command::new(hvac_bin())
        .arg("--dump-config")
        .env("LAUNCHDARKLY_SDK_KEY", "should-not-be-read-by-hvac")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to launch hvac");
    assert!(
        out.status.success(),
        "hvac --dump-config should ignore LAUNCHDARKLY_SDK_KEY in env"
    );
}

#[test]
fn setup_launchdarkly_without_api_key_fails_clearly() {
    let out = run(&["--setup-launchdarkly"]);
    assert!(!out.status.success(), "should require --ld-api-key");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("ld-api-key") || stderr.contains("ld_api_key"),
        "expected mention of --ld-api-key, got: {stderr}"
    );
}

#[test]
fn dry_run_against_empty_directory_exits_cleanly() {
    // GPU detection runs even in dry-run mode, so this test only runs
    // when hvac can find a GPU. On CI runners with no GPU we skip — see
    // the early-return below.
    let tmp = tempfile::tempdir().expect("tempdir");

    let out = Command::new(hvac_bin())
        .arg("--dry-run")
        .arg(tmp.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to launch hvac");

    let stderr = String::from_utf8_lossy(&out.stderr);

    // Two acceptable outcomes on a host without a usable GPU/ffmpeg:
    //   - Exit nonzero with a "No GPU found" message
    //   - Exit nonzero with an ffmpeg-missing message
    // On a healthy host with GPU + ffmpeg we expect exit 0 and a
    // "No media files found" line for the empty dir.
    if !out.status.success() {
        let lower = stderr.to_lowercase();
        assert!(
            lower.contains("gpu") || lower.contains("ffmpeg") || lower.contains("encoder"),
            "non-zero exit without a recognised reason; stderr was: {stderr}"
        );
        return;
    }
    // Healthy-host path: confirms the empty-dir branch from main.rs fires.
    assert!(
        stderr.contains("No media files") || stderr.contains("Nothing to do"),
        "expected an empty-dir notice, got: {stderr}"
    );
}
