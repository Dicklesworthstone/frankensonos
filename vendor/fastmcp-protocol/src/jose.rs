//! Bounded, local-only RS256 compact-JWS verification.
//!
//! This module admits only pre-supplied RSA JWK Sets and never fetches keys or
//! evaluates token policy. In particular, issuer, audience, expiry, replay,
//! and authorization policy remain the responsibility of the owning layer.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use ring::signature::{RSA_PKCS1_2048_8192_SHA256, RsaPublicKeyComponents};
use serde_json::{Map, Value};
use zeroize::Zeroizing;

/// Maximum accepted compact-JWS bytes, including separators.
pub const MAX_COMPACT_JWS_BYTES: usize = 16 * 1024;
/// Maximum decoded protected-header bytes.
pub const MAX_JWS_HEADER_BYTES: usize = 2 * 1024;
/// Maximum decoded JSON-claims bytes.
pub const MAX_JWS_CLAIMS_BYTES: usize = 8 * 1024;
/// Maximum decoded signature bytes.
pub const MAX_JWS_SIGNATURE_BYTES: usize = 1024;
/// Maximum admitted JWKS document bytes.
pub const MAX_JWKS_BYTES: usize = 64 * 1024;
/// Maximum admitted keys in one JWKS document.
pub const MAX_JWKS_KEYS: usize = 64;
/// Maximum UTF-8 bytes in a JWK or JWS key identifier.
pub const MAX_KID_BYTES: usize = 256;
/// The only RSA modulus byte lengths admitted by the FND-09 profile.
pub const ADMITTED_RSA_MODULUS_BYTE_LENGTHS: [usize; 3] = [256, 384, 512];
/// The only RSA public exponent admitted by the FND-09 profile: 65537.
pub const ADMITTED_RSA_PUBLIC_EXPONENT: [u8; 3] = [0x01, 0x00, 0x01];

const MAX_ADMITTED_RSA_MODULUS_BYTES: usize = 512;

/// Maximum finite duration delegated to one external RS256 signing operation.
pub const MAX_EXTERNAL_RS256_SIGNING_DEADLINE: Duration = Duration::from_secs(30);
/// Maximum bytes in a redacted provider-provenance label.
pub const MAX_EXTERNAL_RS256_PROVENANCE_BYTES: usize = 256;

const MAX_JSON_NESTING: usize = 32;
const MAX_JSON_MEMBERS: usize = 1024;

/// A bounded JOSE admission or verification failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoseError {
    /// The supplied input exceeded a fixed resource bound.
    TooLarge(&'static str),
    /// The compact serialization was not exactly three non-empty segments.
    InvalidCompactSerialization,
    /// A segment was not canonical unpadded base64url.
    InvalidBase64Url(&'static str),
    /// JSON did not parse into the required shape.
    InvalidJson(&'static str),
    /// An object repeated a member name.
    DuplicateJsonMember,
    /// The protected header did not select RS256.
    UnsupportedAlgorithm,
    /// The protected header did not contain a usable `kid`.
    MissingKeyId,
    /// A header parameter would change this module's fixed local-key model.
    DisallowedHeader(&'static str),
    /// The supplied JWK Set was not suitable for local RS256 verification.
    InvalidJwk(&'static str),
    /// More than one JWK claimed the same key identifier.
    DuplicateKeyId,
    /// The header selected no admitted JWK.
    UnknownKeyId,
    /// The signature did not verify with the selected admitted key.
    InvalidSignature,
}

impl fmt::Display for JoseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge(part) => write!(formatter, "JOSE {part} exceeds its bound"),
            Self::InvalidCompactSerialization => {
                formatter.write_str("JWS compact serialization must have three non-empty segments")
            }
            Self::InvalidBase64Url(part) => write!(formatter, "invalid base64url {part}"),
            Self::InvalidJson(part) => write!(formatter, "invalid JSON {part}"),
            Self::DuplicateJsonMember => formatter.write_str("duplicate JSON object member"),
            Self::UnsupportedAlgorithm => formatter.write_str("only RS256 is admitted"),
            Self::MissingKeyId => formatter.write_str("missing or invalid JWS key identifier"),
            Self::DisallowedHeader(name) => {
                write!(formatter, "JWS header parameter {name:?} is not admitted")
            }
            Self::InvalidJwk(reason) => write!(formatter, "invalid admitted RSA JWK: {reason}"),
            Self::DuplicateKeyId => formatter.write_str("duplicate JWK key identifier"),
            Self::UnknownKeyId => formatter.write_str("JWS key identifier is not admitted"),
            Self::InvalidSignature => formatter.write_str("invalid RS256 signature"),
        }
    }
}

impl std::error::Error for JoseError {}

/// The integrity-protected JWS fields retained after RS256 verification.
///
/// No compact bearer, encoded header, or signature is retained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedRs256Header {
    kid: String,
}

impl VerifiedRs256Header {
    /// The exact, case-sensitive key identifier selected from the protected header.
    #[must_use]
    pub fn kid(&self) -> &str {
        &self.kid
    }
}

/// Verified JSON claims and the typed, integrity-protected key selector.
///
/// This is deliberately not a bearer-token wrapper: it does not retain the
/// compact serialization, signing input, or signature.
#[derive(Debug, Clone, PartialEq)]
pub struct VerifiedCompactJws {
    header: VerifiedRs256Header,
    claims: Value,
}

impl VerifiedCompactJws {
    /// The verified protected-header fields this primitive admits.
    #[must_use]
    pub fn header(&self) -> &VerifiedRs256Header {
        &self.header
    }

    /// The decoded JSON claims object.
    ///
    /// JSON object-member order and number source lexemes are not retained by
    /// `serde_json::Value`; this primitive retains neither raw claims bytes nor
    /// the compact bearer.
    #[must_use]
    pub fn claims(&self) -> &Value {
        &self.claims
    }

    /// Consume this result and return the decoded JSON claims object.
    #[must_use]
    pub fn into_claims(self) -> Value {
        self.claims
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AdmittedRsaJwk {
    modulus: Vec<u8>,
    exponent: Vec<u8>,
}

/// A locally admitted, bounded RSA JWK Set for RS256 verification.
///
/// Admission accepts public RSA keys only. It does not fetch `jku`/`x5u`, use
/// a JWS-supplied key, or retain any bearer token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmittedRsaJwks {
    keys: BTreeMap<String, AdmittedRsaJwk>,
}

impl AdmittedRsaJwks {
    /// Admit a bounded JSON Web Key Set containing public RS256 RSA keys.
    pub fn from_json(jwks: &[u8]) -> Result<Self, JoseError> {
        if jwks.len() > MAX_JWKS_BYTES {
            return Err(JoseError::TooLarge("JWKS"));
        }
        reject_duplicate_json_members(jwks)?;
        let root = parse_json(jwks, "JWKS")?;
        let root = root
            .as_object()
            .ok_or(JoseError::InvalidJson("JWKS root"))?;
        reject_remote_key_reference(root)?;
        let keys = root
            .get("keys")
            .and_then(Value::as_array)
            .ok_or(JoseError::InvalidJwk(
                "JWKS requires an array `keys` member",
            ))?;
        if keys.is_empty() || keys.len() > MAX_JWKS_KEYS {
            return Err(JoseError::InvalidJwk("JWKS key count"));
        }

        let mut admitted = BTreeMap::new();
        for value in keys {
            let key = admit_rsa_jwk(value)?;
            if admitted.insert(key.0, key.1).is_some() {
                return Err(JoseError::DuplicateKeyId);
            }
        }
        Ok(Self { keys: admitted })
    }

    /// Number of locally admitted public keys.
    #[must_use]
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether no public keys were admitted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Whether this exact key identifier is admitted.
    #[must_use]
    pub fn contains_kid(&self, kid: &str) -> bool {
        self.keys.contains_key(kid)
    }
}

/// Immutable provider, key, ring, and configuration generations for one
/// externally custodied RS256 operation.
///
/// These four values are deliberately separate: a provider restart, key
/// rotation, ring publication change, or deployment configuration change must
/// never be mistaken for another kind of continuity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(
    clippy::struct_field_names,
    reason = "the repeated suffix distinguishes four independent security generations at the API boundary"
)]
pub struct Rs256SigningBinding {
    provider_generation: u64,
    key_generation: u64,
    ring_generation: u64,
    configuration_generation: u64,
}

impl Rs256SigningBinding {
    /// Creates a nonzero immutable signing binding.
    pub const fn new(
        provider_generation: u64,
        key_generation: u64,
        ring_generation: u64,
        configuration_generation: u64,
    ) -> Result<Self, JwsSignerConfigurationError> {
        if provider_generation == 0
            || key_generation == 0
            || ring_generation == 0
            || configuration_generation == 0
        {
            return Err(JwsSignerConfigurationError::ZeroGeneration);
        }
        Ok(Self {
            provider_generation,
            key_generation,
            ring_generation,
            configuration_generation,
        })
    }

    /// Provider generation selected at adapter admission.
    #[must_use]
    pub const fn provider_generation(self) -> u64 {
        self.provider_generation
    }

    /// External key generation selected at adapter admission.
    #[must_use]
    pub const fn key_generation(self) -> u64 {
        self.key_generation
    }

    /// Immutable local signing-ring generation.
    #[must_use]
    pub const fn ring_generation(self) -> u64 {
        self.ring_generation
    }

    /// Immutable signer configuration generation.
    #[must_use]
    pub const fn configuration_generation(self) -> u64 {
        self.configuration_generation
    }
}

/// Nonsecret provenance supplied by an external signer adapter.
///
/// This is intentionally only a bounded redacted label. Provider handles,
/// attestation bodies, requests, credentials, and key material have no place
/// in this protocol-facing type.
pub struct RedactedSignerProvenance {
    label: String,
}

impl RedactedSignerProvenance {
    /// Admits one pre-redacted provider label.
    pub fn new(label: impl Into<String>) -> Result<Self, JwsSignerConfigurationError> {
        let label = label.into();
        if label.is_empty()
            || label.len() > MAX_EXTERNAL_RS256_PROVENANCE_BYTES
            || label.bytes().any(|byte| byte.is_ascii_control())
        {
            return Err(JwsSignerConfigurationError::InvalidProvenance);
        }
        Ok(Self { label })
    }
}

impl fmt::Debug for RedactedSignerProvenance {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RedactedSignerProvenance")
            .field("label_bytes", &self.label.len())
            .finish()
    }
}

/// A canonical public RS256 key bound to one admitted external signer.
///
/// The key contains no private material. It is not a JWKS publication receipt:
/// publication, endpoint read-back, issuer activation, and consumer-store CAS
/// remain higher-layer work.
pub struct AttestedRs256PublicKey {
    kid: String,
    modulus: Vec<u8>,
    binding: Rs256SigningBinding,
    provenance: RedactedSignerProvenance,
}

impl AttestedRs256PublicKey {
    /// Admits canonical public RSA components reported by an external adapter.
    ///
    /// The exponent is fixed by the FND-09 RS256 profile and is consequently
    /// not caller-selectable. The adapter supplies only the canonical unsigned
    /// modulus, `kid`, immutable generations, and a redacted provenance label.
    pub fn admit(
        kid: impl Into<String>,
        modulus: Vec<u8>,
        binding: Rs256SigningBinding,
        provenance: RedactedSignerProvenance,
    ) -> Result<Self, JwsSignerConfigurationError> {
        let kid = kid.into();
        if kid.is_empty() || kid.len() > MAX_KID_BYTES {
            return Err(JwsSignerConfigurationError::InvalidKeyId);
        }
        if !ADMITTED_RSA_MODULUS_BYTE_LENGTHS.contains(&modulus.len())
            || modulus.first() == Some(&0)
            || modulus.first().is_none_or(|first| first & 0x80 == 0)
        {
            return Err(JwsSignerConfigurationError::InvalidPublicKey);
        }
        Ok(Self {
            kid,
            modulus,
            binding,
            provenance,
        })
    }

    /// The exact case-sensitive key identifier selected by the facade.
    #[must_use]
    pub fn kid(&self) -> &str {
        &self.kid
    }

    /// Immutable generations this public key was admitted under.
    #[must_use]
    pub const fn binding(&self) -> Rs256SigningBinding {
        self.binding
    }

    /// Returns this key's one-key canonical public JWKS representation.
    ///
    /// The bytes contain public RSA material only. They are suitable for an
    /// owning issuer to publish and later compare byte-for-byte with its
    /// independently read-back JWKS response; they do not prove publication
    /// or activate signing by themselves.
    pub fn canonical_public_jwks(&self) -> Result<CanonicalRs256PublicJwks, JoseError> {
        let value = serde_json::json!({"keys": [self.canonical_jwk_value()]});
        let bytes =
            serde_json::to_vec(&value).map_err(|_| JoseError::InvalidJson("canonical JWKS"))?;
        Ok(CanonicalRs256PublicJwks {
            bytes,
            binding: self.binding,
        })
    }

