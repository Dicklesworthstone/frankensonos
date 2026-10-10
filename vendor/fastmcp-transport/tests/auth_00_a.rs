//! AUTH-00 A: verified-principal ingress and security-partition types.
//!
//! An **external** consumer of the packaged `fastmcp-transport` and
//! `fastmcp-core` crates: it reaches the AUTH-00 A surface the way a
//! downstream crate does, via `use fastmcp_transport::ingress::...` and
//! `use fastmcp_core::ingress::...`, never through `use super` or a
//! `pub(crate)` path, and nothing here is compiled under `cfg(test)` inside
//! either library. `cfg(test)` behaviour cannot prove shipped behaviour (PL-3).
//!
//! # Why this target lives in `fastmcp-transport` and not `fastmcp-core`
//!
//! The first acceptance row requires driving the public `IngressAuthenticator`
//! through a borrowed `AuthRequestView` and entering ingress only through the
//! opaque `AuthenticatedTransportIngress`. Those three types live in
//! `fastmcp-transport`, while the facts and descriptor live in `fastmcp-core`.
//! `fastmcp-core` has no dependency on `fastmcp-transport` — not even a
//! dev-dependency, and it could not have one without a cycle — so a test under
//! `crates/fastmcp-core/tests/` cannot name the trait at all. `fastmcp-transport`
//! depends on `fastmcp-core`, so a target here sees both halves of the seam.
//!
//! # Why this file implements `IngressAuthenticator`
//!
//! It must, and that is not a fixture standing in for live proof. AUTH-00 A
//! ships a *seam*: bounded fact types, a borrowed request view, the trait, and
//! an opaque ingress value only the framework can mint. AUTH-01 supplies
//! concrete providers. What is under test is the seam itself — that a provider
//! cannot return a credential, cannot retain the request, and cannot mint the
//! opaque value — and exercising it requires a provider on the other side.
//! Every production path invoked below is the shipped one.
//!
//! # No-claim boundary
//!
//! This leaf proves verified-principal ingress and partition-type
//! construction. It does not prove partition admission or lookup (AUTH-00 B),
//! the AUTH-00 aggregate, or aggregate MCP capability. AUTH-00 B's admission
//! controller appears here only as an *observable* for unchanged-state proof;
//! no claim is made about its behaviour.

#![forbid(unsafe_code)]

use std::time::Duration;

use asupersync::Cx;
use asupersync::runtime::reactor::create_reactor;
use asupersync::runtime::{Runtime, RuntimeBuilder};

use fastmcp_core::crypto::{HMAC_SHA256_KEY_BYTES, HMAC_SHA256_TAG_BYTES, HmacSha256Key};
use fastmcp_core::ingress::{
    AuthorizationRotationFacts, DEFAULT_MAXIMUM_STALENESS, HARD_MAXIMUM_STALENESS,
    IngressFactsError, MaximumStaleness, ReplayPurpose, RevalidationDispatch,
    SECRET_FINGERPRINT_KEY_ID_MAX_BYTES, SECRET_FINGERPRINT_KEY_ID_MIN_BYTES,
    SECRET_FINGERPRINT_TAG_BYTES, SealedProviderReference, SecurityPartitionDescriptor,
    VerifiedAudienceBinding, VerifiedIdentityFacts, VerifiedIngressAuthentication,
};
use fastmcp_core::partition::{
    CachePartitionKey, DurableOwnerKey, PartitionAdmissionController, PartitionAuthorization,
    PartitionSlot, QuotaPartitionKey, ReplayReservationKey, RevalidationLimits,
};
use fastmcp_core::sha256_bounded;
use fastmcp_transport::ingress::{
    AuthRequestView, AuthenticatedTransportIngress, IngressAuthenticationError,
    IngressAuthenticator, MAX_PRESENTED_CREDENTIAL_BYTES, VerifiedIngressOutcome,
    authenticate_ingress,
};

// ---------------------------------------------------------------------------
// Frozen subject
// ---------------------------------------------------------------------------

const PROVIDER: &str = "org.fastmcp.provider.auth00a";
const CONFIGURATION_GENERATION: u64 = 11;
const ISSUER: &str = "https://issuer.example/auth00a";
const CANONICAL_RESOURCE: &str = "https://resource.example/mcp";
const VALIDATED_AUDIENCE: &str = "https://resource.example/mcp";
const AUDIENCE_POLICY_ID: &str = "accepted-audience-policy/strict";
const AUDIENCE_POLICY_REVISION: u64 = 4;
const TENANT: &str = "tenant-alpha";
const SUBJECT: &str = "subject-7f3a";
const CLIENT: &str = "client-console";
const AUTH_POLICY_REVISION: u64 = 19;
const TRUST_GENERATION: u64 = 3;
const CLAIMS: [(&str, &str); 2] = [("amr", "mfa"), ("scope", "mcp.read")];

