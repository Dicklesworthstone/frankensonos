//! The real `fsonos` binary reports failures as `docs/ERRORS.md` specifies:
//! `error[CODE]: detail` plus a hint on stderr, and the code's exit status.

use std::process::{Command, Output};

const SETTINGS: [&str; 6] = [
    "FSONOS_HTTP_ADDR",
    "FSONOS_MCP_HTTP_ADDR",
    "FSONOS_DATA_DIR",
    "FSONOS_SEEDS",
    "FSONOS_SPOTIFY_CLIENT_ID",
    "FSONOS_SPOTIFY_REDIRECT_URI",
];

fn fsonos(args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_fsonos"));
    for var in SETTINGS {
        cmd.env_remove(var);
    }
    cmd.env(
        "FSONOS_DATA_DIR",
        std::env::temp_dir().join("fsonos-exit-codes"),
    )
    .env("RUST_LOG", "warn")
    .args(args)
    .output()
    .expect("run fsonos")
}

#[test]
fn unsafe_bind_is_an_invalid_argument() {
    let out = fsonos(&["serve", "--http", "0.0.0.0:8099"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "stderr: {stderr}");
    assert!(
        stderr.contains("error[INVALID_ARGUMENT]: HTTP API would bind every interface"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("hint:") && stderr.contains("--allow-unsafe-bind"),
        "stderr: {stderr}"
    );
}

#[test]
fn public_mcp_bind_is_refused_too() {
    let out = fsonos(&["serve", "--mcp-http", "[2001:db8::1]:8098"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "stderr: {stderr}");
    assert!(
        stderr.contains("MCP server would bind a public address"),
        "stderr: {stderr}"
    );
}

#[test]
fn usage_errors_exit_2() {
    let out = fsonos(&["play"]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn loopback_defaults_pass_the_guard() {
    let out = fsonos(&["serve"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("api 127.0.0.1:8099, mcp 127.0.0.1:8098"),
        "stdout: {stdout}"
    );
}
