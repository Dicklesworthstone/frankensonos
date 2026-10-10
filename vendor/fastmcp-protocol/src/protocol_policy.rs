//! Exact MCP protocol-version identity and immutable era-policy selection.
//!
//! This module intentionally models only the two supported protocol eras. It
//! does not normalize date strings or treat intermediate protocol revisions as
//! aliases. The crate-root integration is owned separately so that this policy
//! surface can remain independent of transport classification and lifecycle
//! state.

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use fastmcp_core::CanonicalHttpUrl;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The exact modern MCP protocol revision supported by this policy surface.
pub const MODERN_PROTOCOL_VERSION: &str = "2026-07-28";

/// The exact legacy MCP protocol revision supported by this policy surface.
pub const LEGACY_PROTOCOL_VERSION: &str = "2024-11-05";

/// A protocol era selected from one exact, supported protocol revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ProtocolEra {
    /// MCP 2026-07-28 per-request metadata semantics.
    Modern2026,
    /// MCP 2024-11-05 initialize-handshake semantics.
    Legacy2024,
}

impl ProtocolEra {
    /// Returns this era's only supported wire version.
    #[must_use]
    pub const fn version(self) -> ProtocolVersion {
        match self {
            Self::Modern2026 => ProtocolVersion::MODERN_2026,
            Self::Legacy2024 => ProtocolVersion::LEGACY_2024,
        }
    }
}

/// An exact, supported MCP protocol-version value.
///
/// This type can contain only the two revisions defined by this module.
/// Callers must retain an unsupported input separately after
/// [`ProtocolVersion::parse`] rejects it; in particular, `2025-11-25` cannot
/// be parsed, aliased, or normalized into either supported era.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProtocolVersion(ProtocolEra);

impl ProtocolVersion {
    /// The exact MCP 2026-07-28 protocol version.
    pub const MODERN_2026: Self = Self(ProtocolEra::Modern2026);

    /// The exact MCP 2024-11-05 protocol version.
    pub const LEGACY_2024: Self = Self(ProtocolEra::Legacy2024);

    /// Parses one exact supported protocol-version string.
    pub fn parse(value: &str) -> Result<Self, ProtocolVersionError> {
        match value {
            MODERN_PROTOCOL_VERSION => Ok(Self::MODERN_2026),
            LEGACY_PROTOCOL_VERSION => Ok(Self::LEGACY_2024),
            _ => Err(ProtocolVersionError::UnsupportedVersion {
                received: value.to_owned(),
            }),
        }
    }

    /// Returns the exact protocol-version spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self.0 {
            ProtocolEra::Modern2026 => MODERN_PROTOCOL_VERSION,
            ProtocolEra::Legacy2024 => LEGACY_PROTOCOL_VERSION,
        }
    }

    /// Returns the era identified by this exact version.
    #[must_use]
    pub const fn era(self) -> ProtocolEra {
        self.0
    }
}

impl fmt::Display for ProtocolVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ProtocolVersion {
    type Err = ProtocolVersionError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl Serialize for ProtocolVersion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ProtocolVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

/// Typed refusal for an unsupported protocol-version spelling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolVersionError {
    /// The string is not one of FastMCP's two exact supported revisions.
    UnsupportedVersion {
        /// The input spelling, retained without normalization.
        received: String,
    },
}

impl fmt::Display for ProtocolVersionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedVersion { received } => {
                write!(formatter, "unsupported MCP protocol version {received:?}")
            }
        }
    }
}

impl std::error::Error for ProtocolVersionError {}

/// Immutable policy chosen before a client connects or a server binds.
///
/// `Auto` remains the default policy. A policy value does not itself classify
/// a peer or mutate in response to peer bytes, errors, timeouts, or auth
/// events; transport-specific classification belongs to FND-03 integration.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ProtocolPolicy {
    /// Permit both supported eras; transport-specific code classifies once.
    #[default]
    Auto,
    /// Permit only MCP 2026-07-28.
    ModernOnly,
    /// Permit only exact MCP 2024-11-05.
    LegacyOnly,
}

const MODERN_ONLY_VERSIONS: [ProtocolVersion; 1] = [ProtocolVersion::MODERN_2026];
const LEGACY_ONLY_VERSIONS: [ProtocolVersion; 1] = [ProtocolVersion::LEGACY_2024];
const AUTO_SUPPORTED_VERSIONS: [ProtocolVersion; 2] =
    [ProtocolVersion::MODERN_2026, ProtocolVersion::LEGACY_2024];
const NO_MODERN_DISCOVERY_VERSIONS: [ProtocolVersion; 0] = [];