/// The raw credential. Its bytes must not appear in any recorded output.
const PRESENTED_CREDENTIAL: &[u8] = b"secret-bearer-value-that-must-never-be-recorded";
const SCHEME: &str = "Bearer";
const TRANSPORT_PROVENANCE: &str = "tls1.3/h2/198.51.100.7";

const FINGERPRINT_KEY_ID: &str = "auth00a-key-2026-09";
const FINGERPRINT_GENERATION: u64 = 2;
const PROVIDER_REFERENCE_MATERIAL: &[u8] = b"opaque-provider-handle-not-a-bearer";
const TOKEN_INSTANCE_MATERIAL: &[u8] = b"opaque-token-instance-not-a-bearer";
const GRANTS: [&str; 2] = ["mcp.read", "mcp.write"];
const OWNERSHIP_EPOCH: u64 = 2;
const QUOTA_EPOCH: u64 = 5;

/// Fixed, non-secret key material for the fingerprints under test.
fn fingerprint_key() -> HmacSha256Key {
    let mut bytes = [0_u8; HMAC_SHA256_KEY_BYTES];
    for (index, slot) in bytes.iter_mut().enumerate() {
        *slot = u8::try_from(index)
            .unwrap_or(0)
            .wrapping_mul(7)
            .wrapping_add(13);
    }
    HmacSha256Key::from_bytes(bytes)
}

// ---------------------------------------------------------------------------
// The provider under the seam
// ---------------------------------------------------------------------------

/// Which single manifest dimension this provider mutates.
///
/// Exactly one dimension moves per variant; everything else is held at the
/// frozen subject above.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Planted {
    /// The conforming provider.
    None,
    /// Row (1): identity is echoed from the caller's own bytes.
    SelfReportedIdentity,
    /// Row (1): a verified identity field is empty.
    EmptyVerifiedField,
    /// Row (3): the fingerprint key identifier is empty.
    EmptyKeyId,
    /// Row (3): the fingerprint key identifier exceeds the frozen ceiling.
    OverlongKeyId,
    /// Row (3): the fingerprint key identifier is not printable ASCII.
    NonAsciiKeyId,
    /// Row (3)/(4): the fingerprint generation moves.
    RotatedGeneration,
    /// Row (4): the token instance rotates.
    RotatedTokenInstance,
    /// Row (4): staleness is configured above the hard ceiling.
    StalenessAboveCeiling,
    /// Row (4): the revalidation outcome is not a dispatched verdict.
    UndispatchedRevalidation,
    /// The provider refuses outright.
    Refuse,
}

struct SubjectAuthenticator {
    planted: Planted,
}

impl SubjectAuthenticator {
    const fn new(planted: Planted) -> Self {
        Self { planted }
    }
}

impl IngressAuthenticator for SubjectAuthenticator {
    fn authenticate(
        &self,
        _cx: &Cx,
        request: &AuthRequestView<'_>,
        _deadline: Duration,
    ) -> Result<VerifiedIngressOutcome, IngressAuthenticationError> {
        if self.planted == Planted::Refuse {
            return Err(IngressAuthenticationError::NotAuthenticated);
        }

        // A conforming provider reports what it verified. The self-reporting
        // variant instead echoes the caller's own presented bytes as identity,
        // which is precisely the laundering AUTH-00 exists to make visible.
        let echoed;
        let subject = if self.planted == Planted::SelfReportedIdentity {
            echoed = String::from_utf8_lossy(request.presented_credential()).into_owned();
            echoed.as_str()
        } else {
            SUBJECT
        };
        let tenant = if self.planted == Planted::EmptyVerifiedField {
            ""
        } else {
            TENANT
        };

        let authentication =
            VerifiedIngressAuthentication::from_verified_provider_output(VerifiedIdentityFacts {
                provider: PROVIDER,
                configuration_generation: CONFIGURATION_GENERATION,
                issuer: ISSUER,
                canonical_resource: CANONICAL_RESOURCE,
                verified_audience_binding: VerifiedAudienceBinding::OAuth {
                    canonical_resource: CANONICAL_RESOURCE.to_owned(),
                    validated_audience: VALIDATED_AUDIENCE.to_owned(),
                    audience_policy_id: AUDIENCE_POLICY_ID.to_owned(),
                    audience_policy_revision: AUDIENCE_POLICY_REVISION,
                    provider: PROVIDER.to_owned(),
                    configuration_generation: CONFIGURATION_GENERATION,
                },
                tenant,
                subject_or_principal: subject,
                authorized_party_or_client: CLIENT,
                verified_claims: &CLAIMS,
                auth_policy_revision: AUTH_POLICY_REVISION,
                trust_generation: TRUST_GENERATION,
            })
            .map_err(|_| IngressAuthenticationError::NotAuthenticated)?;

        let overlong = "k".repeat(SECRET_FINGERPRINT_KEY_ID_MAX_BYTES + 1);
        let key_id = match self.planted {
            Planted::EmptyKeyId => "",
            Planted::OverlongKeyId => overlong.as_str(),
            Planted::NonAsciiKeyId => "auth00a-key-\u{2028}",
            _ => FINGERPRINT_KEY_ID,
        };
        let generation = if self.planted == Planted::RotatedGeneration {
            FINGERPRINT_GENERATION + 1
        } else {
            FINGERPRINT_GENERATION
        };
        let token_material: &[u8] = if self.planted == Planted::RotatedTokenInstance {
            b"opaque-token-instance-AFTER-ROTATION"
        } else {
            TOKEN_INSTANCE_MATERIAL
        };

        let key = fingerprint_key();
        let provider_reference =
            SealedProviderReference::seal(key_id, generation, &key, PROVIDER_REFERENCE_MATERIAL)
                .map_err(|_| IngressAuthenticationError::NotAuthenticated)?;
        let token_instance =
            SealedProviderReference::seal(key_id, generation, &key, token_material)
                .map_err(|_| IngressAuthenticationError::NotAuthenticated)?;

        let staleness = if self.planted == Planted::StalenessAboveCeiling {
            MaximumStaleness::new(HARD_MAXIMUM_STALENESS + Duration::from_secs(1))
        } else {
            MaximumStaleness::new(DEFAULT_MAXIMUM_STALENESS)
        }
        .map_err(|_| IngressAuthenticationError::NotAuthenticated)?;

        let dispatch = if self.planted == Planted::UndispatchedRevalidation {
            RevalidationDispatch::Unknown
        } else {
            RevalidationDispatch::Dispatched
        };

        Ok(VerifiedIngressOutcome {
            authentication,
            rotation: Some(AuthorizationRotationFacts::new(
                provider_reference,
                token_instance,
                &GRANTS,
                TRUST_GENERATION,
                Duration::from_secs(600),
                staleness,
                dispatch,
            )),
        })
    }
}

