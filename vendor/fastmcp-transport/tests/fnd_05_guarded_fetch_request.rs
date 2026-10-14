//! FND-05 A fetcher extension (bd-fnd-05-implementation-a-zq09): the guarded
//! POST request value, its encoder, its pre-contact refusals, and the
//! replace-only root set, proven from OUTSIDE the crate.
//!
//! External consumer of `fastmcp-transport`: every symbol under test is
//! imported from the crate root, so the `pub use` in lib.rs is load-bearing,
//! and nothing here depends on `cfg(test)` code inside the library (PL-3).
//! Everything is offline: no test opens a socket. The only network-shaped
//! step is DNS, and it goes through the shipped `with_resolver` seam into a
//! counting resolver whose answer the public-address fence refuses.
//!
//! WHAT IS NOT CLAIMED HERE: that the bytes reach the wire unchanged, that
//! the root set governs a real TLS handshake, or that a redirect answer to
//! POST is not followed. Those are the lower-class `cfg(test)` loopback
//! proofs in `src/http.rs` (`tests::fnd_05_guarded_post_*`,
//! `tests::fnd_05_guarded_root_set_wire_*`), recorded as that class and
//! never cited for the items proven here.

#![forbid(unsafe_code)]

use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use asupersync::Cx;
use asupersync::tls::Certificate;
use fastmcp_transport::{
    GuardedHttpFetchError, GuardedHttpFetchPolicy, GuardedHttpFetcher, GuardedHttpRequest,
    GuardedHttpResolver, GuardedHttpsUrl, GuardedRootSet, MAX_GUARDED_REQUEST_BODY_BYTES,
    guarded_encode_post_request,
};

/// `Rustls Robust Root`, a self-signed CA (copied from the transport's
/// loopback fixture; RCH never transfers `.pem` files, so fixtures are inline).
const LOOPBACK_ROOT_PEM: &[u8] = br"-----BEGIN CERTIFICATE-----
MIIBgDCCASegAwIBAgIUPHDUu9WL36yvTmFeNFZVe/qhClcwCgYIKoZIzj0EAwIw
HTEbMBkGA1UEAwwSUnVzdGxzIFJvYnVzdCBSb290MCAXDTc1MDEwMTAwMDAwMFoY
DzQwOTYwMTAxMDAwMDAwWjAdMRswGQYDVQQDDBJSdXN0bHMgUm9idXN0IFJvb3Qw
WTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAASW/VkDFs5iGDQvH8jaXYT4jMx66jo+
5CWKyMt4OlTDdBfKfnmQ9LYeK/PsYfJ8wVizuSlPzXi9je8SnyYejGP3o0MwQTAP
BgNVHQ8BAf8EBQMDB4QAMB0GA1UdDgQWBBRqY/oMENJbNo7y39iL6GW3tDs0rzAP
BgNVHRMBAf8EBTADAQH/MAoGCCqGSM49BAMCA0cAMEQCIEUbrmSUjANju9nNpFop
PAl9Wh8tBxI5IY+BPh466+aUAiA1/9+prypt6s3Doo0GDsnoFGJi1UBivUg1qdik
cy4eNw==
-----END CERTIFICATE-----";

/// `Rustls Robust Root - Rung 2`, a CA issued by the root above.
const LOOPBACK_RUNG2_PEM: &[u8] = br"-----BEGIN CERTIFICATE-----
MIIBiTCCATCgAwIBAgIUHWiVYIvMMWoZEFYvSz46COf2FqowCgYIKoZIzj0EAwIw
HTEbMBkGA1UEAwwSUnVzdGxzIFJvYnVzdCBSb290MCAXDTc1MDEwMTAwMDAwMFoY
DzQwOTYwMTAxMDAwMDAwWjAmMSQwIgYDVQQDDBtSdXN0bHMgUm9idXN0IFJvb3Qg
LSBSdW5nIDIwWTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAATAOCcBD7dXjmAZ3te5
D47cCJ9ec93PWv7BKYIL826CJsKfXQOGrBTthLm77hXLhHu6uv8E5QXNLZpfowLQ
Do1ao0MwQTAPBgNVHQ8BAf8EBQMDB4QAMB0GA1UdDgQWBBRdza76r11Ok9vRmlg6
Nn/wL/N+jTAPBgNVHRMBAf8EBTADAQH/MAoGCCqGSM49BAMCA0cAMEQCIFmZrXeK
hnfkahocvkhhNT3cDv1LWf6WBoFaCiBwZXFPAiARaKRiSCMG7PCHmSqFe82TBVmL
odHGogAVax1Dh/aYAA==
-----END CERTIFICATE-----";