    fn canonical_jwk_value(&self) -> Value {
        serde_json::json!({
            "alg": "RS256",
            "e": base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(ADMITTED_RSA_PUBLIC_EXPONENT),
            "kid": &self.kid,
            "kty": "RSA",
            "n": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&self.modulus),
            "use": "sig"
        })
    }

    fn admitted_jwks(&self) -> AdmittedRsaJwks {
        let mut keys = BTreeMap::new();
        keys.insert(
            self.kid.clone(),
            AdmittedRsaJwk {
                modulus: self.modulus.clone(),
                exponent: ADMITTED_RSA_PUBLIC_EXPONENT.to_vec(),
            },
        );
        AdmittedRsaJwks { keys }
    }
}

/// Canonical public JWKS bytes for exactly one externally custodied RS256 key.
///
/// This type intentionally carries no private material and has no public
/// constructor. Only an admitted external signer key can mint it.
pub struct CanonicalRs256PublicJwks {
    bytes: Vec<u8>,
    binding: Rs256SigningBinding,
}

impl CanonicalRs256PublicJwks {
    /// Borrows the exact public JWKS bytes an issuer must publish and read back.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Returns the immutable signing binding represented by these bytes.
    #[must_use]
    pub const fn binding(&self) -> Rs256SigningBinding {
        self.binding
    }
}

/// A canonical public JWKS containing the active RS256 key and any retained
/// verification keys for one issuer key-ring generation.
///
/// This is deliberately minted only from admitted external signer keys.  A
/// server cannot concatenate caller-provided JSON to manufacture overlap: the
/// exact public components, `kid` uniqueness, and deterministic key ordering
/// remain owned by the JOSE boundary.
#[derive(Clone)]
pub struct CanonicalRs256PublicJwksSet {
    bytes: Vec<u8>,
    generation: u64,
}

impl CanonicalRs256PublicJwksSet {
    /// Borrows the exact bytes every advertised endpoint must return.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Returns the monotonically selected issuer key-ring generation.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }
}

impl fmt::Debug for CanonicalRs256PublicJwksSet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CanonicalRs256PublicJwksSet")
            .field("bytes", &self.bytes.len())
            .field("generation", &self.generation)
            .finish()
    }
}

/// A selected external signing key plus public verification overlap retained
/// until the issuer's last artifact can expire.
///
/// The ring owns no private material.  It is a profile-neutral public-key
/// construction primitive; issuer persistence and endpoint publication remain
/// server responsibilities.
#[derive(Clone)]
pub struct Rs256PublicKeyRing {
    active: Arc<ExternalRs256Signer>,
    retained: Vec<Arc<ExternalRs256Signer>>,
    generation: u64,
}

impl Rs256PublicKeyRing {
    /// Creates one nonempty, generation-fenced ring.
    ///
    /// Every `kid` must be unique.  Supplying the active key again as a
    /// retained key is rejected rather than silently normalised.
    pub fn new(
        active: Arc<ExternalRs256Signer>,
        retained: Vec<Arc<ExternalRs256Signer>>,
        generation: u64,
    ) -> Result<Self, JwsSignerConfigurationError> {
        if generation == 0 {
            return Err(JwsSignerConfigurationError::ZeroGeneration);
        }
        if retained.len().saturating_add(1) > MAX_JWKS_KEYS {
            return Err(JwsSignerConfigurationError::InvalidPublicKey);
        }
        let mut kids = BTreeSet::new();
        if !kids.insert(active.public_key.kid.clone()) {
            return Err(JwsSignerConfigurationError::InvalidKeyId);
        }
        for signer in &retained {
            if !kids.insert(signer.public_key.kid.clone()) {
                return Err(JwsSignerConfigurationError::InvalidKeyId);
            }
        }
        Ok(Self {
            active,
            retained,
            generation,
        })
    }

    /// Returns the signer permitted to create new artifacts in this generation.
    #[must_use]
    pub fn active_signer(&self) -> &Arc<ExternalRs256Signer> {
        &self.active
    }

    /// Reports whether this verification ring contains the exact public key
    /// bound to `signer`. This permits an issuer to prove that a successor
    /// ring retains its still-live active key during rotation.
    #[must_use]
    pub fn contains_signer(&self, signer: &ExternalRs256Signer) -> bool {
        let Some(expected) = signer.canonical_public_jwks().ok() else {
            return false;
        };
        self.active
            .canonical_public_jwks()
            .is_ok_and(|active| active.as_bytes() == expected.as_bytes())
            || self.retained.iter().any(|retained| {
                retained
                    .canonical_public_jwks()
                    .is_ok_and(|candidate| candidate.as_bytes() == expected.as_bytes())
            })
    }

    /// Returns the canonical public key identifiers carried by this ring.
    /// Callers must still use [`Self::retains_key_from`] when a key's exact
    /// public bytes, rather than its identifier alone, form a security fence.
    #[must_use]
    pub fn key_ids(&self) -> Vec<String> {
        let mut key_ids = Vec::with_capacity(self.len());
        key_ids.push(self.active.public_key.kid.clone());
        key_ids.extend(
            self.retained
                .iter()
                .map(|signer| signer.public_key.kid.clone()),
        );
        key_ids
    }

    /// Returns the exact canonical single-key JWKS identity bound to `key_id`.
    ///
    /// Durable consumers use these bytes to fence a restart against replacing
    /// a retained key with different RSA material under the same `kid`.
    #[must_use]
    pub fn canonical_public_key_identity(&self, key_id: &str) -> Option<CanonicalRs256PublicJwks> {
        let signer = if self.active.public_key.kid == key_id {
            Some(&self.active)
        } else {
            self.retained
                .iter()
                .find(|signer| signer.public_key.kid == key_id)
        }?;
        signer.canonical_public_jwks().ok()
    }

    /// Confirms that `self` retains the exact public key named by `key_id`
    /// from `previous`, rather than merely reusing that key identifier.
    #[must_use]
    pub fn retains_key_from(&self, previous: &Self, key_id: &str) -> bool {
        let previous_signer = if previous.active.public_key.kid == key_id {
            Some(&previous.active)
        } else {
            previous
                .retained
                .iter()
                .find(|signer| signer.public_key.kid == key_id)
        };
        previous_signer.is_some_and(|signer| self.contains_signer(signer))
    }

    /// Returns the immutable key-ring generation.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Returns the number of public verification keys, including the active key.
    #[must_use]
    pub fn len(&self) -> usize {
        self.retained.len() + 1
    }

    /// Reports whether this ring has no verification keys.
    ///
    /// A valid ring always owns one active key, so this is always false.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        false
    }

    /// Creates the exact canonical JWKS for the active plus retained keys.
    pub fn canonical_public_jwks(&self) -> Result<CanonicalRs256PublicJwksSet, JoseError> {
        let mut keys = BTreeMap::new();
        keys.insert(
            self.active.public_key.kid.clone(),
            self.active.public_key.canonical_jwk_value(),
        );
        for signer in &self.retained {
            if keys
                .insert(
                    signer.public_key.kid.clone(),
                    signer.public_key.canonical_jwk_value(),
                )
                .is_some()
            {
                return Err(JoseError::DuplicateKeyId);
            }
        }
        let bytes = serde_json::to_vec(&serde_json::json!({
            "keys": keys.into_values().collect::<Vec<_>>()
        }))
        .map_err(|_| JoseError::InvalidJson("canonical JWKS"))?;
        if bytes.len() > MAX_JWKS_BYTES {
            return Err(JoseError::TooLarge("JWKS"));
        }
        Ok(CanonicalRs256PublicJwksSet {
            bytes,
            generation: self.generation,
        })
    }

    /// Refuses retirement before the maximum lifetime of artifacts signed by
    /// the retiring key has elapsed.  The caller supplies an issuer-owned
    /// monotonic time value; this protocol module does not consult wall clock.
    pub fn retire(
        &self,
        retiring_kid: &str,
        now_unix_seconds: i64,
        last_artifact_expires_at: i64,
        successor: Arc<ExternalRs256Signer>,
        successor_generation: u64,
    ) -> Result<Self, JwsSignerConfigurationError> {
        if last_artifact_expires_at > now_unix_seconds || successor_generation <= self.generation {
            return Err(JwsSignerConfigurationError::RetirementNotPermitted);
        }
        if self.active.public_key.kid != retiring_kid
            && !self
                .retained
                .iter()
                .any(|signer| signer.public_key.kid == retiring_kid)
        {
            return Err(JwsSignerConfigurationError::InvalidKeyId);
        }
        let mut retained = Vec::with_capacity(self.retained.len() + 1);
        if self.active.public_key.kid != retiring_kid {
            retained.push(Arc::clone(&self.active));
        }
        retained.extend(
            self.retained
                .iter()
                .filter(|signer| signer.public_key.kid != retiring_kid)
                .cloned(),
        );
        Self::new(successor, retained, successor_generation)
    }

    /// Selects a successor signing key while retaining every current
    /// verification key in the next canonical JWKS generation.
    pub fn rotate(
        &self,
        successor: Arc<ExternalRs256Signer>,
        successor_generation: u64,
    ) -> Result<Self, JwsSignerConfigurationError> {
        if successor_generation <= self.generation {
            return Err(JwsSignerConfigurationError::RetirementNotPermitted);
        }
        let mut retained = Vec::with_capacity(self.retained.len() + 1);
        retained.push(Arc::clone(&self.active));
        retained.extend(self.retained.iter().cloned());
        Self::new(successor, retained, successor_generation)
    }
}

impl fmt::Debug for Rs256PublicKeyRing {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Rs256PublicKeyRing")
            .field("generation", &self.generation)
            .field("key_count", &self.len())
            .finish()
    }
}

/// Closed consumer profile for a publication/read-back activation receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SigningActivationProfile {
    /// OpenID Connect ID-token issuance.
    OidcIdToken,
}

/// One externally fetched JWKS response, retained as bounded evidence for an
/// activation receipt.  This type is evidence, not a fetch API: the owning
/// server must acquire it through its configured endpoint verifier.
#[derive(Clone)]
pub struct JwksEndpointReadBack {
    uri: String,
    origin: String,
    bytes: Vec<u8>,
    generation: u64,
}

impl JwksEndpointReadBack {
    /// Constructs bounded endpoint evidence after the owner fetched a response.
    pub fn new(
        uri: impl Into<String>,
        origin: impl Into<String>,
        bytes: Vec<u8>,
        generation: u64,
    ) -> Result<Self, JwsSignerConfigurationError> {
        let uri = uri.into();
        let origin = origin.into();
        if generation == 0 || uri.is_empty() || origin.is_empty() || bytes.is_empty() {
            return Err(JwsSignerConfigurationError::InvalidActivationEvidence);
        }
        if uri.len() > MAX_JWKS_BYTES
            || origin.len() > MAX_JWKS_BYTES
            || bytes.len() > MAX_JWKS_BYTES
        {
            return Err(JwsSignerConfigurationError::InvalidActivationEvidence);
        }
        Ok(Self {
            uri,
            origin,
            bytes,
            generation,
        })
    }

    /// Exact configured endpoint URI fetched by the verifier.
    #[must_use]
    pub fn uri(&self) -> &str {
        &self.uri
    }

    /// Exact origin asserted by the verifier for this endpoint.
    #[must_use]
    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// Monotonic publication generation observed by the verifier.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }
}

/// Sealed evidence that every advertised JWKS endpoint returned one exact
/// key-ring generation and verified a signer-produced canary.
#[derive(Clone)]
pub struct SigningActivationReceipt {
    profile: SigningActivationProfile,
    issuer: String,
    key_ring_generation: u64,
    endpoints: Vec<JwksEndpointReadBack>,
    canary: String,
}

impl SigningActivationReceipt {
    /// Verifies endpoint evidence and the compact canary before minting a
    /// receipt.  The caller must provide every advertised endpoint; copied
    /// bytes fail unless they are the exact fetched response recorded here.
    pub fn verify(
        profile: SigningActivationProfile,
        issuer: impl Into<String>,
        expected: &CanonicalRs256PublicJwksSet,
        configured_endpoints: &[String],
        configured_origins: &[String],
        endpoints: Vec<JwksEndpointReadBack>,
        canary: String,
    ) -> Result<Self, JoseError> {
        let issuer = issuer.into();
        if issuer.is_empty()
            || endpoints.is_empty()
            || endpoints.len() != configured_endpoints.len()
            || endpoints.len() != configured_origins.len()
            || canary.is_empty()
        {
            return Err(JoseError::InvalidJwk("activation receipt evidence"));
        }
        let mut seen = BTreeSet::new();
        for endpoint in &endpoints {
            if endpoint.generation != expected.generation
                || endpoint.bytes != expected.bytes
                || !seen.insert(endpoint.uri.as_str())
                || !configured_endpoints
                    .iter()
                    .zip(configured_origins)
                    .any(|(uri, origin)| uri == endpoint.uri() && origin == endpoint.origin())
            {
                return Err(JoseError::InvalidJwk(
                    "activation receipt endpoint mismatch",
                ));
            }
            let keys = AdmittedRsaJwks::from_json(&endpoint.bytes)?;
            verify_compact_jws_rs256(&canary, &keys)?;
        }
        Ok(Self {
            profile,
            issuer,
            key_ring_generation: expected.generation,
            endpoints,
            canary,
        })
    }