// ---------------------------------------------------------------------------
// Application-owned runtime boundary
// ---------------------------------------------------------------------------

/// One explicit top-level runtime, which a test harness is permitted to own.
fn application_runtime() -> Runtime {
    RuntimeBuilder::current_thread()
        .with_reactor(create_reactor().expect("platform reactor is available"))
        .blocking_threads(0, 2)
        .build()
        .expect("application-owned runtime builds")
}

/// Drives one ingress attempt through the shipped entrypoint.
fn run_ingress(
    planted: Planted,
) -> Result<AuthenticatedTransportIngress, IngressAuthenticationError> {
    application_runtime().block_on(async move {
        let cx = Cx::current().expect("block_on installs a current Cx");
        let authenticator = SubjectAuthenticator::new(planted);
        let request = AuthRequestView::new(
            PRESENTED_CREDENTIAL,
            SCHEME,
            TRANSPORT_PROVENANCE,
            CANONICAL_RESOURCE,
        )?;
        authenticate_ingress(&cx, Some(&authenticator), &request, Duration::from_secs(5))
    })
}

// ---------------------------------------------------------------------------
// Ordered row manifest
// ---------------------------------------------------------------------------

/// One canonical manifest row: an ordered acceptance row id and its bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ManifestRow {
    id: &'static str,
    parts: Vec<Vec<u8>>,
}

impl ManifestRow {
    fn new(id: &'static str, parts: Vec<Vec<u8>>) -> Self {
        Self { id, parts }
    }
}

/// Length-prefixed canonical encoding over the ordered rows.
///
/// Matches the AUTH-00 B manifest shape so the two halves of the package are
/// diffable against each other.
fn auth_00_a_manifest_digest(rows: &[ManifestRow]) -> [u8; 32] {
    let mut encoded = Vec::new();
    encoded.extend_from_slice(b"auth_00_a_manifest_digest-v1");
    for row in rows {
        let id = row.id.as_bytes();
        encoded.extend_from_slice(
            &u64::try_from(id.len())
                .expect("row id fits u64")
                .to_be_bytes(),
        );
        encoded.extend_from_slice(id);
        encoded.extend_from_slice(
            &u64::try_from(row.parts.len())
                .expect("part count fits u64")
                .to_be_bytes(),
        );
        for part in &row.parts {
            encoded.extend_from_slice(
                &u64::try_from(part.len())
                    .expect("part length fits u64")
                    .to_be_bytes(),
            );
            encoded.extend_from_slice(part);
        }
    }
    sha256_bounded(&encoded, 64 * 1024)
        .expect("canonical manifest stays inside the hash bound")
        .into_bytes()
}