impl ProtocolPolicy {
    const fn display_name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::ModernOnly => "modern-only",
            Self::LegacyOnly => "legacy-only",
        }
    }

    /// Returns whether this immutable policy admits a version.
    #[must_use]
    pub const fn permits(self, version: ProtocolVersion) -> bool {
        match self {
            Self::Auto => true,
            Self::ModernOnly => matches!(version.era(), ProtocolEra::Modern2026),
            Self::LegacyOnly => matches!(version.era(), ProtocolEra::Legacy2024),
        }
    }

    /// Returns the policy-level exact supported-version set.
    ///
    /// This answers which eras the selected policy can operate, not which
    /// versions a modern Streamable HTTP discovery response advertises. In
    /// particular, `Auto` retains both eras here so its legacy adapter path
    /// remains available after isolated transport classification.
    #[must_use]
    pub const fn supported_versions(self) -> &'static [ProtocolVersion] {
        match self {
            Self::Auto => &AUTO_SUPPORTED_VERSIONS,
            Self::ModernOnly => &MODERN_ONLY_VERSIONS,
            Self::LegacyOnly => &LEGACY_ONLY_VERSIONS,
        }
    }

    /// Returns the versions advertised by modern Streamable HTTP discovery.
    ///
    /// Legacy `2024-11-05` is selected only through the isolated legacy
    /// transport path. It is never included inline in a modern discovery
    /// response, including when the immutable policy is `Auto`.
    #[must_use]
    pub const fn modern_discovery_versions(self) -> &'static [ProtocolVersion] {
        match self {
            Self::Auto | Self::ModernOnly => &MODERN_ONLY_VERSIONS,
            Self::LegacyOnly => &NO_MODERN_DISCOVERY_VERSIONS,
        }
    }

    /// Returns the client preference order for this policy.
    ///
    /// Auto prefers the modern revision first; a fallback decision is owned by
    /// the isolated client-negotiation layer, never by this value type.
    #[must_use]
    pub const fn preferred_versions(self) -> &'static [ProtocolVersion] {
        self.supported_versions()
    }

    /// Returns whether using this policy requires a legacy adapter receipt.
    #[must_use]
    pub const fn requires_legacy_adapter(self) -> bool {
        !matches!(self, Self::ModernOnly)
    }

    /// Validates a client policy before any connect-side effect.
    pub fn validate_for_client(
        self,
        legacy_receipt: Option<&LegacyClientAdapterInstalledReceipt>,
    ) -> Result<ProtocolPolicySelection, ProtocolPolicyError> {
        self.validate(
            ProtocolRole::Client,
            legacy_receipt.map(LegacyReceipt::Client),
        )
    }

    /// Validates a server policy before any bind-side effect.
    pub fn validate_for_server(
        self,
        legacy_receipt: Option<&LegacyServerAdapterInstalledReceipt>,
    ) -> Result<ProtocolPolicySelection, ProtocolPolicyError> {
        self.validate(
            ProtocolRole::Server,
            legacy_receipt.map(LegacyReceipt::Server),
        )
    }

    fn validate(
        self,
        role: ProtocolRole,
        legacy_receipt: Option<LegacyReceipt<'_>>,
    ) -> Result<ProtocolPolicySelection, ProtocolPolicyError> {
        if !self.requires_legacy_adapter() {
            return Ok(ProtocolPolicySelection { policy: self, role });
        }

        let Some(receipt) = legacy_receipt else {
            return Err(ProtocolPolicyError::FeatureUnavailable { policy: self, role });
        };

        if receipt.policy() != self {
            return Err(ProtocolPolicyError::ReceiptPolicyMismatch {
                policy: self,
                receipt_policy: receipt.policy(),
                role,
            });
        }

        Ok(ProtocolPolicySelection { policy: self, role })
    }
}

/// The role for which an immutable policy is validated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProtocolRole {
    /// A client policy before connect.
    Client,
    /// A server policy before bind.
    Server,
}

/// A validated immutable policy and role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProtocolPolicySelection {
    policy: ProtocolPolicy,
    role: ProtocolRole,
}

impl ProtocolPolicySelection {
    /// Returns the selected immutable policy.
    #[must_use]
    pub const fn policy(self) -> ProtocolPolicy {
        self.policy
    }

    /// Returns the role for which the policy was validated.
    #[must_use]
    pub const fn role(self) -> ProtocolRole {
        self.role
    }
}

/// Typed policy-validation refusals raised before connect or bind side effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolPolicyError {
    /// A legacy-capable policy was selected without its required feature and
    /// sealed adapter-installation receipt.
    FeatureUnavailable {
        /// The rejected policy.
        policy: ProtocolPolicy,
        /// The role whose adapter receipt was required.
        role: ProtocolRole,
    },
    /// A sealed receipt was installed for a different immutable policy.
    ReceiptPolicyMismatch {
        /// The selected policy.
        policy: ProtocolPolicy,
        /// The policy to which the receipt is bound.
        receipt_policy: ProtocolPolicy,
        /// The role whose receipt was supplied.
        role: ProtocolRole,
    },
}

impl fmt::Display for ProtocolPolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FeatureUnavailable { policy, role } => {
                write!(
                    formatter,
                    "{policy:?} is unavailable for {role:?} without a legacy adapter receipt"
                )
            }
            Self::ReceiptPolicyMismatch {
                policy,
                receipt_policy,
                role,
            } => write!(
                formatter,
                "{role:?} legacy receipt is bound to {receipt_policy:?}, not {policy:?}"
            ),
        }
    }
}

impl std::error::Error for ProtocolPolicyError {}

/// Immutable facts bound into a sealed legacy-adapter installation receipt.
///
/// Only the exact adapter installers can supply this value to the sealed
/// issuer. It deliberately has no serializer or public constructor.
#[derive(Debug, PartialEq, Eq)]
pub struct LegacyReceiptBinding {
    policy: ProtocolPolicy,
    transport_binding: String,
    endpoint_or_process_configuration: String,
    security_partition: String,
    adapter_generation: u64,
    store_generation: u64,
    configuration_generation: u64,
    limits_profile_identity: String,
}

impl LegacyReceiptBinding {
    /// Creates a binding only from trusted in-crate adapter installation code.
    #[allow(clippy::too_many_arguments)]
    #[expect(
        dead_code,
        reason = "the sealed LEG-02/LEG-03 receipt installers that call this are not built yet"
    )]
    pub(crate) fn new(
        policy: ProtocolPolicy,
        transport_binding: String,
        endpoint_or_process_configuration: String,
        security_partition: String,
        adapter_generation: u64,
        store_generation: u64,
        configuration_generation: u64,
        limits_profile_identity: String,
    ) -> Self {
        Self {
            policy,
            transport_binding,
            endpoint_or_process_configuration,
            security_partition,
            adapter_generation,
            store_generation,
            configuration_generation,
            limits_profile_identity,
        }
    }
}

/// Opaque, non-cloneable proof that the exact legacy client adapter is installed.
#[derive(Debug, PartialEq, Eq)]
pub struct LegacyClientAdapterInstalledReceipt {
    binding: LegacyReceiptBinding,
}

/// Opaque, non-cloneable proof that the exact legacy server adapter is installed.
#[derive(Debug, PartialEq, Eq)]
pub struct LegacyServerAdapterInstalledReceipt {
    binding: LegacyReceiptBinding,
}

impl LegacyClientAdapterInstalledReceipt {
    /// Returns the immutable policy bound at client-adapter installation.
    #[must_use]
    pub const fn policy(&self) -> ProtocolPolicy {
        self.binding.policy
    }
}