    /// The profile whose signing admission this receipt authorizes.
    #[must_use]
    pub const fn profile(&self) -> SigningActivationProfile {
        self.profile
    }

    /// Exact issuer identity bound to the receipt.
    #[must_use]
    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    /// Exact key-ring generation admitted by the endpoint observations.
    #[must_use]
    pub const fn key_ring_generation(&self) -> u64 {
        self.key_ring_generation
    }

    /// Returns the observed endpoint count without exposing mutable evidence.
    #[must_use]
    pub fn endpoint_count(&self) -> usize {
        self.endpoints.len()
    }

    /// Confirms this receipt remains exactly applicable to a selected profile,
    /// issuer, canonical key-ring, and configured endpoint list.
    pub fn applies_to(
        &self,
        profile: SigningActivationProfile,
        issuer: &str,
        expected: &CanonicalRs256PublicJwksSet,
        endpoints: &[String],
        origins: &[String],
    ) -> bool {
        if self.profile != profile
            || self.issuer != issuer
            || self.key_ring_generation != expected.generation
            || self.endpoints.len() != endpoints.len()
            || self.endpoints.len() != origins.len()
        {
            return false;
        }
        self.endpoints.iter().all(|observed| {
            endpoints
                .iter()
                .zip(origins)
                .any(|(endpoint, origin)| endpoint == &observed.uri && origin == &observed.origin)
                && observed.bytes == expected.bytes
                && observed.generation == expected.generation
        })
    }
}

impl fmt::Debug for SigningActivationReceipt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SigningActivationReceipt")
            .field("profile", &self.profile)
            .field("issuer_bytes", &self.issuer.len())
            .field("key_ring_generation", &self.key_ring_generation)
            .field("endpoint_count", &self.endpoints.len())
            .field("canary_bytes", &self.canary.len())
            .finish()
    }
}

impl fmt::Debug for CanonicalRs256PublicJwks {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CanonicalRs256PublicJwks")
            .field("bytes", &self.bytes.len())
            .field("binding", &self.binding)
            .finish()
    }
}

impl fmt::Debug for AttestedRs256PublicKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AttestedRs256PublicKey")
            .field("kid", &self.kid)
            .field("modulus_bytes", &self.modulus.len())
            .field("binding", &self.binding)
            .field("provenance", &self.provenance)
            .finish()
    }
}

/// A finite bound that the external signer must apply to its one operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExternalRs256SigningDeadline(Duration);

impl ExternalRs256SigningDeadline {
    /// Admits a positive bounded operation deadline.
    pub fn new(duration: Duration) -> Result<Self, JwsSignerConfigurationError> {
        if duration.is_zero() || duration > MAX_EXTERNAL_RS256_SIGNING_DEADLINE {
            return Err(JwsSignerConfigurationError::InvalidDeadline);
        }
        Ok(Self(duration))
    }

    /// Returns the finite duration delegated to the external signer.
    #[must_use]
    pub const fn duration(self) -> Duration {
        self.0
    }
}

/// Framework-admitted JSON claims retained in zeroizing storage.
///
/// A caller may select neither protected-header fields nor a compact JWS.
/// This type exists so a not-dispatched operation can return its exact claim
/// bytes for one explicit retry, while dispatched or unknown work consumes it.
pub struct BoundedJwsClaims {
    bytes: Zeroizing<Vec<u8>>,
}

impl BoundedJwsClaims {
    /// Serializes one framework-constructed claims object under the fixed bound.
    pub fn from_value(claims: &Value) -> Result<Self, JwsSignerConfigurationError> {
        if !claims.is_object() {
            return Err(JwsSignerConfigurationError::InvalidClaims);
        }
        let bytes =
            serde_json::to_vec(claims).map_err(|_| JwsSignerConfigurationError::InvalidClaims)?;
        Self::from_json_bytes(&bytes)
    }

    /// Admits exact framework-produced JSON claim bytes for a signed vector or
    /// byte-preserving higher-layer serializer.
    pub fn from_json_bytes(bytes: &[u8]) -> Result<Self, JwsSignerConfigurationError> {
        if bytes.is_empty() || bytes.len() > MAX_JWS_CLAIMS_BYTES {
            return Err(JwsSignerConfigurationError::InvalidClaims);
        }
        reject_duplicate_json_members(bytes)
            .map_err(|_| JwsSignerConfigurationError::InvalidClaims)?;
        let claims =
            parse_json(bytes, "claims").map_err(|_| JwsSignerConfigurationError::InvalidClaims)?;
        if !claims.is_object() {
            return Err(JwsSignerConfigurationError::InvalidClaims);
        }
        Ok(Self {
            bytes: Zeroizing::new(bytes.to_vec()),
        })
    }

    fn as_bytes(&self) -> &[u8] {
        self.bytes.as_slice()
    }
}

impl fmt::Debug for BoundedJwsClaims {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BoundedJwsClaims")
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

/// Closed protected-header profiles supported by this signer facade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JwsSigningProfile {
    /// An RFC 9068 access-token candidate; emits the fixed `typ=at+jwt`.
    BuiltinAccessToken,
    /// An OpenID Connect ID-token candidate; emits no optional JOSE `typ`.
    OidcIdToken,
    /// An RFC 7523 client-assertion candidate; emits no JOSE `typ` header.
    ClientAssertion,
}

/// A bounded canonical signing input held in zeroizing memory.
///
/// It is intentionally non-Clone and exposes bytes only for the duration of a
/// backend-owned closure. The facade alone forms this exact
/// `base64url(header) + "." + base64url(claims)` value.
pub struct CanonicalRs256SigningInput {
    bytes: Zeroizing<Vec<u8>>,
}

impl CanonicalRs256SigningInput {
    /// Supplies the canonical signing bytes to an external backend without a
    /// reusable public byte accessor.
    pub fn with_bytes<R>(&self, read: impl FnOnce(&[u8]) -> R) -> R {
        read(self.bytes.as_slice())
    }
}

impl fmt::Debug for CanonicalRs256SigningInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CanonicalRs256SigningInput")
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

/// One non-Clone request delivered to an external RS256 signer backend.
pub struct ExternalRs256SigningRequest {
    input: CanonicalRs256SigningInput,
    binding: Rs256SigningBinding,
    deadline: ExternalRs256SigningDeadline,
}

impl ExternalRs256SigningRequest {
    /// Borrows the facade-constructed input for the duration of backend work.
    #[must_use]
    pub fn input(&self) -> &CanonicalRs256SigningInput {
        &self.input
    }

    /// Returns the generations the backend must attest in its receipt.
    #[must_use]
    pub const fn binding(&self) -> Rs256SigningBinding {
        self.binding
    }

    /// Returns the finite operation deadline the backend must enforce.
    #[must_use]
    pub const fn deadline(&self) -> ExternalRs256SigningDeadline {
        self.deadline
    }
}

impl fmt::Debug for ExternalRs256SigningRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExternalRs256SigningRequest")
            .field("input", &self.input)
            .field("binding", &self.binding)
            .field("deadline", &self.deadline)
            .finish()
    }
}

/// A bounded raw RS256 signature returned by an external provider.
pub struct RawRs256Signature {
    bytes: Vec<u8>,
}

impl RawRs256Signature {
    /// Admits one bounded raw signature; its exact modulus-width check belongs
    /// to the facade after it selects the attested public key.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, JwsSignerConfigurationError> {
        if bytes.is_empty() || bytes.len() > MAX_ADMITTED_RSA_MODULUS_BYTES {
            return Err(JwsSignerConfigurationError::InvalidSignature);
        }
        Ok(Self { bytes })
    }
}

impl fmt::Debug for RawRs256Signature {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RawRs256Signature")
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

/// Opaque nonsecret evidence of one external signer operation.
pub struct ExternalRs256OperationReceipt {
    binding: Rs256SigningBinding,
    operation: u64,
    provenance: RedactedSignerProvenance,
}

impl ExternalRs256OperationReceipt {
    /// Builds a nonsecret receipt that binds the provider result to one exact
    /// immutable generation tuple.
    pub fn new(
        binding: Rs256SigningBinding,
        operation: u64,
        provenance: RedactedSignerProvenance,
    ) -> Result<Self, JwsSignerConfigurationError> {
        if operation == 0 {
            return Err(JwsSignerConfigurationError::ZeroOperation);
        }
        Ok(Self {
            binding,
            operation,
            provenance,
        })
    }

    fn matches(&self, binding: Rs256SigningBinding) -> bool {
        self.binding == binding
    }
}

impl fmt::Debug for ExternalRs256OperationReceipt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExternalRs256OperationReceipt")
            .field("binding", &self.binding)
            .field("operation", &self.operation)
            .field("provenance", &self.provenance)
            .finish()
    }
}

/// Typed external-provider dispatch knowledge for exactly one signing call.
pub enum ExternalRs256SignDisposition {
    /// The provider proves it did not dispatch; the facade may return claims
    /// for one explicit retry.
    NotDispatched(ExternalRs256OperationReceipt),
    /// The provider dispatched and returned a raw RS256 signature plus receipt.
    Dispatched(RawRs256Signature, ExternalRs256OperationReceipt),
    /// Provider dispatch knowledge is ambiguous; the facade must consume the
    /// candidate and forbid automatic retry.
    Unknown(ExternalRs256OperationReceipt),
}

impl fmt::Debug for ExternalRs256SignDisposition {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotDispatched(receipt) => formatter
                .debug_tuple("ExternalRs256SignDisposition::NotDispatched")
                .field(receipt)
                .finish(),
            Self::Dispatched(signature, receipt) => formatter
                .debug_tuple("ExternalRs256SignDisposition::Dispatched")
                .field(signature)
                .field(receipt)
                .finish(),
            Self::Unknown(receipt) => formatter
                .debug_tuple("ExternalRs256SignDisposition::Unknown")
                .field(receipt)
                .finish(),
        }
    }
}

/// The sole public extension point for a remote KMS/HSM RS256 adapter.
///
/// A conforming backend owns the opaque provider/key handle and its bounded
/// queue, socket, timeout, cancellation, and cleanup behavior. It can neither
/// select JOSE fields nor return a compact JWS or private key.
pub trait ExternalRs256SignerBackend: Send + Sync + 'static {
    /// Performs exactly one externally custodied signing attempt.
    fn sign<'a>(
        &'a self,
        cx: &'a fastmcp_core::Cx,
        request: ExternalRs256SigningRequest,
    ) -> Pin<Box<dyn Future<Output = ExternalRs256SignDisposition> + Send + 'a>>;
}

/// Configuration refusal while constructing the protocol-only signer facade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JwsSignerConfigurationError {
    /// Every immutable generation must be nonzero.
    ZeroGeneration,
    /// The redacted provider label was empty, too large, or contained control data.
    InvalidProvenance,
    /// The public key identifier was empty or exceeded its bound.
    InvalidKeyId,
    /// The public modulus was not one canonical FND-09 RS256 size.
    InvalidPublicKey,
    /// The deadline was zero or exceeded the fixed maximum.
    InvalidDeadline,
    /// Claims were not one bounded JSON object with unique members.
    InvalidClaims,
    /// A raw signature exceeded the protocol hard bound.
    InvalidSignature,
    /// The provider supplied no operation identifier.
    ZeroOperation,
    /// Endpoint evidence was absent, oversized, or not generation-bound.
    InvalidActivationEvidence,
    /// A key remains needed by a live artifact or the requested generation regressed.
    RetirementNotPermitted,
}

impl fmt::Display for JwsSignerConfigurationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ZeroGeneration => "RS256 signer generations must be nonzero",
            Self::InvalidProvenance => "RS256 signer provenance is not a bounded redacted label",
            Self::InvalidKeyId => "RS256 signer key identifier is invalid",
            Self::InvalidPublicKey => "RS256 signer public key is invalid",
            Self::InvalidDeadline => "RS256 signer deadline is invalid",
            Self::InvalidClaims => "RS256 signer claims are invalid",
            Self::InvalidSignature => "RS256 signer signature is invalid",
            Self::ZeroOperation => "RS256 signer operation identifier must be nonzero",
            Self::InvalidActivationEvidence => {
                "RS256 signing activation evidence is invalid or unbounded"
            }
            Self::RetirementNotPermitted => {
                "RS256 verification key retirement is fenced by live artifacts or generation"
            }
        })
    }
}

impl std::error::Error for JwsSignerConfigurationError {}