/// Builds the six ordered acceptance rows from one admitted ingress value.
fn manifest(ingress: &AuthenticatedTransportIngress) -> Vec<ManifestRow> {
    let authentication = ingress.authentication();
    let binding = authentication.verified_audience_binding();
    let descriptor = SecurityPartitionDescriptor::from_verified_ingress(authentication);
    let rotation = ingress
        .rotation()
        .expect("the conforming provider returns rotation facts");
    let fingerprint = rotation.provider_reference().fingerprint();

    // Row (1): verified identity, in the order the acceptance criteria name.
    let mut identity_parts: Vec<Vec<u8>> = vec![
        authentication.provider().as_bytes().to_vec(),
        authentication
            .configuration_generation()
            .to_be_bytes()
            .to_vec(),
        authentication.issuer().as_bytes().to_vec(),
        authentication.canonical_resource().as_bytes().to_vec(),
    ];
    identity_parts.extend(binding.canonical_parts());
    identity_parts.push(
        binding
            .audience_policy_id()
            .unwrap_or("<non-oauth>")
            .as_bytes()
            .to_vec(),
    );
    identity_parts.push(binding.audience_policy_revision().map_or_else(
        || b"<non-oauth>".to_vec(),
        |value| value.to_be_bytes().to_vec(),
    ));
    identity_parts.push(authentication.tenant().as_bytes().to_vec());
    identity_parts.push(authentication.subject_or_principal().as_bytes().to_vec());
    identity_parts.push(
        authentication
            .authorized_party_or_client()
            .as_bytes()
            .to_vec(),
    );
    for (name, value) in authentication.verified_claims() {
        identity_parts.push(name.as_bytes().to_vec());
        identity_parts.push(value.as_bytes().to_vec());
    }
    identity_parts.push(authentication.auth_policy_revision().to_be_bytes().to_vec());

    vec![
        ManifestRow::new("1-verified-identity", identity_parts),
        ManifestRow::new(
            "2-security-partition-descriptor",
            vec![descriptor.identity().to_vec()],
        ),
        ManifestRow::new(
            "3-secret-fingerprint",
            vec![
                fingerprint.key_id().as_bytes().to_vec(),
                fingerprint.generation().to_be_bytes().to_vec(),
                fingerprint.tag().to_vec(),
            ],
        ),
        ManifestRow::new(
            "4-rotation-revalidation",
            vec![
                rotation.provider_reference().fingerprint().tag().to_vec(),
                rotation.token_instance().fingerprint().tag().to_vec(),
                rotation.required_grants().join(",").into_bytes(),
                rotation.trust_generation().to_be_bytes().to_vec(),
                rotation.expiry().as_secs().to_be_bytes().to_vec(),
                rotation
                    .maximum_staleness()
                    .bound()
                    .as_secs()
                    .to_be_bytes()
                    .to_vec(),
                rotation.dispatch().to_string().into_bytes(),
            ],
        ),
        ManifestRow::new(
            "5-replay-purposes",
            ReplayPurpose::ALL
                .iter()
                .map(|purpose| purpose.as_str().as_bytes().to_vec())
                .collect(),
        ),
        ManifestRow::new(
            "6-redaction",
            vec![
                format!("{ingress:?}").into_bytes(),
                format!("{authentication:?}").into_bytes(),
                format!("{descriptor:?}").into_bytes(),
                format!("{fingerprint:?}").into_bytes(),
                format!("{binding:?}").into_bytes(),
                format!("{rotation:?}").into_bytes(),
            ],
        ),
    ]
}

// ---------------------------------------------------------------------------
// Observable state, for unchanged-state proof
// ---------------------------------------------------------------------------

/// Every observable AUTH-00 B admission counter plus A's own recorded facts.
///
/// AUTH-00 B's controller appears here only as an observable. The acceptance
/// criteria require a refused mutation to leave replay reservation, quota
/// admission and lookup state unchanged, and those live on B's side of the
/// seam, so the honest way to prove it is to read B's real counters rather
/// than a local imitation of them.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Observables {
    descriptor_identity: [u8; 32],
    fingerprint_tag: [u8; SECRET_FINGERPRINT_TAG_BYTES],
    manifest_digest: [u8; 32],
    records: usize,
    quota_in_use: usize,
    replay_reservations: usize,
    denials: usize,
    lookup_present: bool,
    redacted_debug: String,
}

/// A live admission controller holding one principal's admitted state.
struct AdmittedWorld {
    controller: PartitionAdmissionController,
    quota: QuotaPartitionKey,
    authorization: PartitionAuthorization,
    slot: PartitionSlot,
    _reservation: fastmcp_core::partition::QuotaReservation,
}

impl AdmittedWorld {
    /// Admits one principal, moving every observable off its zero value.
    ///
    /// A later assertion that the counters did not change is evidence only if
    /// they were capable of changing, so this half does real work first.
    fn admit(ingress: &AuthenticatedTransportIngress) -> Self {
        let descriptor =
            SecurityPartitionDescriptor::from_verified_ingress(ingress.authentication())
                .to_partition_descriptor()
                .expect("the verified descriptor projects onto admission input");

        let controller = PartitionAdmissionController::new(RevalidationLimits::default(), 8)
            .expect("controller admits a positive quota capacity");
        let owner =
            DurableOwnerKey::derive(&descriptor, OWNERSHIP_EPOCH).expect("durable owner derives");
        let authorization = PartitionAuthorization::current(&descriptor, &owner);
        let cache = CachePartitionKey::derive(
            &descriptor,
            &GRANTS,
            "token-instance-aaaa",
            "representation-json",
            "cache-domain-main",
        )
        .expect("cache partition derives");
        let slot = PartitionSlot::Cache(cache);
        let quota =
            QuotaPartitionKey::derive(&descriptor, QUOTA_EPOCH).expect("quota partition derives");

        controller.store(&authorization, &slot, b"admitted-record".to_vec());
        let reservation = controller
            .reserve_quota(&quota, 2)
            .expect("quota admits two units");
        for purpose in ReplayPurpose::ALL {
            let replay = ReplayReservationKey::derive(&descriptor, "alias-1", purpose.as_str())
                .expect("replay reservation key derives");
            controller
                .reserve_replay(&descriptor, &replay)
                .expect("a first replay reservation is admitted");
        }

        Self {
            controller,
            quota,
            authorization,
            slot,
            _reservation: reservation,
        }
    }