impl LegacyServerAdapterInstalledReceipt {
    /// Returns the immutable policy bound at server-adapter installation.
    #[must_use]
    pub const fn policy(&self) -> ProtocolPolicy {
        self.binding.policy
    }
}

enum LegacyReceipt<'a> {
    Client(&'a LegacyClientAdapterInstalledReceipt),
    Server(&'a LegacyServerAdapterInstalledReceipt),
}

impl LegacyReceipt<'_> {
    const fn policy(&self) -> ProtocolPolicy {
        match self {
            Self::Client(receipt) => receipt.policy(),
            Self::Server(receipt) => receipt.policy(),
        }
    }
}

mod sealed {
    pub trait ReceiptIssuerSealed {}
}

/// Sealed issuer interface for the exact legacy-adapter installation path.
///
/// External crates cannot implement this trait and therefore cannot forge a
/// client or server installation receipt. The future LEG-02 and LEG-03
/// installers are the only intended in-crate implementers.
#[allow(private_bounds)]
pub trait LegacyAdapterReceiptIssuer: sealed::ReceiptIssuerSealed {
    /// Issues a client receipt from trusted installation facts.
    #[doc(hidden)]
    fn issue_client_receipt(binding: LegacyReceiptBinding) -> LegacyClientAdapterInstalledReceipt {
        LegacyClientAdapterInstalledReceipt { binding }
    }

    /// Issues a server receipt from trusted installation facts.
    #[doc(hidden)]
    fn issue_server_receipt(binding: LegacyReceiptBinding) -> LegacyServerAdapterInstalledReceipt {
        LegacyServerAdapterInstalledReceipt { binding }
    }
}

/// The state of one server-side stdio process's immutable era selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StdioEraState {
    /// Auto policy has not yet received a structurally valid opening request.
    Unclassified,
    /// The process selected this era and cannot select again.
    Selected(ProtocolEra),
    /// An invalid opening closed the process without selecting an era.
    TerminalWithoutEra,
}

/// The opening frame relevant to stdio era selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StdioOpeningFrame {
    /// A complete modern request carrying a protocol-version field.
    ModernRequest {
        /// The exact protocol-version spelling supplied by the peer.
        protocol_version: String,
    },
    /// Exact legacy `initialize` with no modern marker.
    LegacyInitialize,
    /// An otherwise modern request that also contains an initialize marker.
    MixedInitializeAndModernMetadata {
        /// The exact protocol-version spelling supplied by the peer.
        protocol_version: String,
    },
    /// A request that provides no complete modern metadata.
    RequestWithoutModernMetadata,
    /// A notification, for which no JSON-RPC response may be emitted.
    Notification,
    /// A JSON-RPC response received where an opening request is required.
    Response,
    /// A malformed opening frame.
    Malformed,
}

/// Modern-version support result after era selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModernVersionSupport {
    /// The modern metadata contained exact MCP 2026-07-28.
    Supported,
    /// The metadata was structurally valid but its version is unsupported.
    Unsupported {
        /// The unnormalized version spelling supplied by the peer.
        received: String,
    },
}

/// Deterministic result of considering one stdio frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StdioEraDecision {
    /// The process selected an era exactly once.
    Selected {
        /// The immutable selected era.
        era: ProtocolEra,
        /// Present only for modern request metadata.
        modern_version: Option<ModernVersionSupport>,
    },
    /// A frame was rejected under an already selected fixed policy or era.
    RejectedUnderSelectedEra {
        /// The fixed or previously selected era.
        era: ProtocolEra,
        /// The exact rejection class.
        reason: StdioEraRejection,
    },
    /// The first frame was rejected and the process closed without an era.
    RejectedAndClosed {
        /// The exact rejection class.
        reason: StdioEraRejection,
    },
    /// A terminal process ignores any attempt to retry classification.
    AlreadyTerminal,
}

/// Typed refusal classes for invalid stdio era-selection traffic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StdioEraRejection {
    /// Legacy initialize and modern metadata were mixed in one opening frame.
    MixedEraMarkers,
    /// A request omitted complete modern metadata where it was required.
    MissingModernMetadata,
    /// A notification arrived where an opening request is required.
    NotificationCannotClassify,
    /// A response arrived where an opening request is required.
    ResponseCannotClassify,
    /// The opening bytes were malformed.
    MalformedOpeningFrame,
    /// Legacy-only policy requires exact legacy initialize as the first frame.
    LegacyInitializeRequired,
    /// Traffic from the opposite era arrived after selection.
    CrossEraTraffic,
}

/// One-shot era classifier owned by a single server-side stdio process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StdioEraClassifier {
    policy: ProtocolPolicy,
    state: StdioEraState,
}

impl StdioEraClassifier {
    /// Creates a classifier with its policy fixed before the first frame.
    #[must_use]
    pub const fn new(policy: ProtocolPolicy) -> Self {
        let state = match policy {
            ProtocolPolicy::Auto => StdioEraState::Unclassified,
            ProtocolPolicy::ModernOnly => StdioEraState::Selected(ProtocolEra::Modern2026),
            ProtocolPolicy::LegacyOnly => StdioEraState::Selected(ProtocolEra::Legacy2024),
        };
        Self { policy, state }
    }

    /// Returns the policy fixed for this process.
    #[must_use]
    pub const fn policy(&self) -> ProtocolPolicy {
        self.policy
    }

    /// Returns the process's current one-shot selection state.
    #[must_use]
    pub const fn state(&self) -> &StdioEraState {
        &self.state
    }

    /// Considers the opening frame without allowing same-process retry.
    pub fn classify_opening(&mut self, frame: StdioOpeningFrame) -> StdioEraDecision {
        match self.state {
            StdioEraState::TerminalWithoutEra => StdioEraDecision::AlreadyTerminal,
            StdioEraState::Unclassified => self.classify_auto(frame),
            StdioEraState::Selected(era) => self.classify_fixed(era, frame),
        }
    }

