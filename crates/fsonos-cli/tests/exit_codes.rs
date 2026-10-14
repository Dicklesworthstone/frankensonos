//! The real `fsonos` binary reports failures as `docs/ERRORS.md` specifies:
//! `error[CODE]: detail` plus a hint on stderr, and the code's exit status.
//! (`fsonos serve` on safe addresses is exercised end to end in
//! `tests/e2e_serve.rs`.)

use std::process::{Command, Output};

const SETTINGS: [&str; 7] = [
    "FSONOS_HTTP_ADDR",
    "FSONOS_MCP_HTTP_ADDR",
    "FSONOS_DATA_DIR",
    "FSONOS_SEEDS",
    "FSONOS_SPOTIFY_CLIENT_ID",
    "FSONOS_SPOTIFY_REDIRECT_URI",
    "FSONOS_TAILSCALE_SERVE",
];

fn fsonos(args: &[&str]) -> Output {
    fsonos_with(args, &[])
}

/// `fsonos args` with `env` set as well.
fn fsonos_with(args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_fsonos"));
    for var in SETTINGS {
        cmd.env_remove(var);
    }
    cmd.env(
        "FSONOS_DATA_DIR",
        std::env::temp_dir().join("fsonos-exit-codes"),
    )
    .env("RUST_LOG", "warn")
    .envs(env.iter().copied())
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

/// The README's `FSONOS_TAILSCALE_SERVE=1` reads as on rather than failing
/// as an invalid value: the bind guard is what answers next.
#[test]
fn tailscale_serve_reads_1_yes_and_true_from_the_environment() {
    for value in ["1", "yes", "true", "0"] {
        let out = fsonos_with(
            &["serve", "--http", "0.0.0.0:8099"],
            &[("FSONOS_TAILSCALE_SERVE", value)],
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("HTTP API would bind every interface"),
            "FSONOS_TAILSCALE_SERVE={value}: {stderr}"
        );
    }
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