/// A sign attempt that did not produce an exposable compact JWS candidate.
pub enum JwsSigningError {
    /// Cancellation was already visible before backend dispatch; callers may
    /// recover the framework claims and retry under a fresh request context.
    CancelledBeforeDispatch(BoundedJwsClaims),
    /// The backend proved no dispatch occurred; callers may recover claims for
    /// an explicit retry. No automatic retry is performed by this facade.
    NotDispatched {
        claims: BoundedJwsClaims,
        receipt: ExternalRs256OperationReceipt,
    },
    /// The backend could not establish dispatch knowledge; the claim candidate
    /// is deliberately consumed and may not be retried automatically.
    Unknown(ExternalRs256OperationReceipt),
    /// Cancellation won after backend dispatch began or a late result arrived.
    /// The candidate is consumed even if the late signature was valid.
    CancelledAfterDispatch(ExternalRs256OperationReceipt),
    /// The backend receipt did not bind to the admitted key/ring/configuration.
    ReceiptMismatch,
    /// A dispatched signature did not have the selected RSA modulus width.
    SignatureLengthMismatch,
    /// The constructed compact JWS failed strict local admission or RS256 verification.
    SelfVerificationFailed,
}

impl JwsSigningError {
    /// Recovers claims only for outcomes that prove no external dispatch.
    #[must_use]
    pub fn into_reusable_claims(self) -> Option<BoundedJwsClaims> {
        match self {
            Self::CancelledBeforeDispatch(claims) | Self::NotDispatched { claims, .. } => {
                Some(claims)
            }
            Self::Unknown(_)
            | Self::CancelledAfterDispatch(_)
            | Self::ReceiptMismatch
            | Self::SignatureLengthMismatch
            | Self::SelfVerificationFailed => None,
        }
    }
}

impl fmt::Debug for JwsSigningError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CancelledBeforeDispatch(claims) => formatter
                .debug_tuple("JwsSigningError::CancelledBeforeDispatch")
                .field(claims)
                .finish(),
            Self::NotDispatched { claims, receipt } => formatter
                .debug_struct("JwsSigningError::NotDispatched")
                .field("claims", claims)
                .field("receipt", receipt)
                .finish(),
            Self::Unknown(receipt) => formatter
                .debug_tuple("JwsSigningError::Unknown")
                .field(receipt)
                .finish(),
            Self::CancelledAfterDispatch(receipt) => formatter
                .debug_tuple("JwsSigningError::CancelledAfterDispatch")
                .field(receipt)
                .finish(),
            Self::ReceiptMismatch => formatter.write_str("JwsSigningError::ReceiptMismatch"),
            Self::SignatureLengthMismatch => {
                formatter.write_str("JwsSigningError::SignatureLengthMismatch")
            }
            Self::SelfVerificationFailed => {
                formatter.write_str("JwsSigningError::SelfVerificationFailed")
            }
        }
    }
}

impl fmt::Display for JwsSigningError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::CancelledBeforeDispatch(_) => "RS256 signing cancelled before dispatch",
            Self::NotDispatched { .. } => "RS256 signing was not dispatched",
            Self::Unknown(_) => "RS256 signing dispatch state is unknown",
            Self::CancelledAfterDispatch(_) => "RS256 signing cancelled after dispatch",
            Self::ReceiptMismatch => "RS256 signer receipt did not match admitted generations",
            Self::SignatureLengthMismatch => {
                "RS256 signer returned a signature with the wrong length"
            }
            Self::SelfVerificationFailed => "RS256 signer result failed local self-verification",
        })
    }
}

impl std::error::Error for JwsSigningError {}

/// A compact JWS candidate that was strictly admitted and self-verified.
///
/// This is not proof of JWKS publication, issuer activation, consumer-store
/// commit, or authorization-token issuance. Higher layers must supply those
/// separate fenced transitions before exposing the compact value on the wire.
pub struct SelfVerifiedRs256Jws {
    compact: String,
    binding: Rs256SigningBinding,
}

impl SelfVerifiedRs256Jws {
    /// Returns the generation binding that the local verification accepted.
    #[must_use]
    pub const fn binding(&self) -> Rs256SigningBinding {
        self.binding
    }

    /// Consumes the protected candidate for a later fenced consumer commit.
    #[must_use]
    pub fn into_compact_jws(self) -> String {
        self.compact
    }
}

impl fmt::Debug for SelfVerifiedRs256Jws {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SelfVerifiedRs256Jws")
            .field("compact_bytes", &self.compact.len())
            .field("binding", &self.binding)
            .finish()
    }
}

/// The protocol-only external RS256 signer facade.
pub struct ExternalRs256Signer {
    backend: Arc<dyn ExternalRs256SignerBackend>,
    public_key: AttestedRs256PublicKey,
}

impl ExternalRs256Signer {
    /// Constructs a facade from one external backend and one admitted public
    /// key binding. This performs no publication, health probe, or signing.
    #[must_use]
    pub fn new(
        backend: Arc<dyn ExternalRs256SignerBackend>,
        public_key: AttestedRs256PublicKey,
    ) -> Self {
        Self {
            backend,
            public_key,
        }
    }

    /// Returns the immutable key/ring/configuration binding selected at admission.
    #[must_use]
    pub const fn binding(&self) -> Rs256SigningBinding {
        self.public_key.binding()
    }

    /// Returns the exact public key identifier selected for this signer.
    #[must_use]
    pub fn key_id(&self) -> &str {
        self.public_key.kid()
    }

    /// Returns the sole canonical public JWKS that can activate this signer.
    pub fn canonical_public_jwks(&self) -> Result<CanonicalRs256PublicJwks, JoseError> {
        self.public_key.canonical_public_jwks()
    }

    /// Signs one bounded framework candidate through the sealed facade.
    pub async fn sign(
        &self,
        cx: &fastmcp_core::Cx,
        profile: JwsSigningProfile,
        claims: BoundedJwsClaims,
        deadline: ExternalRs256SigningDeadline,
    ) -> Result<SelfVerifiedRs256Jws, JwsSigningError> {
        if cx.is_cancel_requested() {
            return Err(JwsSigningError::CancelledBeforeDispatch(claims));
        }

        let header = protected_header(profile, self.public_key.kid())
            .map_err(|_| JwsSigningError::SelfVerificationFailed)?;
        let header_segment = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(header);
        let claims_segment =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.as_bytes());
        let signing_input = format!("{header_segment}.{claims_segment}");
        if signing_input.len() > MAX_COMPACT_JWS_BYTES {
            return Err(JwsSigningError::SelfVerificationFailed);
        }

        let binding = self.public_key.binding();
        let request = ExternalRs256SigningRequest {
            input: CanonicalRs256SigningInput {
                bytes: Zeroizing::new(signing_input.into_bytes()),
            },
            binding,
            deadline,
        };
        let disposition = self.backend.sign(cx, request).await;

        match disposition {
            ExternalRs256SignDisposition::NotDispatched(receipt) => {
                if !receipt.matches(binding) {
                    return Err(JwsSigningError::ReceiptMismatch);
                }
                if cx.is_cancel_requested() {
                    return Err(JwsSigningError::CancelledBeforeDispatch(claims));
                }
                Err(JwsSigningError::NotDispatched { claims, receipt })
            }
            ExternalRs256SignDisposition::Unknown(receipt) => {
                if !receipt.matches(binding) {
                    return Err(JwsSigningError::ReceiptMismatch);
                }
                Err(JwsSigningError::Unknown(receipt))
            }
            ExternalRs256SignDisposition::Dispatched(signature, receipt) => {
                if !receipt.matches(binding) {
                    return Err(JwsSigningError::ReceiptMismatch);
                }
                if cx.is_cancel_requested() {
                    return Err(JwsSigningError::CancelledAfterDispatch(receipt));
                }
                if signature.bytes.len() != self.public_key.modulus.len() {
                    return Err(JwsSigningError::SignatureLengthMismatch);
                }
                let compact = format!(
                    "{header_segment}.{claims_segment}.{}",
                    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature.bytes)
                );
                if compact.len() > MAX_COMPACT_JWS_BYTES
                    || verify_compact_jws_rs256(&compact, &self.public_key.admitted_jwks()).is_err()
                {
                    return Err(JwsSigningError::SelfVerificationFailed);
                }
                Ok(SelfVerifiedRs256Jws { compact, binding })
            }
        }
    }
}

fn protected_header(profile: JwsSigningProfile, kid: &str) -> Result<Vec<u8>, JoseError> {
    let header = match profile {
        JwsSigningProfile::BuiltinAccessToken => {
            serde_json::json!({"alg": "RS256", "kid": kid, "typ": "at+jwt"})
        }
        JwsSigningProfile::OidcIdToken | JwsSigningProfile::ClientAssertion => {
            serde_json::json!({"alg": "RS256", "kid": kid})
        }
    };
    let header = serde_json::to_vec(&header).map_err(|_| JoseError::InvalidJson("header"))?;
    if header.len() > MAX_JWS_HEADER_BYTES {
        return Err(JoseError::TooLarge("header"));
    }
    Ok(header)
}

/// Verify a compact JWS with an already-admitted local RSA JWK Set.
///
/// Only RSASSA-PKCS1-v1_5 with SHA-256 (`RS256`) is accepted. This function
/// verifies integrity only; callers must apply issuer, audience, lifetime,
/// replay, and authorization policy to the returned claims.
pub fn verify_compact_jws_rs256(
    compact_jws: &str,
    admitted_keys: &AdmittedRsaJwks,
) -> Result<VerifiedCompactJws, JoseError> {
    if compact_jws.len() > MAX_COMPACT_JWS_BYTES {
        return Err(JoseError::TooLarge("compact JWS"));
    }
    let (header_segment, claims_segment, signature_segment, signing_input) =
        split_compact_jws(compact_jws)?;
    let header_bytes = decode_base64url(header_segment, MAX_JWS_HEADER_BYTES, "header")?;
    let claims_bytes = decode_base64url(claims_segment, MAX_JWS_CLAIMS_BYTES, "claims")?;
    let header = admit_header(&header_bytes)?;
    reject_duplicate_json_members(&claims_bytes)?;
    let claims = parse_json(&claims_bytes, "claims")?;
    if !claims.is_object() {
        return Err(JoseError::InvalidJson("claims object"));
    }

    let key = admitted_keys
        .keys
        .get(header.kid())
        .ok_or(JoseError::UnknownKeyId)?;
    let signature = decode_base64url(signature_segment, MAX_JWS_SIGNATURE_BYTES, "signature")?;
    if signature.len() != key.modulus.len() {
        return Err(JoseError::InvalidSignature);
    }
    RsaPublicKeyComponents {
        n: &key.modulus,
        e: &key.exponent,
    }
    .verify(
        &RSA_PKCS1_2048_8192_SHA256,
        signing_input.as_bytes(),
        &signature,
    )
    .map_err(|_| JoseError::InvalidSignature)?;

    Ok(VerifiedCompactJws { header, claims })
}

fn split_compact_jws(compact: &str) -> Result<(&str, &str, &str, &str), JoseError> {
    let first_dot = compact
        .find('.')
        .ok_or(JoseError::InvalidCompactSerialization)?;
    let second_dot = compact[first_dot + 1..]
        .find('.')
        .map(|offset| first_dot + 1 + offset)
        .ok_or(JoseError::InvalidCompactSerialization)?;
    if compact[second_dot + 1..].contains('.') {
        return Err(JoseError::InvalidCompactSerialization);
    }
    let header = &compact[..first_dot];
    let claims = &compact[first_dot + 1..second_dot];
    let signature = &compact[second_dot + 1..];
    if header.is_empty() || claims.is_empty() || signature.is_empty() {
        return Err(JoseError::InvalidCompactSerialization);
    }
    Ok((header, claims, signature, &compact[..second_dot]))
}

fn admit_header(header: &[u8]) -> Result<VerifiedRs256Header, JoseError> {
    reject_duplicate_json_members(header)?;
    let header = parse_json(header, "protected header")?;
    let header = header
        .as_object()
        .ok_or(JoseError::InvalidJson("protected header object"))?;
    if header.get("alg").and_then(Value::as_str) != Some("RS256") {
        return Err(JoseError::UnsupportedAlgorithm);
    }
    for name in ["jku", "jwk", "x5u", "x5c", "crit", "b64"] {
        if header.contains_key(name) {
            return Err(JoseError::DisallowedHeader(name));
        }
    }
    let kid = header
        .get("kid")
        .and_then(Value::as_str)
        .filter(|kid| !kid.is_empty() && kid.len() <= MAX_KID_BYTES)
        .ok_or(JoseError::MissingKeyId)?;
    Ok(VerifiedRs256Header {
        kid: kid.to_owned(),
    })
}