    fn classify_auto(&mut self, frame: StdioOpeningFrame) -> StdioEraDecision {
        match frame {
            StdioOpeningFrame::ModernRequest { protocol_version } => {
                self.select_modern(protocol_version)
            }
            StdioOpeningFrame::LegacyInitialize => {
                self.state = StdioEraState::Selected(ProtocolEra::Legacy2024);
                StdioEraDecision::Selected {
                    era: ProtocolEra::Legacy2024,
                    modern_version: None,
                }
            }
            StdioOpeningFrame::MixedInitializeAndModernMetadata { .. } => {
                self.reject_and_close(StdioEraRejection::MixedEraMarkers)
            }
            StdioOpeningFrame::RequestWithoutModernMetadata => {
                // Auto: a first frame with no modern metadata is implicit
                // exact-2024 traffic, not a close. JSON-RPC clients (and
                // Agent Mail protocol_compliance) may send tools/list or an
                // unknown method before initialize; that must stay
                // MethodNotFound / dispatch, not InvalidRequest+close.
                self.state = StdioEraState::Selected(ProtocolEra::Legacy2024);
                StdioEraDecision::Selected {
                    era: ProtocolEra::Legacy2024,
                    modern_version: None,
                }
            }
            StdioOpeningFrame::Notification => {
                self.reject_and_close(StdioEraRejection::NotificationCannotClassify)
            }
            StdioOpeningFrame::Response => {
                self.reject_and_close(StdioEraRejection::ResponseCannotClassify)
            }
            StdioOpeningFrame::Malformed => {
                self.reject_and_close(StdioEraRejection::MalformedOpeningFrame)
            }
        }
    }

    fn classify_fixed(&mut self, era: ProtocolEra, frame: StdioOpeningFrame) -> StdioEraDecision {
        match (era, frame) {
            (ProtocolEra::Modern2026, StdioOpeningFrame::ModernRequest { protocol_version }) => {
                Self::modern_decision(protocol_version)
            }
            (ProtocolEra::Modern2026, StdioOpeningFrame::LegacyInitialize) => {
                StdioEraDecision::RejectedUnderSelectedEra {
                    era,
                    reason: StdioEraRejection::CrossEraTraffic,
                }
            }
            (
                ProtocolEra::Modern2026,
                StdioOpeningFrame::MixedInitializeAndModernMetadata { .. },
            ) => StdioEraDecision::RejectedUnderSelectedEra {
                era,
                reason: StdioEraRejection::MixedEraMarkers,
            },
            (ProtocolEra::Modern2026, StdioOpeningFrame::RequestWithoutModernMetadata) => {
                StdioEraDecision::RejectedUnderSelectedEra {
                    era,
                    reason: StdioEraRejection::MissingModernMetadata,
                }
            }
            (ProtocolEra::Modern2026, StdioOpeningFrame::Notification) => {
                StdioEraDecision::RejectedUnderSelectedEra {
                    era,
                    reason: StdioEraRejection::NotificationCannotClassify,
                }
            }
            (ProtocolEra::Modern2026, StdioOpeningFrame::Response) => {
                StdioEraDecision::RejectedUnderSelectedEra {
                    era,
                    reason: StdioEraRejection::ResponseCannotClassify,
                }
            }
            (ProtocolEra::Modern2026, StdioOpeningFrame::Malformed) => {
                StdioEraDecision::RejectedUnderSelectedEra {
                    era,
                    reason: StdioEraRejection::MalformedOpeningFrame,
                }
            }
            (ProtocolEra::Legacy2024, StdioOpeningFrame::LegacyInitialize) => {
                StdioEraDecision::Selected {
                    era,
                    modern_version: None,
                }
            }
            (
                ProtocolEra::Legacy2024,
                StdioOpeningFrame::RequestWithoutModernMetadata | StdioOpeningFrame::Notification,
            ) => StdioEraDecision::Selected {
                era,
                modern_version: None,
            },
            (
                ProtocolEra::Legacy2024,
                StdioOpeningFrame::ModernRequest { .. }
                | StdioOpeningFrame::MixedInitializeAndModernMetadata { .. },
            ) => StdioEraDecision::RejectedUnderSelectedEra {
                era,
                reason: StdioEraRejection::CrossEraTraffic,
            },
            (ProtocolEra::Legacy2024, _) => StdioEraDecision::RejectedUnderSelectedEra {
                era,
                reason: StdioEraRejection::LegacyInitializeRequired,
            },
        }
    }

    fn select_modern(&mut self, protocol_version: String) -> StdioEraDecision {
        self.state = StdioEraState::Selected(ProtocolEra::Modern2026);
        Self::modern_decision(protocol_version)
    }

    fn modern_decision(protocol_version: String) -> StdioEraDecision {
        let modern_version = match ProtocolVersion::parse(&protocol_version) {
            Ok(ProtocolVersion::MODERN_2026) => ModernVersionSupport::Supported,
            Ok(ProtocolVersion::LEGACY_2024)
            | Err(ProtocolVersionError::UnsupportedVersion { .. }) => {
                ModernVersionSupport::Unsupported {
                    received: protocol_version,
                }
            }
        };
        StdioEraDecision::Selected {
            era: ProtocolEra::Modern2026,
            modern_version: Some(modern_version),
        }
    }

    fn reject_and_close(&mut self, reason: StdioEraRejection) -> StdioEraDecision {
        self.state = StdioEraState::TerminalWithoutEra;
        StdioEraDecision::RejectedAndClosed { reason }
    }
}

/// One explicit configured HTTP route before protocol parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HttpRouteKind {
    /// Modern Streamable HTTP MCP POST route.
    ModernMcpPost,
    /// Exact legacy SSE GET route.
    LegacySseGet,
    /// Exact legacy message POST route.
    LegacyMessagePost,
}

impl HttpRouteKind {
    const fn method(self) -> &'static str {
        match self {
            Self::ModernMcpPost | Self::LegacyMessagePost => "POST",
            Self::LegacySseGet => "GET",
        }
    }

    const fn display_name(self) -> &'static str {
        match self {
            Self::ModernMcpPost => "modern MCP POST",
            Self::LegacySseGet => "legacy SSE GET",
            Self::LegacyMessagePost => "legacy message POST",
        }
    }
}

impl fmt::Display for HttpRouteKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.display_name())
    }
}

/// Immutable, configured HTTP endpoint bundle for one policy selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpEndpointBundle {
    key: HttpEndpointBundleKey,
}

