//! On macOS, embed an `Info.plist` in the `fsonos` binary.
//!
//! macOS 26 keeps Local Network access per code identity, and a launchd job
//! (or a LaunchAgent) whose binary has no approvable identity is refused the
//! LAN outright, with no prompt: `fsonos serve` under launchd could not reach
//! a single speaker (verified live, 2026-10-14). The embedded property list
//! gives the binary a bundle identifier and says why it needs the local
//! network, so macOS can ask once and remember the answer. Sign the binary
//! with a stable identity (docs/DEPLOY.md) so the answer survives rebuilds.

use std::env;
use std::fs;
use std::path::PathBuf;

const INFO_PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleIdentifier</key>
	<string>io.github.dicklesworthstone.fsonos</string>
	<key>CFBundleName</key>
	<string>fsonos</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>CFBundleShortVersionString</key>
	<string>VERSION</string>
	<key>CFBundleVersion</key>
	<string>VERSION</string>
	<key>NSLocalNetworkUsageDescription</key>
	<string>FrankenSonos finds and controls the Sonos speakers on your local network.</string>
	<key>NSBonjourServices</key>
	<array>
		<string>_sonos._tcp</string>
	</array>
</dict>
</plist>
"#;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let version = env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0".into());
    let out = PathBuf::from(env::var("OUT_DIR").expect("cargo sets OUT_DIR")).join("Info.plist");
    fs::write(&out, INFO_PLIST.replace("VERSION", &version)).expect("write Info.plist");
    println!(
        "cargo:rustc-link-arg-bin=fsonos=-Wl,-sectcreate,__TEXT,__info_plist,{}",
        out.display()
    );
}