fn admit_rsa_jwk(value: &Value) -> Result<(String, AdmittedRsaJwk), JoseError> {
    let key = value
        .as_object()
        .ok_or(JoseError::InvalidJwk("JWK must be an object"))?;
    reject_remote_key_reference(key)?;
    if key.get("kty").and_then(Value::as_str) != Some("RSA") {
        return Err(JoseError::InvalidJwk("only RSA keys are admitted"));
    }
    if let Some(algorithm) = key.get("alg") {
        if algorithm.as_str() != Some("RS256") {
            return Err(JoseError::InvalidJwk("JWK alg must be RS256 when present"));
        }
    }
    if key.contains_key("k") {
        return Err(JoseError::InvalidJwk("symmetric key material"));
    }
    for private_member in ["d", "p", "q", "dp", "dq", "qi", "oth"] {
        if key.contains_key(private_member) {
            return Err(JoseError::InvalidJwk("private key material"));
        }
    }
    if let Some(use_) = key.get("use") {
        if use_.as_str() != Some("sig") {
            return Err(JoseError::InvalidJwk("JWK use must be sig"));
        }
    }
    if let Some(key_ops) = key.get("key_ops") {
        let key_ops = key_ops
            .as_array()
            .ok_or(JoseError::InvalidJwk("JWK key_ops"))?;
        if key_ops.len() != 1 || key_ops.first().and_then(Value::as_str) != Some("verify") {
            return Err(JoseError::InvalidJwk("JWK key_ops must be verify"));
        }
    }

    let kid = key
        .get("kid")
        .and_then(Value::as_str)
        .filter(|kid| !kid.is_empty() && kid.len() <= MAX_KID_BYTES)
        .ok_or(JoseError::InvalidJwk("missing or invalid kid"))?;
    let modulus = decode_jwk_component(key, "n", MAX_ADMITTED_RSA_MODULUS_BYTES)?;
    if modulus.first() == Some(&0) {
        return Err(JoseError::InvalidJwk("non-canonical modulus"));
    }
    if modulus.first().is_none_or(|first| first & 0x80 == 0) {
        return Err(JoseError::InvalidJwk("non-exact RSA modulus bit length"));
    }
    if !ADMITTED_RSA_MODULUS_BYTE_LENGTHS.contains(&modulus.len()) {
        return Err(JoseError::InvalidJwk("unsupported RSA modulus length"));
    }
    let exponent = decode_jwk_component(key, "e", ADMITTED_RSA_PUBLIC_EXPONENT.len())?;
    if exponent.as_slice() != ADMITTED_RSA_PUBLIC_EXPONENT.as_slice() {
        return Err(JoseError::InvalidJwk("invalid exponent"));
    }
    Ok((kid.to_owned(), AdmittedRsaJwk { modulus, exponent }))
}

fn reject_remote_key_reference(object: &Map<String, Value>) -> Result<(), JoseError> {
    for name in ["jku", "x5u"] {
        if object.contains_key(name) {
            return Err(JoseError::InvalidJwk("remote key reference"));
        }
    }
    Ok(())
}

fn decode_jwk_component(
    key: &Map<String, Value>,
    name: &'static str,
    max_bytes: usize,
) -> Result<Vec<u8>, JoseError> {
    let encoded = key
        .get(name)
        .and_then(Value::as_str)
        .ok_or(JoseError::InvalidJwk("missing JWK component"))?;
    let decoded = decode_base64url(encoded, max_bytes, "JWK component")
        .map_err(|_| JoseError::InvalidJwk("invalid JWK component"))?;
    Ok(decoded)
}

fn decode_base64url(
    encoded: &str,
    max_decoded_bytes: usize,
    part: &'static str,
) -> Result<Vec<u8>, JoseError> {
    if encoded.len() > base64url_max_encoded_len(max_decoded_bytes)
        || encoded.is_empty()
        || !encoded.bytes().all(is_unpadded_base64url_byte)
    {
        return Err(JoseError::InvalidBase64Url(part));
    }
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| JoseError::InvalidBase64Url(part))?;
    if decoded.len() > max_decoded_bytes {
        return Err(JoseError::TooLarge(part));
    }
    if base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&decoded) != encoded {
        return Err(JoseError::InvalidBase64Url(part));
    }
    Ok(decoded)
}

fn base64url_max_encoded_len(decoded_bytes: usize) -> usize {
    decoded_bytes.saturating_mul(4).div_ceil(3)
}

fn is_unpadded_base64url_byte(byte: u8) -> bool {
    byte.is_ascii_uppercase()
        || byte.is_ascii_lowercase()
        || byte.is_ascii_digit()
        || matches!(byte, b'-' | b'_')
}

fn parse_json(input: &[u8], part: &'static str) -> Result<Value, JoseError> {
    serde_json::from_slice(input).map_err(|_| JoseError::InvalidJson(part))
}

/// Reject duplicate members before `serde_json` can collapse them.
fn reject_duplicate_json_members(input: &[u8]) -> Result<(), JoseError> {
    let mut scanner = JsonMemberScanner::new(input);
    scanner.skip_whitespace();
    scanner.scan_value(0)?;
    scanner.skip_whitespace();
    if scanner.index != input.len() {
        return Err(JoseError::InvalidJson("trailing data"));
    }
    Ok(())
}

struct JsonMemberScanner<'a> {
    input: &'a [u8],
    index: usize,
    members: usize,
}

impl<'a> JsonMemberScanner<'a> {
    const fn new(input: &'a [u8]) -> Self {
        Self {
            input,
            index: 0,
            members: 0,
        }
    }

    fn scan_value(&mut self, nesting: usize) -> Result<(), JoseError> {
        if nesting > MAX_JSON_NESTING {
            return Err(JoseError::TooLarge("JSON nesting"));
        }
        self.skip_whitespace();
        match self.peek() {
            Some(b'{') => self.scan_object(nesting + 1),
            Some(b'[') => self.scan_array(nesting + 1),
            Some(b'"') => self.scan_string().map(|_| ()),
            Some(_) => self.scan_atom(),
            None => Err(JoseError::InvalidJson("value")),
        }
    }

    fn scan_object(&mut self, nesting: usize) -> Result<(), JoseError> {
        self.index += 1;
        self.skip_whitespace();
        if self.consume(b'}') {
            return Ok(());
        }
        let mut names = BTreeSet::new();
        loop {
            self.skip_whitespace();
            let name = self.scan_string()?;
            self.members += 1;
            if self.members > MAX_JSON_MEMBERS {
                return Err(JoseError::TooLarge("JSON members"));
            }
            if !names.insert(name) {
                return Err(JoseError::DuplicateJsonMember);
            }
            self.skip_whitespace();
            if !self.consume(b':') {
                return Err(JoseError::InvalidJson("object member"));
            }
            self.scan_value(nesting)?;
            self.skip_whitespace();
            if self.consume(b'}') {
                return Ok(());
            }
            if !self.consume(b',') {
                return Err(JoseError::InvalidJson("object separator"));
            }
        }
    }

    fn scan_array(&mut self, nesting: usize) -> Result<(), JoseError> {
        self.index += 1;
        self.skip_whitespace();
        if self.consume(b']') {
            return Ok(());
        }
        loop {
            self.scan_value(nesting)?;
            self.skip_whitespace();
            if self.consume(b']') {
                return Ok(());
            }
            if !self.consume(b',') {
                return Err(JoseError::InvalidJson("array separator"));
            }
        }
    }

    fn scan_string(&mut self) -> Result<String, JoseError> {
        let start = self.index;
        if !self.consume(b'"') {
            return Err(JoseError::InvalidJson("string"));
        }
        while let Some(byte) = self.peek() {
            match byte {
                b'"' => {
                    self.index += 1;
                    return serde_json::from_slice(&self.input[start..self.index])
                        .map_err(|_| JoseError::InvalidJson("string"));
                }
                b'\\' => {
                    self.index += 1;
                    let escaped = self.peek().ok_or(JoseError::InvalidJson("string"))?;
                    self.index += 1;
                    if escaped == b'u' {
                        for _ in 0..4 {
                            if !self.peek().is_some_and(|hex| hex.is_ascii_hexdigit()) {
                                return Err(JoseError::InvalidJson("string escape"));
                            }
                            self.index += 1;
                        }
                    }
                }
                0..=0x1f => return Err(JoseError::InvalidJson("string")),
                _ => self.index += 1,
            }
        }
        Err(JoseError::InvalidJson("unterminated string"))
    }

    fn scan_atom(&mut self) -> Result<(), JoseError> {
        let start = self.index;
        while self
            .peek()
            .is_some_and(|byte| !byte.is_ascii_whitespace() && !matches!(byte, b',' | b']' | b'}'))
        {
            self.index += 1;
        }
        if self.index == start {
            return Err(JoseError::InvalidJson("atom"));
        }
        Ok(())
    }

    fn skip_whitespace(&mut self) {
        while self.peek().is_some_and(|byte| byte.is_ascii_whitespace()) {
            self.index += 1;
        }
    }

    fn consume(&mut self, expected: u8) -> bool {
        if self.peek() == Some(expected) {
            self.index += 1;
            true
        } else {
            false
        }
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.index).copied()
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use fastmcp_core::block_on;

    use super::*;

    const FIXED_JWK: &str = concat!(
        r#"{"keys":[{"kty":"RSA","kid":"fixed-rs256","alg":"RS256","use":"sig","n":""#,
        "jlHZ9nzuIuM4aiAQSAgEJMBaYS7qm7Z_3mtGYDdzReIkzxPHHr21oeXQyUJI89eQG13fsUdyoodcuh5kmndPCrODJekfr_zgor6sNspcB88iQEqEc9yf9YAf5v-cNH1Evh82KABuWb26LMaNAzZFR3BMhMEQ1FD6fLFGAbX76Drd5_UZ-1xcU07IXEc_9zvQvOwXckhO7P5Yil1fVzLTrHye_6zTbGWvdqi45095bKPnSqjrLBCTVrUW8o02Gi6mt7Ls9pZeWx2DXV8SqV06DdlqiovtKWRooQ1zV-v7BGsLsVk6T6d-8mNMGNrh0fpNb_5kdaHphAt_Ji6eE1wQPw",
        r#"","e":"AQAB"}]}"#
    );
    const FIXED_COMPACT_JWS: &str = concat!(
        "eyJhbGciOiJSUzI1NiIsImtpZCI6ImZpeGVkLXJzMjU2In0.",
        "eyJzdWIiOiJmaXhlZC12ZWN0b3IiLCJhdWQiOiJzZXJ2ZXItcG9saWN5LWxhdGVyIn0.",
        "Oak9UDEtrL-pNcPIFw31uzuCoCTyXywF5i3jxDixd0gHonZYPFfSlyPwhNSTrqmlzPsL-wNFcDn1zFlug6Ae1vK_QaL-bZBSxq-lOrMDUI_5_3P_HUrngtZaNk8ru88-wdGByGm1jRZa-LfeoSkESHVKPIcQ_WT7wqhq1RX3ZrPiq9QkHFE8nWIgiIesu8DFOXsdN05rmOxHheCbDGRpf8cQAG0ZENpJvYugD-SX9Sg9Kds5HOlOt6csIQBexCeKM2rIrN0r7qCp6jx_0aevqU6rNr6oxCxCGoH3UZGJa5xRh2KeJ6NVBE9BpPW3Kdi3dEfKlKldjzlUW-zEREdeEw"
    );
    const FIXED_CLAIMS: &str = r#"{"sub":"fixed-vector","aud":"server-policy-later"}"#;

    // Generated once with OpenSSL 3.6.3 (9 Jun 2026): `openssl genpkey`
    // kept the private key only in a shell variable, and `openssl dgst
    // -sha256 -sign <(printf ...)` signed the canonical input. Only this
    // public JWK and compact vector are retained; no private material exists
    // in the repository or test output.
    const BUILTIN_ACCESS_TOKEN_JWK: &str = concat!(
        r#"{"keys":[{"kty":"RSA","kid":"builtin-rs256","alg":"RS256","use":"sig","n":""#,
        "26SPJmT9g7acaNN5sr1S8wsJX9z9rNZ2UHEv7LJ8kMxUPSK6hCL7aZMdbhOY6uzCqaOB8RRAa-Z-m5oXbYiXIQBohCv1ZRk3hABl4d4_V0nDnCGjAW8FHtDyCzykeoM0pyDXmHUbLpTJUcZsAPshcz2DRRnXSpnZujuHik0LkBSnfGnNruxIHE6ENl7K8bVXRMNO1H1STfnxkmOS7gh6d27CFnTYYlHRmJfNjYaV-e4WgZB2T-sp1JG7Ecw3fmicr26dkQ29XyaQNV7CBNygBTyBTcLy1z84Fub4Xr55EocHmO7_TX6PssjxO90Gu9HFjtxz3Fd5EYpV2u3mW5EFVw",
        r#"","e":"AQAB"}]}"#
    );
    const BUILTIN_ACCESS_TOKEN_COMPACT_JWS: &str = concat!(
        "eyJhbGciOiJSUzI1NiIsImtpZCI6ImJ1aWx0aW4tcnMyNTYiLCJ0eXAiOiJhdCtqd3QifQ.",
        "eyJzdWIiOiJidWlsdGluLXZlY3RvciIsImF1ZCI6InNlcnZlci1wb2xpY3ktbGF0ZXIifQ.",
        "GGpHGWOw1LY88mXA0JCQkwUzDhsxvvwuURggFYhA02TgvK4hi3N3X0XGC3pDG6nyD1r1AI6eq73BY2wqzUOzzzsVYAOM_VMNkI6THNe_FF-w-JYnq8GwnP2h4TGziWT6dXNueGSZ6A81tyzOgrgNxAPMi8kGG5ZhzQhwFhzs13VFg0HrEis_3BM9pqbQ2czQjq5OS4hM841I9_abtWELRJDIR3dGg6kLzrUXOvsUwlQGv9rIXf5_bMYFLkMM3lU7JMLM-JwTG6DKWBAfZxrkNF5ww5Q9IaBbu_BCYagDiMiCN1_mLwbvhi4xLMqHsiRHdBnXTbOIXebbj8_hf9oyyw"
    );
    const BUILTIN_ACCESS_TOKEN_CLAIMS: &str =
        r#"{"sub":"builtin-vector","aud":"server-policy-later"}"#;