/// Opaque key for HTTP era negotiation and cached classification state.
///
/// Equality includes complete canonical targets, including path and query, as
/// well as the opaque partition/profile/generation values. Origin equality by
/// itself is never bundle identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HttpEndpointBundleKey {
    modern_post_target: Option<String>,
    legacy_sse_target: Option<String>,
    legacy_message_post_target: Option<String>,
    credential_partition: String,
    security_partition: String,
    transport_profile: String,
    policy: ProtocolPolicy,
    policy_generation: u64,
    configuration_generation: u64,
    legacy_receipt_generation: u64,
}

/// Typed refusal while constructing a configured HTTP endpoint bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpEndpointBundleError {
    /// A policy requiring modern HTTP lacked its explicit configured POST target.
    MissingModernPostTarget {
        /// The rejected policy.
        policy: ProtocolPolicy,
    },
    /// A policy requiring legacy HTTP lacked its configured SSE GET target.
    MissingLegacySseTarget {
        /// The rejected policy.
        policy: ProtocolPolicy,
    },
    /// A policy requiring legacy HTTP lacked its configured message POST target.
    MissingLegacyMessagePostTarget {
        /// The rejected policy.
        policy: ProtocolPolicy,
    },
    /// A configured endpoint contained a fragment, which is never sent in HTTP.
    FragmentNotAllowed {
        /// The route that supplied the invalid target.
        route: HttpRouteKind,
    },
    /// Two configured routes have the same method and exact canonical target.
    RouteCollision {
        /// The first colliding route.
        first: HttpRouteKind,
        /// The second colliding route.
        second: HttpRouteKind,
        /// The colliding full canonical target.
        target: String,
    },
}

impl fmt::Display for HttpEndpointBundleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingModernPostTarget { policy } => write!(
                formatter,
                "protocol policy {} requires a configured modern MCP POST target",
                policy.display_name()
            ),
            Self::MissingLegacySseTarget { policy } => write!(
                formatter,
                "protocol policy {} requires a configured legacy SSE GET target",
                policy.display_name()
            ),
            Self::MissingLegacyMessagePostTarget { policy } => write!(
                formatter,
                "protocol policy {} requires a configured legacy message POST target",
                policy.display_name()
            ),
            Self::FragmentNotAllowed { route } => write!(
                formatter,
                "configured {route} target must not contain a fragment"
            ),
            Self::RouteCollision {
                first,
                second,
                target,
            } => write!(
                formatter,
                "configured {first} and {second} routes collide at {target}"
            ),
        }
    }
}

impl std::error::Error for HttpEndpointBundleError {}

impl HttpEndpointBundle {
    /// Builds a trusted bundle exclusively from configured canonical targets.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        policy: ProtocolPolicy,
        modern_post: Option<CanonicalHttpUrl>,
        legacy_sse: Option<CanonicalHttpUrl>,
        legacy_message_post: Option<CanonicalHttpUrl>,
        credential_partition: String,
        security_partition: String,
        transport_profile: String,
        policy_generation: u64,
        configuration_generation: u64,
        legacy_receipt_generation: u64,
    ) -> Result<Self, HttpEndpointBundleError> {
        let requires_modern = !matches!(policy, ProtocolPolicy::LegacyOnly);
        let requires_legacy = !matches!(policy, ProtocolPolicy::ModernOnly);

        if requires_modern && modern_post.is_none() {
            return Err(HttpEndpointBundleError::MissingModernPostTarget { policy });
        }
        if requires_legacy && legacy_sse.is_none() {
            return Err(HttpEndpointBundleError::MissingLegacySseTarget { policy });
        }
        if requires_legacy && legacy_message_post.is_none() {
            return Err(HttpEndpointBundleError::MissingLegacyMessagePostTarget { policy });
        }

        Self::reject_fragment(modern_post.as_ref(), HttpRouteKind::ModernMcpPost)?;
        Self::reject_fragment(legacy_sse.as_ref(), HttpRouteKind::LegacySseGet)?;
        Self::reject_fragment(
            legacy_message_post.as_ref(),
            HttpRouteKind::LegacyMessagePost,
        )?;

        let key = HttpEndpointBundleKey {
            modern_post_target: modern_post.map(|target| target.as_str().to_owned()),
            legacy_sse_target: legacy_sse.map(|target| target.as_str().to_owned()),
            legacy_message_post_target: legacy_message_post
                .map(|target| target.as_str().to_owned()),
            credential_partition,
            security_partition,
            transport_profile,
            policy,
            policy_generation,
            configuration_generation,
            legacy_receipt_generation,
        };
        Self::reject_route_collisions(&key)?;
        Ok(Self { key })
    }

    /// Returns the immutable opaque cache key for this configured bundle.
    #[must_use]
    pub fn key(&self) -> HttpEndpointBundleKey {
        self.key.clone()
    }

    fn reject_fragment(
        target: Option<&CanonicalHttpUrl>,
        route: HttpRouteKind,
    ) -> Result<(), HttpEndpointBundleError> {
        if target.is_some_and(|target| target.fragment().is_some()) {
            return Err(HttpEndpointBundleError::FragmentNotAllowed { route });
        }
        Ok(())
    }

    fn reject_route_collisions(key: &HttpEndpointBundleKey) -> Result<(), HttpEndpointBundleError> {
        let routes = [
            (
                HttpRouteKind::ModernMcpPost,
                key.modern_post_target.as_deref(),
            ),
            (
                HttpRouteKind::LegacySseGet,
                key.legacy_sse_target.as_deref(),
            ),
            (
                HttpRouteKind::LegacyMessagePost,
                key.legacy_message_post_target.as_deref(),
            ),
        ];
        for (index, (first_kind, first_target)) in routes.iter().enumerate() {
            let Some(first_target) = first_target else {
                continue;
            };
            for (second_kind, second_target) in routes.iter().skip(index + 1) {
                if first_kind.method() == second_kind.method()
                    && second_target.is_some_and(|second_target| second_target == *first_target)
                {
                    return Err(HttpEndpointBundleError::RouteCollision {
                        first: *first_kind,
                        second: *second_kind,
                        target: (*first_target).to_owned(),
                    });
                }
            }
        }
        Ok(())
    }
}