    /// Reads every observable.
    fn observe(&self, ingress: &AuthenticatedTransportIngress) -> Observables {
        let authentication = ingress.authentication();
        let descriptor = SecurityPartitionDescriptor::from_verified_ingress(authentication);
        let rows = manifest(ingress);
        Observables {
            descriptor_identity: *descriptor.identity(),
            fingerprint_tag: *ingress
                .rotation()
                .expect("rotation facts present")
                .provider_reference()
                .fingerprint()
                .tag(),
            manifest_digest: auth_00_a_manifest_digest(&rows),
            records: self.controller.record_count(),
            quota_in_use: self.controller.quota_in_use(&self.quota),
            replay_reservations: self.controller.replay_reservation_count(),
            denials: self.controller.denial_count(),
            lookup_present: self
                .controller
                .lookup(&self.authorization, &self.slot)
                .is_present(),
            redacted_debug: format!("{ingress:?}{authentication:?}{descriptor:?}"),
        }
    }

    /// Reads the observables that do not depend on an ingress value.
    fn observe_admission_only(&self) -> (usize, usize, usize, usize, bool) {
        (
            self.controller.record_count(),
            self.controller.quota_in_use(&self.quota),
            self.controller.replay_reservation_count(),
            self.controller.denial_count(),
            self.controller
                .lookup(&self.authorization, &self.slot)
                .is_present(),
        )
    }
}

// ---------------------------------------------------------------------------
// auth_00_a_positive
// ---------------------------------------------------------------------------