/// `FastMCP OAuth TEST ONLY Root`, an unrelated self-signed CA (copied from
/// the fastmcp-client OAuth fixture).
const OUTSIDE_ROOT_PEM: &[u8] = br"-----BEGIN CERTIFICATE-----
MIIBgzCCASmgAwIBAgICA+kwCgYIKoZIzj0EAwIwJzElMCMGA1UEAwwcRmFzdE1D
UCBPQXV0aCBURVNUIE9OTFkgUm9vdDAeFw0yMDAxMDEwMDAwMDBaFw00OTEyMzEw
MDAwMDBaMCcxJTAjBgNVBAMMHEZhc3RNQ1AgT0F1dGggVEVTVCBPTkxZIFJvb3Qw
WTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAAS5t2O8JZ0hNjgI38E9Ov6i6mKoDRGo
ApMsykFkvgb6Zm9/5gCZ90eIKw7aWgK6iNs7lbtVY9mysZBIqm6pKQO2o0UwQzAS
BgNVHRMBAf8ECDAGAQH/AgEAMA4GA1UdDwEB/wQEAwIBhjAdBgNVHQ4EFgQU6QNI
rmvMiLoV3jIoCyohXARwI8gwCgYIKoZIzj0EAwIDSAAwRQIgCKOrW3vhzUJ2EyuY
vQUTdqGFhy0zEHj4ITFLvXPz1X8CIQCLKD4EKCvS/zkBSu/6uee1WV9d97UpK3yW
X/aCEJ5+hA==
-----END CERTIFICATE-----";

/// A planted credential. X8: it must never appear in any `Debug` output.
const PLANTED_SECRET: &str = "zq09-planted-secret-7f3a";

const CALLER_REVISION: &str = "fnd05-zq09";

fn certificate(pem: &[u8]) -> Certificate {
    let mut certificates = Certificate::from_pem(pem).expect("inline fixture PEM parses");
    assert_eq!(certificates.len(), 1, "each fixture holds one certificate");
    certificates.remove(0)
}

fn policy() -> GuardedHttpFetchPolicy {
    GuardedHttpFetchPolicy::new(
        64 * 1024,
        Duration::from_secs(5),
        Duration::from_secs(2),
        CALLER_REVISION,
    )
    .expect("a finite guarded policy is admitted")
}

fn token_url() -> GuardedHttpsUrl {
    GuardedHttpsUrl::parse("https://auth.example.test/oauth/token?tenant=a")
        .expect("a bounded https URL is admitted")
}

/// Returns one fixed answer and counts how often it was consulted.
struct CountingResolver {
    answer: IpAddr,
    calls: Arc<AtomicUsize>,
}