    fn admitted_fixed_jwks() -> AdmittedRsaJwks {
        AdmittedRsaJwks::from_json(FIXED_JWK.as_bytes()).expect("fixed public JWK admits")
    }

    fn admitted_builtin_access_token_jwks() -> AdmittedRsaJwks {
        AdmittedRsaJwks::from_json(BUILTIN_ACCESS_TOKEN_JWK.as_bytes())
            .expect("builtin access-token public JWK admits")
    }

    #[test]
    fn canonical_public_jwks_is_admitted_and_bound_to_its_external_key() {
        let binding = fixed_binding();
        let key = fixed_attested_key(binding);
        let canonical = key
            .canonical_public_jwks()
            .expect("admitted external public key has canonical public JWKS");

        assert_eq!(canonical.binding(), binding);
        assert_eq!(
            canonical.as_bytes(),
            key.canonical_public_jwks()
                .expect("canonicalization is stable")
                .as_bytes()
        );
        let admitted = AdmittedRsaJwks::from_json(canonical.as_bytes())
            .expect("canonical public JWKS remains locally verifiable");
        assert_eq!(admitted.len(), 1);
        assert!(admitted.contains_kid("fixed-rs256"));
    }

    #[test]
    fn fnd09_fixed_and_builtin_public_jwk_fixtures_are_exact_json_and_admit() {
        for jwk in [FIXED_JWK, BUILTIN_ACCESS_TOKEN_JWK] {
            let parsed: Value = serde_json::from_str(jwk).expect("retained public JWK is JSON");
            assert_eq!(parsed["keys"][0]["kty"], "RSA");
            assert!(AdmittedRsaJwks::from_json(jwk.as_bytes()).is_ok());
        }
    }

    #[test]
    fn rh5_unquoted_modulus_fixture_is_rejected_without_changing_valid_jwk_admission() {
        let valid_jwk = FIXED_JWK.to_owned();
        let valid_snapshot = valid_jwk.clone();
        let malformed_jwk = valid_jwk.replacen("\"n\":\"", "\"n\":", 1);

        assert_eq!(
            AdmittedRsaJwks::from_json(malformed_jwk.as_bytes()),
            Err(JoseError::InvalidJson("JWKS")),
            "RH-5: removing only the modulus JSON quote must reject before public-key admission"
        );
        assert_eq!(
            valid_jwk, valid_snapshot,
            "the malformed near-neighbor cannot alter the retained valid fixture"
        );
        assert!(
            AdmittedRsaJwks::from_json(valid_jwk.as_bytes()).is_ok(),
            "the valid fixed JWK remains admissible after rejection"
        );
        assert!(
            AdmittedRsaJwks::from_json(BUILTIN_ACCESS_TOKEN_JWK.as_bytes()).is_ok(),
            "the valid builtin-access-token JWK remains admissible after rejection"
        );
    }

    fn jwks_with_rsa_components(kid: &str, modulus: &[u8], exponent: &[u8]) -> String {
        let modulus = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(modulus);
        let exponent = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(exponent);
        format!(
            r#"{{"keys":[{{"kty":"RSA","kid":"{kid}","alg":"RS256","use":"sig","n":"{modulus}","e":"{exponent}"}}]}}"#
        )
    }

    #[test]
    fn verifies_fixed_rs256_vector_without_retaining_the_bearer() {
        let bearer = FIXED_COMPACT_JWS.to_owned();
        let bearer_snapshot = bearer.clone();
        let verified = verify_compact_jws_rs256(&bearer, &admitted_fixed_jwks())
            .expect("fixed OpenSSL RS256 vector verifies");

        assert_eq!(verified.header().kid(), "fixed-rs256");
        assert_eq!(verified.claims()["sub"], "fixed-vector");
        // `aud` is deliberately returned untouched; server policy owns it later.
        assert_eq!(verified.claims()["aud"], "server-policy-later");
        let debug = format!("{verified:?}");
        let (protected_and_claims, signature) = FIXED_COMPACT_JWS
            .rsplit_once('.')
            .expect("compact JWS signature");
        assert!(!debug.contains(FIXED_COMPACT_JWS));
        assert!(!debug.contains(protected_and_claims));
        assert!(!debug.contains(signature));
        assert_eq!(bearer, bearer_snapshot);
    }

    #[test]
    fn rh5_signature_mutation_leaves_admitted_key_and_input_unchanged() {
        let keys = admitted_fixed_jwks();
        let key_snapshot = keys.clone();
        let mut mutated = FIXED_COMPACT_JWS.to_owned();
        let last = mutated.pop().expect("non-empty token");
        mutated.push(if last == 'A' { 'B' } else { 'A' });
        let input_snapshot = mutated.clone();

        assert_eq!(
            verify_compact_jws_rs256(&mutated, &keys),
            Err(JoseError::InvalidSignature)
        );
        assert_eq!(keys, key_snapshot);
        assert_eq!(mutated, input_snapshot);
        assert!(verify_compact_jws_rs256(FIXED_COMPACT_JWS, &keys).is_ok());
    }

    #[test]
    fn rh5_header_kid_mutation_cannot_select_another_or_mutate_admission() {
        let keys = admitted_fixed_jwks();
        let key_snapshot = keys.clone();
        let mutated = FIXED_COMPACT_JWS.replacen(
            "eyJhbGciOiJSUzI1NiIsImtpZCI6ImZpeGVkLXJzMjU2In0",
            "eyJhbGciOiJSUzI1NiIsImtpZCI6ImZpeGVkLXJzMjU3In0",
            1,
        );
        let input_snapshot = mutated.clone();

        assert_eq!(
            verify_compact_jws_rs256(&mutated, &keys),
            Err(JoseError::UnknownKeyId)
        );
        assert_eq!(keys, key_snapshot);
        assert_eq!(mutated, input_snapshot);
        assert!(verify_compact_jws_rs256(FIXED_COMPACT_JWS, &keys).is_ok());
    }

    #[test]
    fn rh5_key_mutation_cannot_validate_or_change_the_admitted_snapshot() {
        let mutated_jwks = FIXED_JWK.replacen("jlHZ", "klHZ", 1);
        let input_snapshot = mutated_jwks.clone();
        let keys = AdmittedRsaJwks::from_json(mutated_jwks.as_bytes())
            .expect("same-shape key mutation remains an admitted public key");
        let key_snapshot = keys.clone();

        assert_eq!(
            verify_compact_jws_rs256(FIXED_COMPACT_JWS, &keys),
            Err(JoseError::InvalidSignature)
        );
        assert_eq!(keys, key_snapshot);
        assert_eq!(mutated_jwks, input_snapshot);
        assert!(verify_compact_jws_rs256(FIXED_COMPACT_JWS, &admitted_fixed_jwks()).is_ok());
    }

    #[test]
    fn admits_only_fnd09_rsa_modulus_sizes() {
        // These synthetic public keys exercise admission only. The fixed
        // 2048-bit vector above is the sole signature-verification proof.
        for modulus_len in ADMITTED_RSA_MODULUS_BYTE_LENGTHS {
            let jwks = jwks_with_rsa_components(
                &format!("modulus-{modulus_len}"),
                &vec![0x80; modulus_len],
                &ADMITTED_RSA_PUBLIC_EXPONENT,
            );
            let admitted = AdmittedRsaJwks::from_json(jwks.as_bytes())
                .expect("FND-09 modulus size admits without a fabricated signature");

            assert_eq!(admitted.len(), 1);
            assert!(admitted.contains_kid(&format!("modulus-{modulus_len}")));
        }
    }

    #[test]
    fn rh5_rejects_one_byte_fnd09_modulus_boundaries_without_state_change() {
        let admitted = admitted_fixed_jwks();
        let admitted_snapshot = admitted.clone();
        for modulus_len in [255, 257, 383, 385, 511, 513] {
            let jwks = jwks_with_rsa_components(
                &format!("boundary-{modulus_len}"),
                &vec![0x80; modulus_len],
                &ADMITTED_RSA_PUBLIC_EXPONENT,
            );
            let input_snapshot = jwks.clone();

            assert!(AdmittedRsaJwks::from_json(jwks.as_bytes()).is_err());
            assert_eq!(jwks, input_snapshot);
            assert_eq!(admitted, admitted_snapshot);
        }
        assert!(verify_compact_jws_rs256(FIXED_COMPACT_JWS, &admitted).is_ok());
    }

    #[test]
    fn rh5_rejects_short_high_bit_at_each_fnd09_modulus_size_without_state_change() {
        let admitted = admitted_fixed_jwks();
        let admitted_snapshot = admitted.clone();
        for modulus_len in ADMITTED_RSA_MODULUS_BYTE_LENGTHS {
            let positive_modulus = vec![0x80; modulus_len];
            let positive = jwks_with_rsa_components(
                &format!("exact-bits-{modulus_len}"),
                &positive_modulus,
                &ADMITTED_RSA_PUBLIC_EXPONENT,
            );
            assert!(AdmittedRsaJwks::from_json(positive.as_bytes()).is_ok());

            let mut short_high_bit_modulus = positive_modulus.clone();
            short_high_bit_modulus[0] = 0x7f;
            let negative = jwks_with_rsa_components(
                &format!("exact-bits-{modulus_len}"),
                &short_high_bit_modulus,
                &ADMITTED_RSA_PUBLIC_EXPONENT,
            );
            let input_snapshot = negative.clone();

            assert_eq!(
                AdmittedRsaJwks::from_json(negative.as_bytes()),
                Err(JoseError::InvalidJwk("non-exact RSA modulus bit length"))
            );
            assert_eq!(negative, input_snapshot);
            assert_eq!(admitted, admitted_snapshot);
        }
        assert!(verify_compact_jws_rs256(FIXED_COMPACT_JWS, &admitted).is_ok());
    }

    #[test]
    fn rh5_rejects_noncanonical_exponent_without_state_change_and_allows_valid_retry() {
        let admitted = admitted_fixed_jwks();
        let admitted_snapshot = admitted.clone();
        let noncanonical = FIXED_JWK.replacen("\"e\":\"AQAB\"", "\"e\":\"AQAC\"", 1);
        let input_snapshot = noncanonical.clone();

        assert_eq!(
            AdmittedRsaJwks::from_json(noncanonical.as_bytes()),
            Err(JoseError::InvalidJwk("invalid exponent"))
        );
        assert_eq!(noncanonical, input_snapshot);
        assert_eq!(admitted, admitted_snapshot);
        assert!(verify_compact_jws_rs256(FIXED_COMPACT_JWS, &admitted).is_ok());
    }

    #[test]
    fn rejects_security_relevant_duplicate_members_and_unsafe_jwk_forms() {
        assert_eq!(
            AdmittedRsaJwks::from_json(
                br#"{"keys":[{"kty":"RSA","kty":"oct","kid":"a","alg":"RS256","n":"AQAB","e":"AQAB"}]}"#
            ),
            Err(JoseError::DuplicateJsonMember)
        );
        assert_eq!(
            AdmittedRsaJwks::from_json(
                br#"{"keys":[{"kty":"oct","kid":"a","alg":"RS256","k":"secret"}]}"#
            ),
            Err(JoseError::InvalidJwk("only RSA keys are admitted"))
        );
        assert_eq!(
            AdmittedRsaJwks::from_json(
                br#"{"keys":[{"kty":"RSA","kid":"a","alg":"RS256","jku":"https://attacker.invalid/keys","n":"AQAB","e":"AQAB"}]}"#
            ),
            Err(JoseError::InvalidJwk("remote key reference"))
        );
        assert_eq!(
            AdmittedRsaJwks::from_json(
                FIXED_JWK
                    .replacen("\"kid\":\"fixed-rs256\",", "", 1)
                    .as_bytes()
            ),
            Err(JoseError::InvalidJwk("missing or invalid kid"))
        );
    }