#[test]
fn auth_00_a_positive() {
    let ingress = run_ingress(Planted::None).expect("the conforming provider admits ingress");
    let authentication = ingress.authentication();
    let binding = authentication.verified_audience_binding();
    let descriptor = SecurityPartitionDescriptor::from_verified_ingress(authentication);
    let rotation = ingress.rotation().expect("rotation facts present");
    let fingerprint = rotation.provider_reference().fingerprint();

    // --- Row (1): verified identity, every named field -----------------------
    assert_eq!(authentication.provider(), PROVIDER);
    assert_eq!(
        authentication.configuration_generation(),
        CONFIGURATION_GENERATION
    );
    assert_eq!(authentication.issuer(), ISSUER);
    assert_eq!(authentication.canonical_resource(), CANONICAL_RESOURCE);
    assert!(binding.is_oauth(), "the subject authenticated over OAuth");
    assert_eq!(binding.audience_policy_id(), Some(AUDIENCE_POLICY_ID));
    assert_eq!(
        binding.audience_policy_revision(),
        Some(AUDIENCE_POLICY_REVISION)
    );
    assert_eq!(authentication.tenant(), TENANT);
    assert_eq!(authentication.subject_or_principal(), SUBJECT);
    assert_eq!(authentication.authorized_party_or_client(), CLIENT);
    assert_eq!(authentication.auth_policy_revision(), AUTH_POLICY_REVISION);
    // Claims are normalized to a deterministic order at construction, so two
    // providers that verified the same set digest identically.
    let claims: Vec<(&str, &str)> = authentication
        .verified_claims()
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    assert_eq!(claims, vec![("amr", "mfa"), ("scope", "mcp.read")]);

    // Ingress was entered only through the opaque value, and the transport
    // provenance was stamped by the framework rather than by the provider.
    assert_eq!(ingress.scheme(), SCHEME);
    assert_eq!(ingress.transport_provenance(), TRANSPORT_PROVENANCE);

    // --- Row (2): descriptor is derived from verified ingress ----------------
    assert_eq!(
        descriptor.verified_ingress(),
        authentication,
        "the descriptor must carry the admitted facts, not a separately assembled copy"
    );
    // Determinism, proven across two *independent* ingress runs rather than by
    // recomputing the same call on the same value. Recomputation only catches
    // nondeterminism inside one invocation; running ingress again proves the
    // identity is a function of the verified facts themselves, which is what
    // this row claims. It is also what would catch a regression in claim-order
    // normalization, since two runs could otherwise order claims differently.
    let independent =
        run_ingress(Planted::None).expect("a second independent ingress admits identically");
    assert_eq!(
        *SecurityPartitionDescriptor::from_verified_ingress(independent.authentication())
            .identity(),
        *descriptor.identity(),
        "descriptor identity must be a deterministic function of the verified facts, \
         equal across independent ingress runs"
    );

    // --- Row (3): secret fingerprint, and the frozen numeric floors ----------
    assert_eq!(fingerprint.key_id(), FINGERPRINT_KEY_ID);
    assert_eq!(fingerprint.generation(), FINGERPRINT_GENERATION);
    // Floors are anchored to literals, never to the constant or type that
    // defines them. `tag()` returns `&[u8; SECRET_FINGERPRINT_TAG_BYTES]`, so
    // asserting its `len()` against that same constant restates the type
    // instead of testing it, and `SECRET_FINGERPRINT_TAG_BYTES` is *defined as*
    // `HMAC_SHA256_TAG_BYTES`. Both held for a 16-byte MAC. The frozen floor is
    // 32 bytes, so 32 is what each one is checked against.
    assert_eq!(fingerprint.tag().len(), 32);
    assert_eq!(SECRET_FINGERPRINT_TAG_BYTES, 32);
    assert_eq!(HMAC_SHA256_TAG_BYTES, 32);
    assert_eq!(HMAC_SHA256_KEY_BYTES, 32);
    assert_eq!(SECRET_FINGERPRINT_KEY_ID_MIN_BYTES, 1);
    assert_eq!(SECRET_FINGERPRINT_KEY_ID_MAX_BYTES, 128);
    assert!(
        fingerprint.key_id().is_ascii(),
        "key identifiers are printable ASCII"
    );
    // Equality is decided in constant time, through the key, and only there.
    fingerprint
        .verify_material(&fingerprint_key(), PROVIDER_REFERENCE_MATERIAL)
        .expect("the fingerprint verifies against the material it names");

    // --- Row (4): rotation and revalidation facts ----------------------------
    let grants: Vec<&str> = rotation
        .required_grants()
        .iter()
        .map(String::as_str)
        .collect();
    assert_eq!(grants, vec!["mcp.read", "mcp.write"]);
    assert_eq!(rotation.trust_generation(), TRUST_GENERATION);
    assert_eq!(rotation.expiry(), Duration::from_secs(600));
    assert_eq!(
        rotation.maximum_staleness().bound(),
        DEFAULT_MAXIMUM_STALENESS
    );
    assert_eq!(DEFAULT_MAXIMUM_STALENESS, Duration::from_secs(30));
    assert_eq!(HARD_MAXIMUM_STALENESS, Duration::from_mins(5));
    assert_eq!(rotation.dispatch(), RevalidationDispatch::Dispatched);
    assert!(
        rotation.is_fresh_after(Duration::from_secs(29)),
        "a dispatched verdict inside the bound is fresh"
    );
    assert!(
        !rotation.is_fresh_after(Duration::from_secs(31)),
        "freshness expires at the configured bound"
    );

    // --- Row (5): exactly the two enterprise replay purposes -----------------
    assert_eq!(ReplayPurpose::ALL.len(), 2);
    assert_eq!(
        ReplayPurpose::ALL,
        [
            ReplayPurpose::EnterpriseIdentityAssertionReplay,
            ReplayPurpose::EnterpriseIdJagJtiReplay,
        ]
    );
    assert_ne!(
        ReplayPurpose::EnterpriseIdentityAssertionReplay.domain(),
        ReplayPurpose::EnterpriseIdJagJtiReplay.domain(),
        "each replay purpose is its own domain, so a reservation for one cannot \
         satisfy a lookup for the other"
    );

    // --- Row (6): redaction ---------------------------------------------------
    let credential = String::from_utf8_lossy(PRESENTED_CREDENTIAL).into_owned();
    let rendered = [
        format!("{ingress:?}"),
        format!("{authentication:?}"),
        format!("{descriptor:?}"),
        format!("{fingerprint:?}"),
        format!("{binding:?}"),
        format!("{rotation:?}"),
    ];
    for rendering in &rendered {
        assert!(
            !rendering.contains(&credential),
            "no raw credential may reach Debug output: {rendering}"
        );
        for secret in [SUBJECT, TENANT, ISSUER, CLIENT, VALIDATED_AUDIENCE] {
            assert!(
                !rendering.contains(secret),
                "no principal identity may reach Debug output: {rendering}"
            );
        }
    }
    assert!(
        !format!("{fingerprint:?}").contains(&hex_lower(fingerprint.tag())),
        "the fingerprint tag stays behind its accessor rather than in Debug output"
    );

    // --- Manifest digest ------------------------------------------------------
    let rows = manifest(&ingress);
    assert_eq!(rows.len(), 6, "six ordered acceptance rows");
    let digest = auth_00_a_manifest_digest(&rows);
    assert_eq!(
        digest,
        auth_00_a_manifest_digest(&manifest(
            &run_ingress(Planted::None).expect("a second conforming ingress")
        )),
        "auth_00_a_manifest_digest must be stable across independent derivations"
    );
    assert_ne!(digest, [0_u8; 32], "the digest is not a degenerate value");
}