/// The body category returned by one isolated modern HTTP probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpProbeBody {
    /// A recognized modern JSON-RPC result or error object.
    RecognizedModernJsonRpc,
    /// An empty response body.
    Empty,
    /// A body that is not a recognized modern JSON-RPC result or error.
    Unrecognized,
    /// No response was received because of a transport failure.
    TransportFailure,
}

/// One isolated response to the configured modern HTTP probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HttpModernProbe {
    /// The received HTTP status, when a response was received.
    pub status: u16,
    /// The classified body form.
    pub body: HttpProbeBody,
}

/// Era classification result for a configured HTTP bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpEraDecision {
    /// The bundle selected this era and may cache it under its exact key.
    Selected(ProtocolEra),
    /// The modern probe permits one configured legacy SSE observation.
    ///
    /// This is not an era selection and must never be cached. Only a later
    /// validated legacy SSE endpoint event may select exact MCP 2024-11-05.
    LegacySseFallbackAuthorized,
    /// The response cannot signal downgrade and must not trigger legacy GET.
    RejectedWithoutLegacyFallback,
}

/// Per-bundle era cache that cannot be contaminated by origin-only matches.
#[derive(Debug, Default)]
pub struct HttpEraCache {
    selected_eras: HashMap<HttpEndpointBundleKey, ProtocolEra>,
}

impl HttpEraCache {
    /// Classifies one configured bundle once or returns its immutable cached era.
    pub fn classify_or_cached(
        &mut self,
        bundle: &HttpEndpointBundle,
        probe: HttpModernProbe,
    ) -> HttpEraDecision {
        let key = bundle.key();
        if let Some(era) = self.selected_eras.get(&key) {
            return HttpEraDecision::Selected(*era);
        }

        let decision = Self::classify_probe(bundle.key.policy, probe);
        if let HttpEraDecision::Selected(era) = decision {
            self.selected_eras.insert(key, era);
        }
        decision
    }

    /// Explicit trusted invalidation removes only one exact bundle key.
    pub fn invalidate(&mut self, key: &HttpEndpointBundleKey) -> Option<ProtocolEra> {
        self.selected_eras.remove(key)
    }

    /// Returns an era only for the exact full bundle key.
    #[must_use]
    pub fn selected_era(&self, key: &HttpEndpointBundleKey) -> Option<ProtocolEra> {
        self.selected_eras.get(key).copied()
    }