    #[test]
    fn rejects_header_alg_none_missing_or_duplicate_kid_and_remote_references() {
        let keys = admitted_fixed_jwks();
        for (header, expected) in [
            (
                r#"{"alg":"none","kid":"fixed-rs256"}"#,
                JoseError::UnsupportedAlgorithm,
            ),
            (
                r#"{"alg":"ES256","kid":"fixed-rs256"}"#,
                JoseError::UnsupportedAlgorithm,
            ),
            (r#"{"alg":"RS256"}"#, JoseError::MissingKeyId),
            (
                r#"{"alg":"RS256","kid":"fixed-rs256","kid":"fixed-rs256"}"#,
                JoseError::DuplicateJsonMember,
            ),
            (
                r#"{"alg":"RS256","kid":"fixed-rs256","jku":"https://attacker.invalid/keys"}"#,
                JoseError::DisallowedHeader("jku"),
            ),
            (
                r#"{"alg":"RS256","kid":"fixed-rs256","x5u":"https://attacker.invalid/key"}"#,
                JoseError::DisallowedHeader("x5u"),
            ),
        ] {
            let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(header);
            let candidate = format!(
                "{header}.{}",
                FIXED_COMPACT_JWS.split_once('.').expect("compact JWS").1
            );
            assert_eq!(verify_compact_jws_rs256(&candidate, &keys), Err(expected));
        }
    }

    #[test]
    fn rejects_malformed_signature_encoding_without_mutating_admitted_keys() {
        let keys = admitted_fixed_jwks();
        let key_snapshot = keys.clone();
        let (protected_and_claims, _) = FIXED_COMPACT_JWS
            .rsplit_once('.')
            .expect("compact JWS signature");
        let malformed = format!("{protected_and_claims}.A");
        let input_snapshot = malformed.clone();

        assert_eq!(
            verify_compact_jws_rs256(&malformed, &keys),
            Err(JoseError::InvalidBase64Url("signature"))
        );
        assert_eq!(keys, key_snapshot);
        assert_eq!(malformed, input_snapshot);
        assert!(verify_compact_jws_rs256(FIXED_COMPACT_JWS, &keys).is_ok());
    }

    #[derive(Clone, Copy)]
    enum FixedBackendMode {
        Valid,
        ChangedGeneration,
        WrongReceipt,
        MalformedSignature,
        SaturatedNotDispatched,
        Unknown,
        CancelImmediatelyBeforeReturn,
    }

    struct FixedVectorExternalBackend {
        mode: FixedBackendMode,
        calls: Arc<AtomicUsize>,
    }

    impl ExternalRs256SignerBackend for FixedVectorExternalBackend {
        fn sign<'a>(
            &'a self,
            cx: &'a fastmcp_core::Cx,
            request: ExternalRs256SigningRequest,
        ) -> Pin<Box<dyn Future<Output = ExternalRs256SignDisposition> + Send + 'a>> {
            let expected_input = FIXED_COMPACT_JWS
                .rsplit_once('.')
                .expect("fixed compact JWS contains a signature")
                .0
                .as_bytes()
                .to_vec();
            let input_matches = request
                .input()
                .with_bytes(|input| input == expected_input.as_slice());
            let binding = request.binding();
            let mode = self.mode;
            let calls = Arc::clone(&self.calls);
            Box::pin(async move {
                assert!(
                    input_matches,
                    "facade must construct the exact vector input"
                );
                calls.fetch_add(1, Ordering::AcqRel);
                if matches!(mode, FixedBackendMode::CancelImmediatelyBeforeReturn) {
                    cx.set_cancel_requested(true);
                }
                let receipt_binding = match mode {
                    FixedBackendMode::ChangedGeneration => Rs256SigningBinding::new(
                        binding.provider_generation(),
                        binding.key_generation(),
                        binding.ring_generation().saturating_add(1),
                        binding.configuration_generation(),
                    )
                    .expect("changed generation remains well formed"),
                    FixedBackendMode::WrongReceipt => Rs256SigningBinding::new(
                        binding.provider_generation(),
                        binding.key_generation(),
                        binding.ring_generation(),
                        binding.configuration_generation().saturating_add(1),
                    )
                    .expect("changed configuration remains well formed"),
                    _ => binding,
                };
                let receipt = ExternalRs256OperationReceipt::new(
                    receipt_binding,
                    1,
                    RedactedSignerProvenance::new("test-external-kms")
                        .expect("redacted fixture provenance"),
                )
                .expect("fixture receipt");
                match mode {
                    FixedBackendMode::SaturatedNotDispatched => {
                        ExternalRs256SignDisposition::NotDispatched(receipt)
                    }
                    FixedBackendMode::Unknown => ExternalRs256SignDisposition::Unknown(receipt),
                    FixedBackendMode::MalformedSignature => {
                        ExternalRs256SignDisposition::Dispatched(
                            RawRs256Signature::from_bytes(vec![0_u8; 1])
                                .expect("bounded malformed fixture signature"),
                            receipt,
                        )
                    }
                    FixedBackendMode::Valid
                    | FixedBackendMode::ChangedGeneration
                    | FixedBackendMode::WrongReceipt
                    | FixedBackendMode::CancelImmediatelyBeforeReturn => {
                        ExternalRs256SignDisposition::Dispatched(fixed_raw_signature(), receipt)
                    }
                }
            })
        }
    }

    struct BuiltinAccessTokenVectorBackend {
        calls: Arc<AtomicUsize>,
        return_altered_signature: bool,
    }

    impl ExternalRs256SignerBackend for BuiltinAccessTokenVectorBackend {
        fn sign<'a>(
            &'a self,
            _: &'a fastmcp_core::Cx,
            request: ExternalRs256SigningRequest,
        ) -> Pin<Box<dyn Future<Output = ExternalRs256SignDisposition> + Send + 'a>> {
            let expected_input = BUILTIN_ACCESS_TOKEN_COMPACT_JWS
                .rsplit_once('.')
                .expect("builtin compact JWS contains a signature")
                .0
                .as_bytes()
                .to_vec();
            let input_matches = request
                .input()
                .with_bytes(|input| input == expected_input.as_slice());
            let binding = request.binding();
            let calls = Arc::clone(&self.calls);
            let return_altered_signature = self.return_altered_signature;
            Box::pin(async move {
                assert!(
                    input_matches,
                    "BuiltinAccessToken must use the exact facade-built typ=at+jwt input"
                );
                calls.fetch_add(1, Ordering::AcqRel);
                let receipt = ExternalRs256OperationReceipt::new(
                    binding,
                    1,
                    RedactedSignerProvenance::new("test-external-kms")
                        .expect("redacted fixture provenance"),
                )
                .expect("fixture receipt");
                ExternalRs256SignDisposition::Dispatched(
                    if return_altered_signature {
                        altered_builtin_access_token_raw_signature()
                    } else {
                        builtin_access_token_raw_signature()
                    },
                    receipt,
                )
            })
        }
    }

    fn fixed_binding() -> Rs256SigningBinding {
        Rs256SigningBinding::new(11, 12, 13, 14).expect("fixed nonzero generations")
    }

    fn attested_key_from_jwk(
        jwk_json: &str,
        kid: &str,
        binding: Rs256SigningBinding,
    ) -> AttestedRs256PublicKey {
        let jwk: Value = serde_json::from_str(jwk_json).expect("fixed JWK JSON");
        let modulus = jwk["keys"][0]["n"].as_str().expect("fixed modulus text");
        let modulus = decode_base64url(modulus, MAX_ADMITTED_RSA_MODULUS_BYTES, "modulus")
            .expect("fixed modulus bytes");
        AttestedRs256PublicKey::admit(
            kid,
            modulus,
            binding,
            RedactedSignerProvenance::new("test-external-kms")
                .expect("redacted fixture provenance"),
        )
        .expect("fixed public key admits")
    }

    fn fixed_attested_key(binding: Rs256SigningBinding) -> AttestedRs256PublicKey {
        attested_key_from_jwk(FIXED_JWK, "fixed-rs256", binding)
    }

    fn builtin_access_token_attested_key(binding: Rs256SigningBinding) -> AttestedRs256PublicKey {
        attested_key_from_jwk(BUILTIN_ACCESS_TOKEN_JWK, "builtin-rs256", binding)
    }

    fn fixed_signer(
        mode: FixedBackendMode,
    ) -> (ExternalRs256Signer, Arc<AtomicUsize>, Rs256SigningBinding) {
        let calls = Arc::new(AtomicUsize::new(0));
        let binding = fixed_binding();
        let backend = Arc::new(FixedVectorExternalBackend {
            mode,
            calls: Arc::clone(&calls),
        });
        (
            ExternalRs256Signer::new(backend, fixed_attested_key(binding)),
            calls,
            binding,
        )
    }

    fn ring_signer(kid: &str, binding: Rs256SigningBinding) -> Arc<ExternalRs256Signer> {
        let key = attested_key_from_jwk(FIXED_JWK, kid, binding);
        Arc::new(ExternalRs256Signer::new(
            Arc::new(FixedVectorExternalBackend {
                mode: FixedBackendMode::Valid,
                calls: Arc::new(AtomicUsize::new(0)),
            }),
            key,
        ))
    }

    #[test]
    fn key_ring_canonical_overlap_and_retirement_fence_are_generation_bound() {
        let binding = fixed_binding();
        let active = ring_signer("active", binding);
        let retained = ring_signer("retained", binding);
        let ring = Rs256PublicKeyRing::new(Arc::clone(&active), vec![retained], 31)
            .expect("two public signer keys form one ring");
        let overlap = ring
            .canonical_public_jwks()
            .expect("canonical overlap JWKS");
        let admitted = AdmittedRsaJwks::from_json(overlap.as_bytes()).expect("admit overlap");
        assert_eq!(admitted.len(), 2);
        assert_eq!(overlap.generation(), 31);

        let successor = ring_signer("successor", binding);
        let rotated = ring
            .rotate(Arc::clone(&successor), 32)
            .expect("successor retains active verification overlap");
        assert_eq!(
            AdmittedRsaJwks::from_json(
                rotated
                    .canonical_public_jwks()
                    .expect("canonical rotated JWKS")
                    .as_bytes(),
            )
            .expect("admit rotated JWKS")
            .len(),
            3,
        );
        assert!(matches!(
            ring.retire("active", 100, 101, Arc::clone(&successor), 32),
            Err(JwsSignerConfigurationError::RetirementNotPermitted)
        ));
        let retired = ring
            .retire("active", 101, 101, successor, 32)
            .expect("retirement after final token expiry");
        let retired = retired
            .canonical_public_jwks()
            .expect("canonical retired JWKS");
        assert_eq!(retired.generation(), 32);
        assert_eq!(
            AdmittedRsaJwks::from_json(retired.as_bytes())
                .expect("admit retired JWKS")
                .len(),
            2,
        );
    }

    #[test]
    fn signing_activation_receipt_requires_exact_endpoint_bytes_and_generation() {
        let binding = fixed_binding();
        let active = ring_signer("fixed-rs256", binding);
        let ring = Rs256PublicKeyRing::new(active, Vec::new(), 41).expect("one-key ring");
        let jwks = ring.canonical_public_jwks().expect("canonical JWKS");
        let endpoint = JwksEndpointReadBack::new(
            "https://issuer.example.test/oidc/jwks",
            "https://issuer.example.test",
            jwks.as_bytes().to_vec(),
            41,
        )
        .expect("bounded endpoint evidence");
        let receipt = SigningActivationReceipt::verify(
            SigningActivationProfile::OidcIdToken,
            "https://issuer.example.test/",
            &jwks,
            &["https://issuer.example.test/oidc/jwks".to_string()],
            &["https://issuer.example.test".to_string()],
            vec![endpoint],
            FIXED_COMPACT_JWS.to_string(),
        )
        .expect("fixed RS256 canary verifies against endpoint bytes");
        assert!(receipt.applies_to(
            SigningActivationProfile::OidcIdToken,
            "https://issuer.example.test/",
            &jwks,
            &["https://issuer.example.test/oidc/jwks".to_string()],
            &["https://issuer.example.test".to_string()],
        ));

        let stale = JwksEndpointReadBack::new(
            "https://issuer.example.test/oidc/jwks",
            "https://issuer.example.test",
            jwks.as_bytes().to_vec(),
            40,
        )
        .expect("bounded stale evidence");
        assert!(
            SigningActivationReceipt::verify(
                SigningActivationProfile::OidcIdToken,
                "https://issuer.example.test/",
                &jwks,
                &["https://issuer.example.test/oidc/jwks".to_string()],
                &["https://issuer.example.test".to_string()],
                vec![stale],
                FIXED_COMPACT_JWS.to_string(),
            )
            .is_err()
        );
    }

    fn builtin_access_token_signer(
        return_altered_signature: bool,
    ) -> (ExternalRs256Signer, Arc<AtomicUsize>, Rs256SigningBinding) {
        let calls = Arc::new(AtomicUsize::new(0));
        let binding = fixed_binding();
        let backend = Arc::new(BuiltinAccessTokenVectorBackend {
            calls: Arc::clone(&calls),
            return_altered_signature,
        });
        (
            ExternalRs256Signer::new(backend, builtin_access_token_attested_key(binding)),
            calls,
            binding,
        )
    }

    fn fixed_claims() -> BoundedJwsClaims {
        BoundedJwsClaims::from_json_bytes(FIXED_CLAIMS.as_bytes())
            .expect("fixed vector claims admit")
    }

    fn builtin_access_token_claims() -> BoundedJwsClaims {
        BoundedJwsClaims::from_json_bytes(BUILTIN_ACCESS_TOKEN_CLAIMS.as_bytes())
            .expect("builtin access-token claims admit")
    }

    fn fixed_deadline() -> ExternalRs256SigningDeadline {
        ExternalRs256SigningDeadline::new(Duration::from_secs(1)).expect("finite fixture deadline")
    }

    async fn sign_with_runtime_context(
        signer: &ExternalRs256Signer,
        profile: JwsSigningProfile,
        claims: BoundedJwsClaims,
        deadline: ExternalRs256SigningDeadline,
    ) -> (
        Result<SelfVerifiedRs256Jws, JwsSigningError>,
        fastmcp_core::Cx,
    ) {
        let cx = fastmcp_core::Cx::current()
            .expect("fastmcp_core::block_on installs an ambient runtime context");
        let result = signer.sign(&cx, profile, claims, deadline).await;
        (result, cx)
    }

    #[test]
    fn external_rs256_signing_deadline_admits_exact_bound_and_rejects_one_nanosecond_over() {
        let admitted = ExternalRs256SigningDeadline::new(MAX_EXTERNAL_RS256_SIGNING_DEADLINE)
            .expect("the exact external signing deadline bound admits");
        let over_bound = MAX_EXTERNAL_RS256_SIGNING_DEADLINE
            .checked_add(Duration::from_nanos(1))
            .expect("the bounded maximum has room for the RH-5 delta");

        assert_eq!(admitted.duration(), MAX_EXTERNAL_RS256_SIGNING_DEADLINE);
        assert_eq!(
            ExternalRs256SigningDeadline::new(over_bound),
            Err(JwsSignerConfigurationError::InvalidDeadline),
            "RH-5: changing only the deadline by one nanosecond rejects before any signing state exists"
        );
        assert_eq!(
            admitted.duration(),
            MAX_EXTERNAL_RS256_SIGNING_DEADLINE,
            "the rejected N+1 admission cannot alter the previously admitted deadline"
        );
    }

    fn raw_signature_from_compact(compact_jws: &str) -> RawRs256Signature {
        let signature = compact_jws
            .rsplit_once('.')
            .expect("fixed compact JWS contains a signature")
            .1;
        RawRs256Signature::from_bytes(
            decode_base64url(signature, MAX_ADMITTED_RSA_MODULUS_BYTES, "signature")
                .expect("fixed signature bytes"),
        )
        .expect("fixed signature admits")
    }

    fn fixed_raw_signature() -> RawRs256Signature {
        raw_signature_from_compact(FIXED_COMPACT_JWS)
    }

    fn builtin_access_token_raw_signature() -> RawRs256Signature {
        raw_signature_from_compact(BUILTIN_ACCESS_TOKEN_COMPACT_JWS)
    }

    fn altered_builtin_access_token_raw_signature() -> RawRs256Signature {
        let signature = BUILTIN_ACCESS_TOKEN_COMPACT_JWS
            .rsplit_once('.')
            .expect("builtin compact JWS contains a signature")
            .1;
        let mut signature =
            decode_base64url(signature, MAX_ADMITTED_RSA_MODULUS_BYTES, "signature")
                .expect("builtin signature bytes");
        signature[0] ^= 1;
        RawRs256Signature::from_bytes(signature).expect("altered signature remains bounded")
    }

    #[test]
    fn fnd09_external_signer_facade_self_verifies_the_fixed_rs256_vector() {
        let (signer, calls, binding) = fixed_signer(FixedBackendMode::Valid);
        let candidate = block_on(sign_with_runtime_context(
            &signer,
            JwsSigningProfile::ClientAssertion,
            fixed_claims(),
            fixed_deadline(),
        ))
        .0
        .expect("the externally returned fixed signature self-verifies");

        assert_eq!(calls.load(Ordering::Acquire), 1);
        assert_eq!(candidate.binding(), binding);
        assert_eq!(candidate.into_compact_jws(), FIXED_COMPACT_JWS);
    }

    #[test]
    fn fnd09_builtin_access_token_facade_uses_canonical_typ_and_self_verifies() {
        let (signer, calls, binding) = builtin_access_token_signer(false);
        let candidate = block_on(sign_with_runtime_context(
            &signer,
            JwsSigningProfile::BuiltinAccessToken,
            builtin_access_token_claims(),
            fixed_deadline(),
        ))
        .0
        .expect("the facade-built builtin access-token vector self-verifies");

        assert_eq!(calls.load(Ordering::Acquire), 1);
        assert_eq!(candidate.binding(), binding);
        let compact = candidate.into_compact_jws();
        assert_eq!(compact, BUILTIN_ACCESS_TOKEN_COMPACT_JWS);
        let header = compact.split_once('.').expect("compact JWS header").0;
        let header = decode_base64url(header, MAX_JWS_HEADER_BYTES, "header")
            .expect("canonical header bytes");
        assert_eq!(
            header,
            br#"{"alg":"RS256","kid":"builtin-rs256","typ":"at+jwt"}"#
        );
        assert!(verify_compact_jws_rs256(&compact, &admitted_builtin_access_token_jwks()).is_ok());
    }

    #[test]
    fn rh5_builtin_access_token_missing_or_wrong_typ_exposes_no_candidate() {
        let keys = admitted_builtin_access_token_jwks();
        let key_snapshot = keys.clone();
        let (_, claims_and_signature) = BUILTIN_ACCESS_TOKEN_COMPACT_JWS
            .split_once('.')
            .expect("compact JWS header");
        let (claims, signature) = claims_and_signature
            .rsplit_once('.')
            .expect("compact JWS signature");

        for header in [
            r#"{"alg":"RS256","kid":"builtin-rs256"}"#,
            r#"{"alg":"RS256","kid":"builtin-rs256","typ":"jwt"}"#,
        ] {
            let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(header);
            let altered = format!("{header}.{claims}.{signature}");
            assert_eq!(
                verify_compact_jws_rs256(&altered, &keys),
                Err(JoseError::InvalidSignature),
                "changing only the facade-owned typ field cannot expose a candidate"
            );
        }

        assert_eq!(
            keys, key_snapshot,
            "rejection cannot mutate public-key admission"
        );
        assert!(verify_compact_jws_rs256(BUILTIN_ACCESS_TOKEN_COMPACT_JWS, &keys).is_ok());
    }

    #[test]
    fn rh5_builtin_access_token_altered_signature_through_facade_exposes_no_candidate() {
        let (signer, calls, binding) = builtin_access_token_signer(true);
        let error = block_on(sign_with_runtime_context(
            &signer,
            JwsSigningProfile::BuiltinAccessToken,
            builtin_access_token_claims(),
            fixed_deadline(),
        ))
        .0
        .expect_err("an altered external signature cannot pass facade self-verification");

        assert!(matches!(error, JwsSigningError::SelfVerificationFailed));
        assert_eq!(calls.load(Ordering::Acquire), 1);
        assert_eq!(
            signer.binding(),
            binding,
            "rejection cannot mutate the signer"
        );

        let (fresh, _, fresh_binding) = builtin_access_token_signer(false);
        let candidate = block_on(sign_with_runtime_context(
            &fresh,
            JwsSigningProfile::BuiltinAccessToken,
            builtin_access_token_claims(),
            fixed_deadline(),
        ))
        .0
        .expect("a fresh facade remains usable after rejecting an altered signature");
        assert_eq!(candidate.binding(), fresh_binding);
    }

    #[test]
    fn rh5_external_signer_changed_generation_rejects_then_fresh_binding_still_signs() {
        let (signer, calls, binding) = fixed_signer(FixedBackendMode::ChangedGeneration);
        let error = block_on(sign_with_runtime_context(
            &signer,
            JwsSigningProfile::ClientAssertion,
            fixed_claims(),
            fixed_deadline(),
        ))
        .0
        .expect_err("changing only the ring generation must reject before a candidate exists");
        assert!(matches!(error, JwsSigningError::ReceiptMismatch));
        assert_eq!(calls.load(Ordering::Acquire), 1);
        assert_eq!(
            signer.binding(),
            binding,
            "rejected receipt cannot mutate binding"
        );

        let (fresh, _, fresh_binding) = fixed_signer(FixedBackendMode::Valid);
        let candidate = block_on(sign_with_runtime_context(
            &fresh,
            JwsSigningProfile::ClientAssertion,
            fixed_claims(),
            fixed_deadline(),
        ))
        .0
        .expect("a fresh matching binding remains usable after rejection");
        assert_eq!(candidate.binding(), fresh_binding);
    }

    #[test]
    fn rh5_external_signer_wrong_receipt_and_malformed_signature_expose_no_candidate() {
        for mode in [
            FixedBackendMode::WrongReceipt,
            FixedBackendMode::MalformedSignature,
        ] {
            let (signer, calls, binding) = fixed_signer(mode);
            let error = block_on(sign_with_runtime_context(
                &signer,
                JwsSigningProfile::ClientAssertion,
                fixed_claims(),
                fixed_deadline(),
            ))
            .0
            .expect_err("one malicious provider return dimension must fail closed");
            assert!(matches!(
                error,
                JwsSigningError::ReceiptMismatch | JwsSigningError::SignatureLengthMismatch
            ));
            assert_eq!(calls.load(Ordering::Acquire), 1);
            assert_eq!(
                signer.binding(),
                binding,
                "refusal cannot alter signer state"
            );

            let (fresh, _, fresh_binding) = fixed_signer(FixedBackendMode::Valid);
            let candidate = block_on(sign_with_runtime_context(
                &fresh,
                JwsSigningProfile::ClientAssertion,
                fixed_claims(),
                fixed_deadline(),
            ))
            .0
            .expect("a fresh matching signer remains usable after refusal");
            assert_eq!(candidate.binding(), fresh_binding);
        }
    }

    #[test]
    fn rh5_not_dispatched_is_reusable_but_unknown_consumes_the_candidate() {
        let (saturated, calls, binding) = fixed_signer(FixedBackendMode::SaturatedNotDispatched);
        let reusable = block_on(sign_with_runtime_context(
            &saturated,
            JwsSigningProfile::ClientAssertion,
            fixed_claims(),
            fixed_deadline(),
        ))
        .0
        .expect_err("a saturated backend reports not-dispatched without a candidate")
        .into_reusable_claims()
        .expect("only a proven not-dispatched attempt may return the claims");
        assert_eq!(calls.load(Ordering::Acquire), 1);
        assert_eq!(saturated.binding(), binding);

        let (fresh, _, _) = fixed_signer(FixedBackendMode::Valid);
        assert!(
            block_on(sign_with_runtime_context(
                &fresh,
                JwsSigningProfile::ClientAssertion,
                reusable,
                fixed_deadline(),
            ))
            .0
            .is_ok()
        );

        let (unknown, calls, unknown_binding) = fixed_signer(FixedBackendMode::Unknown);
        let error = block_on(sign_with_runtime_context(
            &unknown,
            JwsSigningProfile::ClientAssertion,
            fixed_claims(),
            fixed_deadline(),
        ))
        .0
        .expect_err("unknown dispatch state consumes the candidate");
        assert!(matches!(&error, JwsSigningError::Unknown(_)));
        assert!(error.into_reusable_claims().is_none());
        assert_eq!(
            calls.load(Ordering::Acquire),
            1,
            "the facade never retries unknown work"
        );
        assert_eq!(unknown.binding(), unknown_binding);

        let (fresh, _, fresh_binding) = fixed_signer(FixedBackendMode::Valid);
        let candidate = block_on(sign_with_runtime_context(
            &fresh,
            JwsSigningProfile::ClientAssertion,
            fixed_claims(),
            fixed_deadline(),
        ))
        .0
        .expect("a fresh signer remains usable after unknown work is consumed");
        assert_eq!(candidate.binding(), fresh_binding);
    }

    #[test]
    fn rh5_cancellation_immediately_before_valid_provider_return_discards_the_late_signature() {
        let (signer, calls, binding) =
            fixed_signer(FixedBackendMode::CancelImmediatelyBeforeReturn);
        let (error, cx) = block_on(sign_with_runtime_context(
            &signer,
            JwsSigningProfile::ClientAssertion,
            fixed_claims(),
            fixed_deadline(),
        ));
        let error =
            error.expect_err("a valid signature returned after cancellation must remain unexposed");
        assert!(matches!(error, JwsSigningError::CancelledAfterDispatch(_)));
        assert_eq!(
            calls.load(Ordering::Acquire),
            1,
            "late cancellation cannot cause a retry"
        );
        assert_eq!(
            signer.binding(),
            binding,
            "late result cannot alter signer state"
        );
        assert!(cx.is_cancel_requested());

        let (fresh, _, fresh_binding) = fixed_signer(FixedBackendMode::Valid);
        let candidate = block_on(sign_with_runtime_context(
            &fresh,
            JwsSigningProfile::ClientAssertion,
            fixed_claims(),
            fixed_deadline(),
        ))
        .0
        .expect("a fresh context remains usable after a late cancellation");
        assert_eq!(candidate.binding(), fresh_binding);
    }
}