/// Lowercase hex, used only to prove a value is absent from rendered output.
fn hex_lower(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(DIGITS[usize::from(byte >> 4)] as char);
        out.push(DIGITS[usize::from(byte & 0x0f)] as char);
    }
    out
}

// ---------------------------------------------------------------------------
// auth_00_a_planted_negative
// ---------------------------------------------------------------------------

/// What a planted mutation must do, stated per row rather than in aggregate.
///
/// A single blanket predicate over all mutations would pass if every one of
/// them happened to be refused for the *same* reason, which would prove far
/// less than it appears to. Each row therefore names its own denial class.
#[derive(Debug, Clone, Copy)]
enum Denial {
    /// Ingress itself refuses: the typed transport-side boundary.
    TypedIngressRefusal(IngressAuthenticationError),
    /// Ingress admits, but the recorded identity is not the frozen subject and
    /// the manifest digest moves. Self-reporting cannot be refused by a type —
    /// a provider is entitled to assert what it verified — so the observable
    /// property is that it cannot be laundered in silently.
    AdmittedButDigestMoves,
}

#[test]
fn auth_00_a_planted_negative() {
    // --- Control half: prove every observable is capable of moving -----------
    //
    // A zero delta below is evidence only if a non-zero delta was reachable.
    let baseline = run_ingress(Planted::None).expect("the conforming provider admits ingress");
    let world = AdmittedWorld::admit(&baseline);

    let empty_world = PartitionAdmissionController::new(RevalidationLimits::default(), 8)
        .expect("controller admits a positive quota capacity");
    assert_eq!(empty_world.record_count(), 0);
    assert_eq!(empty_world.replay_reservation_count(), 0);

    let admitted = world.observe(&baseline);
    assert_eq!(admitted.records, 1, "the control half stored a record");
    assert_eq!(admitted.quota_in_use, 2, "the control half reserved quota");
    assert_eq!(
        admitted.replay_reservations,
        ReplayPurpose::ALL.len(),
        "the control half reserved one replay alias per purpose"
    );
    assert!(admitted.lookup_present, "the stored record is reachable");
    assert_ne!(admitted.descriptor_identity, [0_u8; 32]);
    assert_ne!(admitted.manifest_digest, [0_u8; 32]);

    // --- Each planted mutation, one dimension at a time ----------------------
    let cases: [(Planted, &str, Denial); 10] = [
        (
            Planted::EmptyVerifiedField,
            "row 1: an empty verified identity field",
            Denial::TypedIngressRefusal(IngressAuthenticationError::NotAuthenticated),
        ),
        (
            Planted::SelfReportedIdentity,
            "row 1: identity echoed from the caller's own bytes",
            Denial::AdmittedButDigestMoves,
        ),
        (
            Planted::EmptyKeyId,
            "row 3: an empty fingerprint key identifier",
            Denial::TypedIngressRefusal(IngressAuthenticationError::NotAuthenticated),
        ),
        (
            Planted::OverlongKeyId,
            "row 3: a key identifier above the 128-byte ceiling",
            Denial::TypedIngressRefusal(IngressAuthenticationError::NotAuthenticated),
        ),
        (
            Planted::NonAsciiKeyId,
            "row 3: a non-printable-ASCII key identifier",
            Denial::TypedIngressRefusal(IngressAuthenticationError::NotAuthenticated),
        ),
        (
            Planted::RotatedGeneration,
            "row 3: the fingerprint generation rotates",
            Denial::AdmittedButDigestMoves,
        ),
        (
            Planted::RotatedTokenInstance,
            "row 4: the token instance rotates",
            Denial::AdmittedButDigestMoves,
        ),
        (
            Planted::StalenessAboveCeiling,
            "row 4: staleness configured above the 5-minute ceiling",
            Denial::TypedIngressRefusal(IngressAuthenticationError::NotAuthenticated),
        ),
        (
            Planted::UndispatchedRevalidation,
            "row 4: an undispatched revalidation outcome",
            Denial::AdmittedButDigestMoves,
        ),
        (
            Planted::Refuse,
            "the provider refuses outright",
            Denial::TypedIngressRefusal(IngressAuthenticationError::NotAuthenticated),
        ),
    ];

    for (planted, description, expected) in cases {
        let outcome = run_ingress(planted);
        match expected {
            Denial::TypedIngressRefusal(error) => {
                assert_eq!(
                    outcome.err(),
                    Some(error),
                    "{description} must reach the typed ingress denial"
                );
            }
            Denial::AdmittedButDigestMoves => {
                let mutated = outcome.unwrap_or_else(|error| {
                    panic!("{description} should still admit, got {error:?}")
                });
                assert_ne!(
                    auth_00_a_manifest_digest(&manifest(&mutated)),
                    admitted.manifest_digest,
                    "{description} must move auth_00_a_manifest_digest; a mutation that \
                     digests identically is one the manifest does not actually bind"
                );
            }
        }

        // Unchanged-state proof, after every case regardless of its class.
        let after = world.observe_admission_only();
        assert_eq!(
            after,
            (
                admitted.records,
                admitted.quota_in_use,
                admitted.replay_reservations,
                admitted.denials,
                admitted.lookup_present,
            ),
            "{description} must leave record, quota, replay reservation, denial and lookup \
             state byte-for-byte unchanged"
        );
    }

    // --- The baseline survives every planted mutation ------------------------
    let unchanged = world.observe(&baseline);
    assert_eq!(
        unchanged, admitted,
        "the accepted principal's descriptor, fingerprint, manifest digest, admission state \
         and redacted output must all be unchanged after every refusal"
    );

    // --- Row 3/4: a rotated fingerprint fails constant-time verification -----
    //
    // The tag is never compared with `==`; the only way to decide a match is
    // through the key, so that is how the negative decides it too.
    let baseline_fingerprint = baseline
        .rotation()
        .expect("rotation facts present")
        .provider_reference()
        .fingerprint();
    assert_eq!(
        baseline_fingerprint
            .verify_material(&fingerprint_key(), TOKEN_INSTANCE_MATERIAL)
            .unwrap_err(),
        IngressFactsError::FingerprintMismatch,
        "a fingerprint must not verify against material it does not name"
    );

    // --- Row 5: replay purposes are domains, not labels ----------------------
    let descriptor = SecurityPartitionDescriptor::from_verified_ingress(baseline.authentication())
        .to_partition_descriptor()
        .expect("the verified descriptor projects onto admission input");
    let assertion_replay = ReplayReservationKey::derive(
        &descriptor,
        "alias-1",
        ReplayPurpose::EnterpriseIdentityAssertionReplay.as_str(),
    )
    .expect("replay reservation key derives");
    let jti_replay = ReplayReservationKey::derive(
        &descriptor,
        "alias-1",
        ReplayPurpose::EnterpriseIdJagJtiReplay.as_str(),
    )
    .expect("replay reservation key derives");
    assert_ne!(
        assertion_replay.as_bytes(),
        jti_replay.as_bytes(),
        "the same alias under two replay purposes must not collide, or a reservation taken \
         for one would silently satisfy the other"
    );

    // --- Row 6: the redaction boundary holds under mutation ------------------
    let credential = String::from_utf8_lossy(PRESENTED_CREDENTIAL).into_owned();
    let self_reported =
        run_ingress(Planted::SelfReportedIdentity).expect("the self-reporting provider admits");
    assert_eq!(
        self_reported.authentication().subject_or_principal(),
        credential,
        "the planted provider did echo the credential as identity, so the redaction check \
         below is exercising a real disclosure risk rather than a hypothetical one"
    );
    assert!(
        !format!("{:?}", self_reported.authentication()).contains(&credential),
        "even a provider that laundered the credential into identity must not leak it through \
         Debug, because Debug never renders identity at all"
    );

    // --- Boundary refusals at the transport seam -----------------------------
    let runtime = application_runtime();
    runtime.block_on(async {
        let cx = Cx::current().expect("block_on installs a current Cx");
        let authenticator = SubjectAuthenticator::new(Planted::None);

        assert_eq!(
            AuthRequestView::new(b"", SCHEME, TRANSPORT_PROVENANCE, CANONICAL_RESOURCE).err(),
            Some(IngressAuthenticationError::CredentialAbsent),
            "an absent credential is refused before any provider is invoked"
        );

        let oversized = vec![b'x'; MAX_PRESENTED_CREDENTIAL_BYTES + 1];
        assert_eq!(
            AuthRequestView::new(&oversized, SCHEME, TRANSPORT_PROVENANCE, CANONICAL_RESOURCE)
                .err(),
            Some(IngressAuthenticationError::CredentialTooLarge),
            "an oversized presentation is refused before any provider is invoked"
        );

        let request = AuthRequestView::new(
            PRESENTED_CREDENTIAL,
            SCHEME,
            TRANSPORT_PROVENANCE,
            CANONICAL_RESOURCE,
        )
        .expect("the frozen presentation is admissible");
        assert_eq!(
            authenticate_ingress(&cx, None, &request, Duration::from_secs(5)).err(),
            Some(IngressAuthenticationError::NoAuthenticatorRegistered),
            "ingress with no registered authenticator fails closed"
        );

        // The opaque value is minted only here. A provider's own return value
        // is not admissible anywhere downstream, which is the seal.
        let admitted_here =
            authenticate_ingress(&cx, Some(&authenticator), &request, Duration::from_secs(5))
                .expect("the conforming provider admits ingress");
        assert_eq!(admitted_here.scheme(), SCHEME);
    });

    // Admission state is still untouched after the boundary cases.
    assert_eq!(
        world.observe_admission_only(),
        (
            admitted.records,
            admitted.quota_in_use,
            admitted.replay_reservations,
            admitted.denials,
            admitted.lookup_present,
        ),
        "transport-boundary refusals must not reach admission state at all"
    );
}