    fn classify_probe(policy: ProtocolPolicy, probe: HttpModernProbe) -> HttpEraDecision {
        match policy {
            ProtocolPolicy::ModernOnly
                if matches!(probe.body, HttpProbeBody::RecognizedModernJsonRpc) =>
            {
                HttpEraDecision::Selected(ProtocolEra::Modern2026)
            }
            ProtocolPolicy::ModernOnly => HttpEraDecision::RejectedWithoutLegacyFallback,
            ProtocolPolicy::LegacyOnly => HttpEraDecision::Selected(ProtocolEra::Legacy2024),
            ProtocolPolicy::Auto
                if matches!(probe.body, HttpProbeBody::RecognizedModernJsonRpc) =>
            {
                HttpEraDecision::Selected(ProtocolEra::Modern2026)
            }
            ProtocolPolicy::Auto
                if matches!(probe.status, 400 | 404 | 405)
                    && matches!(
                        probe.body,
                        HttpProbeBody::Empty | HttpProbeBody::Unrecognized
                    ) =>
            {
                HttpEraDecision::LegacySseFallbackAuthorized
            }
            ProtocolPolicy::Auto => HttpEraDecision::RejectedWithoutLegacyFallback,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn protocol_version_serde_uses_only_exact_wire_versions() {
        for (version, wire_value) in [
            (ProtocolVersion::MODERN_2026, MODERN_PROTOCOL_VERSION),
            (ProtocolVersion::LEGACY_2024, LEGACY_PROTOCOL_VERSION),
        ] {
            assert_eq!(
                serde_json::to_value(version).expect("supported version serializes"),
                serde_json::json!(wire_value),
            );
            assert_eq!(
                serde_json::from_value::<ProtocolVersion>(serde_json::json!(wire_value))
                    .expect("exact supported wire version deserializes"),
                version,
            );
        }
    }

    #[test]
    fn protocol_version_serde_planted_negative_rejects_internal_variant_spelling() {
        let accepted_wire_value = serde_json::json!(MODERN_PROTOCOL_VERSION);
        let accepted = serde_json::from_value::<ProtocolVersion>(accepted_wire_value.clone())
            .expect("exact modern wire version is admitted");

        // The wire spelling is the only changed dimension. Internal enum
        // labels are never protocol-version aliases.
        let rejected = serde_json::from_value::<ProtocolVersion>(serde_json::json!("Modern2026"))
            .expect_err("internal enum spelling must not be admitted on the wire");

        assert!(
            rejected
                .to_string()
                .contains("unsupported MCP protocol version \"Modern2026\"")
        );
        assert_eq!(accepted, ProtocolVersion::MODERN_2026);
        assert_eq!(
            serde_json::to_value(accepted).expect("accepted version remains serializable"),
            accepted_wire_value,
        );
    }

    #[test]
    fn auto_stdio_request_without_modern_metadata_selects_legacy() {
        let mut classifier = StdioEraClassifier::new(ProtocolPolicy::Auto);
        assert_eq!(
            classifier.classify_opening(StdioOpeningFrame::RequestWithoutModernMetadata),
            StdioEraDecision::Selected {
                era: ProtocolEra::Legacy2024,
                modern_version: None,
            }
        );
        assert_eq!(
            classifier.state(),
            &StdioEraState::Selected(ProtocolEra::Legacy2024)
        );
        assert_eq!(
            classifier.classify_opening(StdioOpeningFrame::RequestWithoutModernMetadata),
            StdioEraDecision::Selected {
                era: ProtocolEra::Legacy2024,
                modern_version: None,
            }
        );
    }

    #[test]
    fn auto_stdio_planted_negative_treats_exact_legacy_claim_as_modern_contradiction() {
        let mut accepted = StdioEraClassifier::new(ProtocolPolicy::Auto);
        assert_eq!(
            accepted.classify_opening(StdioOpeningFrame::ModernRequest {
                protocol_version: MODERN_PROTOCOL_VERSION.to_owned(),
            }),
            StdioEraDecision::Selected {
                era: ProtocolEra::Modern2026,
                modern_version: Some(ModernVersionSupport::Supported),
            }
        );
        let accepted_state = accepted.state().clone();

        let mut contradictory = StdioEraClassifier::new(ProtocolPolicy::Auto);
        assert_eq!(
            contradictory.classify_opening(StdioOpeningFrame::ModernRequest {
                protocol_version: LEGACY_PROTOCOL_VERSION.to_owned(),
            }),
            StdioEraDecision::Selected {
                era: ProtocolEra::Modern2026,
                modern_version: Some(ModernVersionSupport::Unsupported {
                    received: LEGACY_PROTOCOL_VERSION.to_owned(),
                }),
            }
        );
        assert_eq!(
            contradictory.classify_opening(StdioOpeningFrame::LegacyInitialize),
            StdioEraDecision::RejectedUnderSelectedEra {
                era: ProtocolEra::Modern2026,
                reason: StdioEraRejection::CrossEraTraffic,
            }
        );
        assert_eq!(accepted.state(), &accepted_state);
        assert_eq!(
            contradictory.state(),
            &StdioEraState::Selected(ProtocolEra::Modern2026)
        );
    }

    #[test]
    fn auto_http_planted_negative_does_not_downgrade_a_recognized_modern_refusal() {
        let bundle = HttpEndpointBundle::new(
            ProtocolPolicy::Auto,
            Some(CanonicalHttpUrl::parse("https://api.example.test/mcp").unwrap()),
            Some(CanonicalHttpUrl::parse("https://api.example.test/sse").unwrap()),
            Some(CanonicalHttpUrl::parse("https://api.example.test/messages").unwrap()),
            "credential-partition-a".to_owned(),
            "security-partition-a".to_owned(),
            "http-sse-v2".to_owned(),
            1,
            1,
            1,
        )
        .expect("complete Auto bundle is valid");

        let mut accepted = HttpEraCache::default();
        assert_eq!(
            accepted.classify_or_cached(
                &bundle,
                HttpModernProbe {
                    status: 200,
                    body: HttpProbeBody::RecognizedModernJsonRpc,
                },
            ),
            HttpEraDecision::Selected(ProtocolEra::Modern2026)
        );
        let accepted_era = accepted.selected_era(&bundle.key());

        // The HTTP status is the sole changed dimension. A recognized modern
        // JSON-RPC response confirms the modern era even when it refuses the
        // discovery request, so it cannot authorize legacy fallback.
        let mut refusal = HttpEraCache::default();
        assert_eq!(
            refusal.classify_or_cached(
                &bundle,
                HttpModernProbe {
                    status: 404,
                    body: HttpProbeBody::RecognizedModernJsonRpc,
                },
            ),
            HttpEraDecision::Selected(ProtocolEra::Modern2026)
        );
        assert_eq!(accepted.selected_era(&bundle.key()), accepted_era);
        assert_eq!(
            refusal.selected_era(&bundle.key()),
            Some(ProtocolEra::Modern2026)
        );
    }

    pub(crate) fn fnd_03_policy_receipts_positive() {
        assert_eq!(
            ProtocolVersion::parse(MODERN_PROTOCOL_VERSION),
            Ok(ProtocolVersion::MODERN_2026)
        );
        assert_eq!(
            ProtocolVersion::parse(LEGACY_PROTOCOL_VERSION),
            Ok(ProtocolVersion::LEGACY_2024)
        );
        assert_eq!(ProtocolVersion::MODERN_2026.era(), ProtocolEra::Modern2026);
        assert_eq!(ProtocolVersion::LEGACY_2024.era(), ProtocolEra::Legacy2024);

        let policy = ProtocolPolicy::default();
        assert_eq!(policy, ProtocolPolicy::Auto);
        assert_eq!(
            policy.supported_versions(),
            [ProtocolVersion::MODERN_2026, ProtocolVersion::LEGACY_2024]
        );
        assert_eq!(
            policy.modern_discovery_versions(),
            [ProtocolVersion::MODERN_2026]
        );
        assert_eq!(
            ProtocolPolicy::LegacyOnly.modern_discovery_versions().len(),
            0
        );
        assert_eq!(policy.preferred_versions(), policy.supported_versions());
        assert!(policy.permits(ProtocolVersion::MODERN_2026));
        assert!(policy.permits(ProtocolVersion::LEGACY_2024));

        let modern_client = ProtocolPolicy::ModernOnly
            .validate_for_client(None)
            .expect("modern-only client policy requires no legacy receipt");
        let modern_server = ProtocolPolicy::ModernOnly
            .validate_for_server(None)
            .expect("modern-only server policy requires no legacy receipt");
        assert_eq!(modern_client.policy(), ProtocolPolicy::ModernOnly);
        assert_eq!(modern_client.role(), ProtocolRole::Client);
        assert_eq!(modern_server.policy(), ProtocolPolicy::ModernOnly);
        assert_eq!(modern_server.role(), ProtocolRole::Server);
    }

    pub(crate) fn fnd_03_policy_receipts_planted_negative() {
        let accepted_policy = ProtocolPolicy::ModernOnly;
        let accepted_state = accepted_policy
            .validate_for_client(None)
            .expect("modern-only baseline must be accepted");
        let state_before_refusal = accepted_state;

        // The selected policy is the sole planted dimension. Neither the role
        // nor the receipt input changes from the accepted baseline.
        let planted_policy = ProtocolPolicy::LegacyOnly;
        let refusal = planted_policy
            .validate_for_client(None)
            .expect_err("legacy-only policy without a receipt must be refused");

        assert_eq!(
            refusal,
            ProtocolPolicyError::FeatureUnavailable {
                policy: ProtocolPolicy::LegacyOnly,
                role: ProtocolRole::Client,
            }
        );
        assert_eq!(accepted_state, state_before_refusal);
        assert_eq!(accepted_state.policy(), ProtocolPolicy::ModernOnly);
        assert_eq!(accepted_state.role(), ProtocolRole::Client);
        assert_eq!(
            ProtocolVersion::parse("2025-11-25"),
            Err(ProtocolVersionError::UnsupportedVersion {
                received: "2025-11-25".to_owned(),
            })
        );
    }

    pub(crate) fn fnd_03_era_classification_positive() {
        let mut stdio = StdioEraClassifier::new(ProtocolPolicy::Auto);
        assert_eq!(stdio.state(), &StdioEraState::Unclassified);
        assert_eq!(
            stdio.classify_opening(StdioOpeningFrame::ModernRequest {
                protocol_version: "2025-11-25".to_owned(),
            }),
            StdioEraDecision::Selected {
                era: ProtocolEra::Modern2026,
                modern_version: Some(ModernVersionSupport::Unsupported {
                    received: "2025-11-25".to_owned(),
                }),
            }
        );
        assert_eq!(
            stdio.state(),
            &StdioEraState::Selected(ProtocolEra::Modern2026)
        );
        assert_eq!(
            stdio.classify_opening(StdioOpeningFrame::LegacyInitialize),
            StdioEraDecision::RejectedUnderSelectedEra {
                era: ProtocolEra::Modern2026,
                reason: StdioEraRejection::CrossEraTraffic,
            }
        );

        let first_bundle = HttpEndpointBundle::new(
            ProtocolPolicy::Auto,
            Some(CanonicalHttpUrl::parse("https://api.example.test/mcp").unwrap()),
            Some(CanonicalHttpUrl::parse("https://api.example.test/sse").unwrap()),
            Some(CanonicalHttpUrl::parse("https://api.example.test/messages").unwrap()),
            "partition-a".to_owned(),
            "security-a".to_owned(),
            "http-sse-v2".to_owned(),
            1,
            1,
            1,
        )
        .unwrap();
        let second_bundle = HttpEndpointBundle::new(
            ProtocolPolicy::Auto,
            Some(CanonicalHttpUrl::parse("https://api.example.test/other-mcp").unwrap()),
            Some(CanonicalHttpUrl::parse("https://api.example.test/other-sse").unwrap()),
            Some(CanonicalHttpUrl::parse("https://api.example.test/other-messages").unwrap()),
            "partition-a".to_owned(),
            "security-a".to_owned(),
            "http-sse-v2".to_owned(),
            1,
            1,
            1,
        )
        .unwrap();
        assert_ne!(first_bundle.key(), second_bundle.key());

        let mut cache = HttpEraCache::default();
        assert_eq!(
            cache.classify_or_cached(
                &first_bundle,
                HttpModernProbe {
                    status: 500,
                    body: HttpProbeBody::RecognizedModernJsonRpc,
                },
            ),
            HttpEraDecision::Selected(ProtocolEra::Modern2026)
        );
        assert_eq!(
            cache.classify_or_cached(
                &second_bundle,
                HttpModernProbe {
                    status: 404,
                    body: HttpProbeBody::Empty,
                },
            ),
            HttpEraDecision::LegacySseFallbackAuthorized
        );
        assert_eq!(
            cache.selected_era(&first_bundle.key()),
            Some(ProtocolEra::Modern2026)
        );
        assert_eq!(cache.selected_era(&second_bundle.key()), None);
    }

    #[test]
    fn auto_http_refusal_authorizes_legacy_observation_without_selecting_or_caching_legacy() {
        let bundle = HttpEndpointBundle::new(
            ProtocolPolicy::Auto,
            Some(CanonicalHttpUrl::parse("https://api.example.test/mcp").unwrap()),
            Some(CanonicalHttpUrl::parse("https://api.example.test/sse").unwrap()),
            Some(CanonicalHttpUrl::parse("https://api.example.test/messages").unwrap()),
            "credential-partition-a".to_owned(),
            "security-partition-a".to_owned(),
            "http-sse-v2".to_owned(),
            1,
            1,
            1,
        )
        .expect("complete Auto bundle is valid");

        let mut cache = HttpEraCache::default();
        assert_eq!(
            cache.classify_or_cached(
                &bundle,
                HttpModernProbe {
                    status: 404,
                    body: HttpProbeBody::Empty,
                },
            ),
            HttpEraDecision::LegacySseFallbackAuthorized
        );
        assert_eq!(cache.selected_era(&bundle.key()), None);
    }

    pub(crate) fn fnd_03_era_classification_planted_negative() {
        let baseline_frame = StdioOpeningFrame::ModernRequest {
            protocol_version: MODERN_PROTOCOL_VERSION.to_owned(),
        };
        let mut accepted_classifier = StdioEraClassifier::new(ProtocolPolicy::Auto);
        assert_eq!(
            accepted_classifier.classify_opening(baseline_frame.clone()),
            StdioEraDecision::Selected {
                era: ProtocolEra::Modern2026,
                modern_version: Some(ModernVersionSupport::Supported),
            }
        );
        let accepted_state = accepted_classifier.state().clone();

        // The initialize marker is the sole planted opening-frame dimension;
        // policy, request shape, and protocol version remain unchanged.
        let mut planted_classifier = StdioEraClassifier::new(ProtocolPolicy::Auto);
        let refusal = planted_classifier.classify_opening(
            StdioOpeningFrame::MixedInitializeAndModernMetadata {
                protocol_version: MODERN_PROTOCOL_VERSION.to_owned(),
            },
        );
        assert_eq!(
            refusal,
            StdioEraDecision::RejectedAndClosed {
                reason: StdioEraRejection::MixedEraMarkers,
            }
        );
        assert_eq!(
            planted_classifier.state(),
            &StdioEraState::TerminalWithoutEra
        );
        assert_eq!(accepted_classifier.state(), &accepted_state);
        assert_eq!(
            planted_classifier.classify_opening(baseline_frame),
            StdioEraDecision::AlreadyTerminal
        );
        assert_eq!(
            planted_classifier.state(),
            &StdioEraState::TerminalWithoutEra
        );
    }
}