impl GuardedHttpResolver for CountingResolver {
    fn resolve_all(
        &self,
        _cx: Cx,
        _host: String,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<IpAddr>, GuardedHttpFetchError>> + Send + 'static>>
    {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let answer = self.answer;
        Box::pin(async move { Ok(vec![answer]) })
    }
}

/// The non-public answer the fence refuses, so no test reaches a socket.
fn private_answer() -> IpAddr {
    "10.0.0.1".parse().expect("ipv4 literal")
}

/// A counting resolver and its call counter.
fn counting_resolver() -> (Arc<CountingResolver>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let resolver = Arc::new(CountingResolver {
        answer: private_answer(),
        calls: Arc::clone(&calls),
    });
    (resolver, calls)
}

/// A fetcher built through ho7of's already-verified `with_resolver` seam.
///
/// X4's refusals that do not involve roots (body bound, CR/LF/NUL in a field
/// value) are proven through this seam, so the proof that nothing reaches DNS
/// does not depend on code this bead adds (WildMountain's ruling S3).
fn counted_fetcher() -> (GuardedHttpFetcher, Arc<AtomicUsize>) {
    let (resolver, calls) = counting_resolver();
    let fetcher =
        GuardedHttpFetcher::with_resolver(policy(), resolver).expect("verified resolver seam");
    (fetcher, calls)
}

fn post(
    fetcher: &GuardedHttpFetcher,
    request: &GuardedHttpRequest,
) -> Result<fastmcp_transport::GuardedHttpFetchResponse, GuardedHttpFetchError> {
    fastmcp_core::block_on(async {
        let cx = Cx::current().expect("block_on installs an ambient context");
        fetcher.post(&cx, &token_url(), request).await
    })
}

/// The publicly observable state of a fetcher, compared field by field rather
/// than through `Debug` (a redacted `Debug` comparison would be vacuous).
fn fetcher_state(fetcher: &GuardedHttpFetcher) -> (GuardedHttpFetchPolicy, String) {
    (fetcher.policy().clone(), fetcher.root_identity().to_owned())
}

/// Splits encoded request bytes into header lines and the body.
fn split_request(bytes: &[u8]) -> (Vec<String>, Vec<u8>) {
    let end = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("encoded request has a header terminator");
    let head = std::str::from_utf8(&bytes[..end]).expect("header block is ASCII");
    (
        head.split("\r\n").map(str::to_owned).collect(),
        bytes[end + 4..].to_vec(),
    )
}

/// The identity fold, recomputed here from the rule documented on
/// `GuardedHttpFetcher::with_root_set`, with no library helper.
fn documented_identity(caller_revision: &str, ders: &[&[u8]]) -> String {
    let mut sorted: Vec<&[u8]> = ders.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut preimage = b"FND05ROOTSETv1\0".to_vec();
    preimage.extend_from_slice(
        &u32::try_from(caller_revision.len())
            .expect("short revision")
            .to_be_bytes(),
    );
    preimage.extend_from_slice(caller_revision.as_bytes());
    preimage.extend_from_slice(
        &u32::try_from(sorted.len())
            .expect("small set")
            .to_be_bytes(),
    );
    for der in sorted {
        preimage.extend_from_slice(&u32::try_from(der.len()).expect("small DER").to_be_bytes());
        preimage.extend_from_slice(der);
    }
    let digest = fastmcp_core::sha256_bounded(&preimage, preimage.len()).expect("exact bound");
    let mut identity = String::from("custom-roots.sha256.");
    for byte in digest.as_bytes() {
        identity.push_str(&format!("{byte:02x}"));
    }
    identity
}

/// X3 POSITIVE: exact bytes for a POST with and without authorization.
#[test]
fn fnd_05_guarded_request_encoding_positive() {
    let url = token_url();
    let plain = GuardedHttpRequest::new("application/x-www-form-urlencoded", b"grant=abc".to_vec())
        .expect("admissible request");
    let with_auth = plain
        .clone()
        .with_authorization("Basic dGVzdA==")
        .expect("admissible authorization value");

    let plain_bytes = guarded_encode_post_request(&url, &plain).expect("encodes");
    assert_eq!(
        plain_bytes,
        b"POST /oauth/token?tenant=a HTTP/1.1\r\n\
          Host: auth.example.test\r\n\
          Content-Type: application/x-www-form-urlencoded\r\n\
          Content-Length: 9\r\n\
          Accept-Encoding: identity\r\n\
          Connection: close\r\n\
          \r\n\
          grant=abc"
            .to_vec(),
        "exact request bytes without authorization"
    );

    let auth_bytes = guarded_encode_post_request(&url, &with_auth).expect("encodes");
    assert_eq!(
        auth_bytes,
        b"POST /oauth/token?tenant=a HTTP/1.1\r\n\
          Host: auth.example.test\r\n\
          Content-Type: application/x-www-form-urlencoded\r\n\
          Content-Length: 9\r\n\
          Authorization: Basic dGVzdA==\r\n\
          Accept-Encoding: identity\r\n\
          Connection: close\r\n\
          \r\n\
          grant=abc"
            .to_vec(),
        "exact request bytes with authorization, which appears only when supplied"
    );

    let non_default_port =
        GuardedHttpsUrl::parse("https://auth.example.test:8443/t").expect("explicit port URL");
    let (lines, _) =
        split_request(&guarded_encode_post_request(&non_default_port, &plain).expect("encodes"));
    assert_eq!(lines[0], "POST /t HTTP/1.1");
    assert_eq!(
        lines[1], "Host: auth.example.test:8443",
        "Host carries a non-default port"
    );
    for bytes in [&plain_bytes, &auth_bytes] {
        let (lines, _) = split_request(bytes);
        assert!(
            !lines
                .iter()
                .any(|line| line.to_ascii_lowercase().starts_with("transfer-encoding")),
            "no Transfer-Encoding is ever emitted: {lines:?}"
        );
    }

    // X8: the planted credential is absent from the request value's Debug.
    let secret = plain
        .clone()
        .with_authorization(format!("Bearer {PLANTED_SECRET}"))
        .expect("admissible bearer value");
    assert!(secret.has_authorization());
    let debug = format!("{secret:?}");
    assert!(
        !debug.contains(PLANTED_SECRET),
        "request Debug must not contain the credential: {debug}"
    );
}

/// X3 PLANTED NEGATIVE: the same request with a body exactly one byte longer.
/// Content-Length differs by exactly one and every other header byte is
/// identical, so Content-Length is computed, not constant.
#[test]
fn fnd_05_guarded_request_encoding_planted_negative() {
    let url = token_url();
    let control = GuardedHttpRequest::new("application/json", b"{\"k\":\"123\"}".to_vec())
        .expect("admissible control")
        .with_authorization("Basic dGVzdA==")
        .expect("admissible authorization value");
    let planted = GuardedHttpRequest::new("application/json", b"{\"k\":\"1234\"}".to_vec())
        .expect("admissible planted request")
        .with_authorization("Basic dGVzdA==")
        .expect("admissible authorization value");
    assert_eq!(planted.body().len(), control.body().len() + 1);

    let (control_lines, control_body) =
        split_request(&guarded_encode_post_request(&url, &control).expect("encodes"));
    let (planted_lines, planted_body) =
        split_request(&guarded_encode_post_request(&url, &planted).expect("encodes"));

    let content_length = |lines: &[String]| -> usize {
        let values: Vec<usize> = lines
            .iter()
            .filter_map(|line| line.strip_prefix("Content-Length: "))
            .map(|value| value.parse().expect("decimal Content-Length"))
            .collect();
        assert_eq!(values.len(), 1, "exactly one Content-Length: {lines:?}");
        values[0]
    };
    assert_eq!(content_length(&control_lines), control.body().len());
    assert_eq!(content_length(&planted_lines), planted.body().len());
    assert_eq!(
        content_length(&planted_lines),
        content_length(&control_lines) + 1,
        "Content-Length differs by exactly one"
    );

    assert_eq!(control_lines.len(), planted_lines.len());
    let differing: Vec<usize> = control_lines
        .iter()
        .zip(&planted_lines)
        .enumerate()
        .filter(|(_, (left, right))| left != right)
        .map(|(index, _)| index)
        .collect();
    assert_eq!(differing.len(), 1, "only one header line may differ");
    assert!(
        control_lines[differing[0]].starts_with("Content-Length: "),
        "the one differing header line is Content-Length"
    );
    assert_eq!(control_body, control.body());
    assert_eq!(planted_body, planted.body());
}

/// X4 POSITIVE: a body exactly AT the bound is admitted, reaches the resolver
/// exactly once, and is then refused by the existing public-address fence,
/// so validation passed and no wire was touched.
#[test]
fn fnd_05_guarded_request_refusals_positive() {
    let (fetcher, calls) = counted_fetcher();
    let before = fetcher_state(&fetcher);
    let at_bound = GuardedHttpRequest::new(
        "application/octet-stream",
        vec![b'a'; MAX_GUARDED_REQUEST_BODY_BYTES],
    )
    .expect("a body exactly at the bound is admitted");
    assert_eq!(at_bound.body().len(), MAX_GUARDED_REQUEST_BODY_BYTES);

    let outcome = post(&fetcher, &at_bound);
    assert_eq!(
        outcome,
        Err(GuardedHttpFetchError::DisallowedResolvedAddress(
            private_answer()
        )),
        "the at-bound request passes validation and stops at the address fence"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the resolver is reached exactly once"
    );
    assert_eq!(
        fetcher_state(&fetcher),
        before,
        "fetcher state is unchanged"
    );
}

/// X4 PLANTED NEGATIVE: the same request at bound + 1 is refused with the
/// typed body-bound error and 0 resolver calls. The other refusals follow,
/// each one variable away from an admissible value. Every refusal happens at
/// construction, so a refused value cannot exist and `post` cannot be called
/// with it; the counter on the same fetcher reads 0 throughout. The accepted
/// control at the end proves that counter is live.
#[test]
fn fnd_05_guarded_request_refusals_planted_negative() {
    let (fetcher, calls) = counted_fetcher();
    let before = fetcher_state(&fetcher);

    assert_eq!(
        GuardedHttpRequest::new(
            "application/octet-stream",
            vec![b'a'; MAX_GUARDED_REQUEST_BODY_BYTES + 1],
        ),
        Err(GuardedHttpFetchError::InvalidRequest("request body bound")),
        "bound + 1 is refused with the typed body-bound error"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(fetcher_state(&fetcher), before);

    for forbidden in ['\r', '\n', '\0'] {
        let content_type = format!("application/json{forbidden}x");
        assert_eq!(
            GuardedHttpRequest::new(content_type, b"{}".to_vec()),
            Err(GuardedHttpFetchError::InvalidRequest("content type")),
            "content type with {forbidden:?} is refused"
        );

        let admissible = GuardedHttpRequest::new("application/json", b"{}".to_vec())
            .expect("admissible base request");
        let refused = admissible.with_authorization(format!("Bearer {PLANTED_SECRET}{forbidden}x"));
        assert_eq!(
            refused,
            Err(GuardedHttpFetchError::InvalidRequest("authorization value")),
            "authorization with {forbidden:?} is refused"
        );
        // X8: the refusal does not echo the rejected value.
        let debug = format!("{refused:?}");
        let display = refused.expect_err("refused").to_string();
        assert!(
            !debug.contains(PLANTED_SECRET) && !display.contains(PLANTED_SECRET),
            "a refusal must not echo the rejected value: {debug} / {display}"
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(fetcher_state(&fetcher), before);

    // ROOT-SET refusals, the only arms that involve roots, and so the only
    // place `with_root_set_and_resolver` is used (ruling S3). Both are refused
    // by `GuardedRootSet::new`, so no root-set fetcher, and therefore no
    // resolver, can be reached with them. The unparseable-DER arm is the
    // planted negative for the pre-admission check (reviewer S2): without that
    // check the certificate would be dropped silently by the connector while
    // still being folded into the reported identity.
    let (root_resolver, root_calls) = counting_resolver();
    assert!(
        matches!(
            GuardedRootSet::new(Vec::new()),
            Err(GuardedHttpFetchError::InvalidPolicy("empty root set"))
        ),
        "an empty root set is refused at construction"
    );
    assert!(
        matches!(
            GuardedRootSet::new([Certificate::from_der(b"not a certificate".to_vec())]),
            Err(GuardedHttpFetchError::InvalidPolicy("root certificate"))
        ),
        "a certificate the root store rejects is refused, not silently dropped"
    );
    assert!(
        matches!(
            GuardedRootSet::new([
                certificate(LOOPBACK_ROOT_PEM),
                Certificate::from_der(b"not a certificate".to_vec()),
            ]),
            Err(GuardedHttpFetchError::InvalidPolicy("root certificate"))
        ),
        "one rejected certificate refuses the whole set"
    );
    assert_eq!(root_calls.load(Ordering::SeqCst), 0);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(fetcher_state(&fetcher), before);

    // Live control for the root-set path: the valid set, over the same
    // prepared resolver, reaches it exactly once and stops at the fence.
    let roots = GuardedRootSet::new([certificate(LOOPBACK_ROOT_PEM)]).expect("one-CA root set");
    let root_fetcher =
        GuardedHttpFetcher::with_root_set_and_resolver(policy(), &roots, root_resolver)
            .expect("root-set fetcher over a caller resolver");
    let root_request =
        GuardedHttpRequest::new("application/json", b"{}".to_vec()).expect("admissible request");
    assert_eq!(
        post(&root_fetcher, &root_request),
        Err(GuardedHttpFetchError::DisallowedResolvedAddress(
            private_answer()
        ))
    );
    assert_eq!(
        root_calls.load(Ordering::SeqCst),
        1,
        "the root-set counter is live, so its zero above is a measurement"
    );

    // X8: the fetcher's Debug holds no credential either.
    assert!(!format!("{fetcher:?}").contains(PLANTED_SECRET));

    // Live-counter control: the at-bound request on the same fetcher counts.
    let at_bound = GuardedHttpRequest::new(
        "application/octet-stream",
        vec![b'a'; MAX_GUARDED_REQUEST_BODY_BYTES],
    )
    .expect("at-bound control");
    assert_eq!(
        post(&fetcher, &at_bound),
        Err(GuardedHttpFetchError::DisallowedResolvedAddress(
            private_answer()
        ))
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the counter is live, so its earlier zeros are measurements"
    );
}

/// X5 POSITIVE: the root identity is folded over certificate content. Two
/// fetchers from an identical policy whose root sets differ in exactly one
/// certificate report different identities; a custom-root identity differs
/// from the WebPKI identity for the same policy; the identity is
/// deterministic and equals the documented fold recomputed here.
#[test]
fn fnd_05_guarded_root_set_positive() {
    let root = certificate(LOOPBACK_ROOT_PEM);
    let rung2 = certificate(LOOPBACK_RUNG2_PEM);
    let outside = certificate(OUTSIDE_ROOT_PEM);

    let left_set = GuardedRootSet::new([root.clone(), rung2.clone()]).expect("two-CA set");
    let right_set = GuardedRootSet::new([root.clone(), outside]).expect("two-CA set");
    assert_eq!(left_set.certificate_count(), 2);
    assert_eq!(right_set.certificate_count(), 2);

    let left = GuardedHttpFetcher::with_root_set(policy(), &left_set).expect("left fetcher");
    let right = GuardedHttpFetcher::with_root_set(policy(), &right_set).expect("right fetcher");
    let webpki = GuardedHttpFetcher::new(policy()).expect("WebPKI fetcher");

    assert_ne!(
        left.root_identity(),
        right.root_identity(),
        "one differing certificate must change the identity"
    );
    assert_eq!(webpki.root_identity(), CALLER_REVISION);
    assert_ne!(
        left.root_identity(),
        webpki.root_identity(),
        "a custom-root fetcher must not report the WebPKI identity"
    );
    assert_ne!(right.root_identity(), webpki.root_identity());

    let again = GuardedHttpFetcher::with_root_set(policy(), &left_set).expect("rebuilt fetcher");
    assert_eq!(left.root_identity(), again.root_identity(), "deterministic");
    assert_eq!(
        left.root_identity(),
        documented_identity(CALLER_REVISION, &[root.as_der(), rung2.as_der()]),
        "the identity is the documented fold over DER content"
    );

    // The identity is what the stored policy, and so every fetch's
    // provenance, reports; the finite limits are the caller's, unchanged.
    let caller = policy();
    for fetcher in [&left, &right] {
        assert_eq!(
            fetcher.policy().root_policy_revision(),
            fetcher.root_identity()
        );
        assert_eq!(
            fetcher.policy().response_body_bytes(),
            caller.response_body_bytes()
        );
        assert_eq!(fetcher.policy().deadline(), caller.deadline());
        assert_eq!(
            fetcher.policy().tls_handshake_timeout(),
            caller.tls_handshake_timeout()
        );
    }
}

/// X5 PLANTED NEGATIVE: the same two sets built with identical DER content,
/// from separately parsed certificates, in a different supply order and with
/// a duplicate, report EQUAL identities. The fold is over content, not over
/// construction order or object identity.
#[test]
fn fnd_05_guarded_root_set_planted_negative() {
    let first = GuardedRootSet::new([
        certificate(LOOPBACK_ROOT_PEM),
        certificate(LOOPBACK_RUNG2_PEM),
    ])
    .expect("first set");
    let second = GuardedRootSet::new([
        certificate(LOOPBACK_RUNG2_PEM),
        certificate(LOOPBACK_ROOT_PEM),
        certificate(LOOPBACK_RUNG2_PEM),
    ])
    .expect("second set, reordered with a duplicate");
    assert_eq!(first.certificate_count(), 2);
    assert_eq!(
        second.certificate_count(),
        2,
        "the duplicate is removed, not counted"
    );

    let left = GuardedHttpFetcher::with_root_set(policy(), &first).expect("left fetcher");
    let right = GuardedHttpFetcher::with_root_set(policy(), &second).expect("right fetcher");
    assert_eq!(
        left.root_identity(),
        right.root_identity(),
        "identical DER content must report an equal identity"
    );
    assert_eq!(left.policy(), right.policy());
}
