//! Doc-lint: the committed deployment docs must carry no real site data.
//!
//! `docs/DEPLOY.md`, the launchd plist template and `.env.example` are the most
//! likely place to accidentally paste a real IP, MAC or speaker serial while
//! writing an example. This guards the project's first rule — no site data in
//! the public repo (AGENTS.md / docs/SCOPE.md) — by rejecting any IPv4 literal
//! that is not a documentation placeholder, any MAC-shaped token, and any
//! Sonos `RINCON_` serial outside the synthetic scheme.
//!
//! It is deliberately dependency-free (no regex) and scoped to the three deploy
//! artifacts; the env-var/doc consistency drift check lives in
//! `fsonos_cli::config` tests.

const DEPLOY_MD: &str = include_str!("../../../docs/DEPLOY.md");
const PLIST: &str = include_str!("../../../docs/launchd/io.github.dicklesworthstone.fsonos.plist");
const ENV_EXAMPLE: &str = include_str!("../../../.env.example");

/// IPv4 literals that are legitimate in public documentation.
/// - `127.0.0.1` / `0.0.0.0`: loopback and the wildcard the bind guard names.
/// - TEST-NET-1/2/3 (RFC 5737): reserved for documentation/examples.
/// - `100.64.0.0`: base of the Tailscale/CGNAT range (RFC 6598) named by the bind guard.
/// - `100.101.102.103` / `.104`: the deploy guide's tailnet host placeholders.
const ALLOWED_IPV4: &[&str] = &[
    "127.0.0.1",
    "0.0.0.0",
    "100.64.0.0",
    "100.101.102.103",
    "100.101.102.104",
];
const ALLOWED_IPV4_PREFIXES: &[&str] = &["192.0.2.", "198.51.100.", "203.0.113."];

/// The only Sonos serial scheme allowed in the repo (see tests/fixtures).
const SYNTHETIC_RINCON_PREFIX: &str = "RINCON_000E58A0xxxx";

fn is_allowed_ipv4(ip: &str) -> bool {
    ALLOWED_IPV4.contains(&ip) || ALLOWED_IPV4_PREFIXES.iter().any(|p| ip.starts_with(p))
}

/// Find dotted-quad IPv4 literals: exactly four `.`-separated groups of 1–3
/// digits each in 0–255. Avoids matching version strings ("0.5.0", 3 groups)
/// and dates ("2026-07-28", dashes).
fn ipv4_literals(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        // Only start at a digit that is not preceded by a digit or dot.
        let prev_ok = i == 0 || !(bytes[i - 1].is_ascii_digit() || bytes[i - 1] == b'.');
        if prev_ok && bytes[i].is_ascii_digit() {
            let start = i;
            let mut groups = 0u8;
            let mut j = i;
            let mut ok = true;
            while groups < 4 {
                let gstart = j;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                let glen = j - gstart;
                if glen == 0 || glen > 3 {
                    ok = false;
                    break;
                }
                if text[gstart..j].parse::<u16>().unwrap_or(999) > 255 {
                    ok = false;
                    break;
                }
                groups += 1;
                if groups < 4 {
                    if j < bytes.len() && bytes[j] == b'.' {
                        j += 1;
                    } else {
                        ok = false;
                        break;
                    }
                }
            }
            // Reject if the next char continues the number (e.g. a 5th group).
            if ok
                && groups == 4
                && !(j < bytes.len() && (bytes[j] == b'.' || bytes[j].is_ascii_digit()))
            {
                out.push(text[start..j].to_string());
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
    out
}

/// Find MAC-shaped tokens: six `:`-separated pairs of hex digits.
fn mac_literals(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let is_hex = |b: u8| b.is_ascii_hexdigit();
    let mut i = 0;
    while i + 16 < bytes.len() + 1 && i < bytes.len() {
        let prev_ok = i == 0 || !(is_hex(bytes[i - 1]) || bytes[i - 1] == b':');
        // pattern: HH:HH:HH:HH:HH:HH  (17 chars)
        if prev_ok && i + 17 <= bytes.len() {
            let w = &bytes[i..i + 17];
            let shaped = (0..6).all(|g| {
                let off = g * 3;
                is_hex(w[off]) && is_hex(w[off + 1]) && (g == 5 || w[off + 2] == b':')
            });
            let ends_clean =
                i + 17 >= bytes.len() || !(is_hex(bytes[i + 17]) || bytes[i + 17] == b':');
            if shaped && ends_clean {
                out.push(String::from_utf8_lossy(w).into_owned());
                i += 17;
                continue;
            }
        }
        i += 1;
    }
    out
}

fn check(name: &str, text: &str) {
    for ip in ipv4_literals(text) {
        assert!(
            is_allowed_ipv4(&ip),
            "{name} contains a non-placeholder IPv4 literal `{ip}` — deploy docs must use \
             loopback, TEST-NET (192.0.2/198.51.100/203.0.113), or the documented tailnet \
             placeholders only (no real site data)",
        );
    }
    assert!(
        mac_literals(text).is_empty(),
        "{name} contains a MAC-shaped token {:?} — no real MAC addresses in the public repo",
        mac_literals(text),
    );
    for (idx, _) in text.match_indices("RINCON_") {
        let tail = &text[idx..];
        assert!(
            tail.starts_with(SYNTHETIC_RINCON_PREFIX),
            "{name} contains a RINCON_ serial outside the synthetic scheme ({SYNTHETIC_RINCON_PREFIX}…): \
             `{}`",
            &tail[..tail.len().min(28)],
        );
    }
}

#[test]
fn deploy_docs_carry_no_real_site_data() {
    check("docs/DEPLOY.md", DEPLOY_MD);
    check("docs/launchd/…fsonos.plist", PLIST);
    check(".env.example", ENV_EXAMPLE);
}

#[test]
fn ipv4_scanner_sanity() {
    // Matches real dotted quads only.
    assert_eq!(ipv4_literals("bind 127.0.0.1:8099 now"), vec!["127.0.0.1"]);
    assert_eq!(
        ipv4_literals("players = [\"192.0.2.10\"]"),
        vec!["192.0.2.10"]
    );
    // Not fooled by versions or dates.
    assert!(ipv4_literals("asupersync 0.5.0 on 2026-07-28").is_empty());
    // Catches a real private address.
    assert_eq!(ipv4_literals("192.168.1.50"), vec!["192.168.1.50"]);
    assert!(!is_allowed_ipv4("192.168.1.50"));
    assert!(is_allowed_ipv4("198.51.100.99"));
    // MAC detection.
    assert_eq!(mac_literals("x 00:0e:58:aa:bb:cc y").len(), 1);
    assert!(mac_literals("no macs 127.0.0.1 here").is_empty());
}
