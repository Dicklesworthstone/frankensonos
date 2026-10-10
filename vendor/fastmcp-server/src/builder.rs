//! Server builder for configuring MCP servers.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fmt;
use std::sync::{Arc, Mutex};

use fastmcp_console::config::{BannerStyle, ConsoleConfig, TrafficVerbosity};
use fastmcp_console::stats::ServerStats;
use fastmcp_core::{McpError, McpResult};
use fastmcp_protocol::extensions::ExtensionSettingsCompatibilityResolver;
#[cfg(feature = "apps")]
use fastmcp_protocol::extensions::{
    official_mcp_apps_extension_id, validate_official_mcp_apps_descriptor,
    validate_official_mcp_apps_server_settings,
};
#[cfg(feature = "proxy")]
use fastmcp_protocol::protocol_policy::ProtocolEra;
use fastmcp_protocol::protocol_policy::ProtocolPolicy;
use fastmcp_protocol::{
    LoggingCapability, PromptsCapability, ResourceTemplate, ResourcesCapability,
    ServerCapabilities, ServerExtensionDiscovery, ServerInfo, ToolsCapability,
};
use log::{Level, LevelFilter};

#[cfg(feature = "tasks")]
use crate::FinalTaskRuntime;
use crate::handler::CompletionHandler;
#[cfg(feature = "proxy")]
use crate::handler::{
    FinalProxyPromptHandler, FinalProxyResourceHandler, FinalProxyResourceTemplateHandler,
};
use crate::oauth::OAuthHttpRoutes;
#[cfg(feature = "apps")]
use crate::providers::McpAppsUiResource;
#[cfg(all(test, feature = "proxy"))]
use crate::proxy::ProxyFinalCatalog;
#[cfg(all(feature = "proxy", feature = "tasks"))]
use crate::proxy::ProxyFinalTaskRelay;
#[cfg(feature = "proxy")]
use crate::proxy::{
    ProxyCompletionHandler, ProxyPromptCatalog, ProxyPromptHandler, ProxyResourceCatalog,
    ProxyResourceHandler, ProxyResourceTemplateCatalog, ProxyToolCatalog, ProxyToolHandler,
    ProxyTypedCatalog,
};
#[cfg(feature = "tasks")]
use crate::tasks::FinalTaskRuntimeConfig;
#[cfg(all(test, feature = "tasks"))]
use crate::tasks::SharedTaskManager;
use crate::{
    AuthProvider, DuplicateBehavior, ExtensionHandlerRegistry, FinalSubscriptionRegistry,
    HttpServerConfig, LifespanHooks, LoggingConfig, PromptHandler, ResourceHandler, Router, Server,
    ServerExtensionConfigurationError, ServerExtensionRuntime, ToolHandler,
};
#[cfg(feature = "proxy")]
use crate::{ProxyCatalog, ProxyClient};

/// Default request timeout in seconds.
const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 30;

/// Reserved launch setting written by `fastmcp run` for FastMCP server targets.
const FASTMCP_PROTOCOL_POLICY_ENV: &str = "FASTMCP_PROTOCOL_POLICY";

/// The selected launch policy is malformed, not valid Unicode, or unavailable
/// in the compiled server feature set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerLaunchPolicyError {
    /// The reserved launch setting was present but was not valid Unicode.
    NonUnicode,
    /// The reserved launch setting was not one of the exact supported values.
    InvalidValue,
    /// The selected policy requires a server feature that is not compiled in.
    FeatureUnavailable,
}

impl fmt::Display for ServerLaunchPolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonUnicode => write!(
                formatter,
                "{FASTMCP_PROTOCOL_POLICY_ENV} must be valid Unicode"
            ),
            Self::InvalidValue => write!(
                formatter,
                "{FASTMCP_PROTOCOL_POLICY_ENV} must be auto, modern-only, or legacy-only"
            ),
            Self::FeatureUnavailable => write!(
                formatter,
                "{FASTMCP_PROTOCOL_POLICY_ENV}=auto or legacy-only requires the legacy-2024-11-05 feature"
            ),
        }
    }
}

impl std::error::Error for ServerLaunchPolicyError {}

/// The kind of catalog item a builder refused to register.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistrationKind {
    Tool,
    LegacyTool,
    Resource,
    LegacyResource,
    ResourceTemplate,
    LegacyResourceTemplate,
    Prompt,
    LegacyPrompt,
    /// A mounted child server, named by its prefix.
    Mount,
}

impl fmt::Display for RegistrationKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Tool => "tool",
            Self::LegacyTool => "exact-2024 tool",
            Self::Resource => "resource",
            Self::LegacyResource => "exact-2024 resource",
            Self::ResourceTemplate => "resource template",
            Self::LegacyResourceTemplate => "exact-2024 resource template",
            Self::Prompt => "prompt",
            Self::LegacyPrompt => "exact-2024 prompt",
            Self::Mount => "mount",
        })
    }
}

/// One registration the builder refused. It is kept so the build fails with
/// it instead of starting a server that silently lacks the item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefusedRegistration {
    /// What was being registered.
    pub kind: RegistrationKind,
    /// The tool or prompt name, resource URI, URI template, or mount prefix.
    pub name: String,
    /// Why the registration was refused.
    pub reason: String,
}

impl fmt::Display for RefusedRegistration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} {:?}: {}", self.kind, self.name, self.reason)
    }
}

/// A builder could not produce a server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerBuildError {
    /// One or more registrations were refused, listed in registration order.
    /// No server was built.
    InvalidConfiguration(Vec<RefusedRegistration>),
}

impl fmt::Display for ServerBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfiguration(refused) => {
                write!(
                    formatter,
                    "invalid server configuration: {} refused registration(s)",
                    refused.len()
                )?;
                for registration in refused {
                    write!(formatter, "; {registration}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for ServerBuildError {}

#[cfg(feature = "apps")]
const MOUNT_APPS_REFUSAL: &str = "the child contains MCP Apps components but the destination has no active compatible MCP Apps extension";

/// Reads a registration's name from a host definition hook. A panicking hook
/// yields a placeholder; the router's own admission reports the panic.
fn registration_name(name: impl FnOnce() -> String) -> String {
    crate::catch_extension_unwind(name)
        .unwrap_or_else(|_payload| "<definition hook panicked>".to_owned())
}

fn protocol_policy_from_server_launch_value(
    value: Option<&OsStr>,
) -> Result<Option<ProtocolPolicy>, ServerLaunchPolicyError> {
    match value {
        None => Ok(None),
        Some(value) => match value.to_str() {
            Some("auto") => Ok(Some(ProtocolPolicy::Auto)),
            Some("modern-only") => Ok(Some(ProtocolPolicy::ModernOnly)),
            Some("legacy-only") => Ok(Some(ProtocolPolicy::LegacyOnly)),
            Some(_) => Err(ServerLaunchPolicyError::InvalidValue),
            None => Err(ServerLaunchPolicyError::NonUnicode),
        },
    }
}

fn protocol_policy_from_server_launch_environment()
-> Result<Option<ProtocolPolicy>, ServerLaunchPolicyError> {
    protocol_policy_from_server_launch_value(
        std::env::var_os(FASTMCP_PROTOCOL_POLICY_ENV).as_deref(),
    )
}

const fn legacy_protocol_is_available() -> bool {
    cfg!(feature = "legacy-2024-11-05")
}

fn resolve_protocol_policy(
    launch_protocol_policy: Option<ProtocolPolicy>,
    legacy_protocol_available: bool,
) -> Result<ProtocolPolicy, ServerLaunchPolicyError> {
    let protocol_policy = launch_protocol_policy.unwrap_or(if legacy_protocol_available {
        ProtocolPolicy::Auto
    } else {
        ProtocolPolicy::ModernOnly
    });

    if !legacy_protocol_available && !matches!(protocol_policy, ProtocolPolicy::ModernOnly) {
        return Err(ServerLaunchPolicyError::FeatureUnavailable);
    }

    Ok(protocol_policy)
}

/// Builder for configuring an MCP server.
pub struct ServerBuilder {
    info: ServerInfo,
    capabilities: ServerCapabilities,
    router: Router,
    instructions: Option<String>,
    /// Optional modern discovery title. Exact-2024 initialize stays name/version.
    title: Option<String>,
    /// Optional modern discovery description.
    description: Option<String>,
    /// Optional modern discovery website identity.
    website_url: Option<String>,
    /// Optional modern discovery icons.
    icons: Vec<fastmcp_protocol::common_types::RawIcon>,
    /// Request timeout in seconds (0 = no timeout).
    request_timeout_secs: u64,
    /// Whether to enable statistics collection.
    stats_enabled: bool,
    /// Whether to mask internal error details in responses.
    mask_error_details: bool,
    /// Logging configuration.
    logging: LoggingConfig,
    /// Console configuration for rich output.
    console_config: ConsoleConfig,
    /// Lifecycle hooks for startup/shutdown.
    lifespan: LifespanHooks,
    /// Optional authentication provider.
    auth_provider: Option<Arc<dyn AuthProvider>>,
    /// Registered middleware.
    middleware: Vec<Box<dyn crate::Middleware>>,
    /// Test-only legacy task manager. Production builds have no task-manager
    /// field or builder edge.
    #[cfg(all(test, feature = "tasks"))]
    task_manager: Option<SharedTaskManager>,
    /// Behavior when registering duplicate component names.
    on_duplicate: DuplicateBehavior,
    /// Whether to use strict input validation (reject extra properties).
    strict_input_validation: bool,
    /// Per-connection ceiling for concurrent server-to-client requests.
    max_bidirectional_requests_per_connection: usize,
    /// Immutable protocol-era admission policy for live stdio/runtime connections.
    protocol_policy: ProtocolPolicy,
    /// Explicit application vocabulary for legacy tools/call results only.
    #[cfg(any(feature = "legacy-2024-11-05", test))]
    legacy_application_tool_content: bool,
    /// Reserved policy selected before construction by a launch setting or a
    /// sealed embedding component.
    launch_protocol_policy: Option<ProtocolPolicy>,
    /// Immutable configuration for the live dual-era HTTP endpoint.
    http_config: HttpServerConfig,
    /// Optional immutable OAuth-only public HTTP routes.
    oauth_http_routes: Option<OAuthHttpRoutes>,
    /// Installed extension handlers and current server discovery settings.
    extension_runtime: Option<ServerExtensionRuntime>,
    /// Application-owned state for the configured final Tasks extension.
    #[cfg(feature = "tasks")]
    final_task_runtime: Option<FinalTaskRuntime>,
    /// One route-bound upstream final Tasks relay. The official Task methods
    /// cannot disambiguate two independent upstream task-ID namespaces.
    #[cfg(all(feature = "proxy", feature = "tasks"))]
    final_task_relay: Option<Arc<ProxyFinalTaskRelay>>,
    /// Registrations refused so far. A nonempty list fails the build.
    refused_registrations: Vec<RefusedRegistration>,
}

impl ServerBuilder {
    /// Creates a new server builder.
    ///
    /// Statistics collection is enabled by default. Use [`without_stats`](Self::without_stats)
    /// to disable it for performance-critical scenarios.
    ///
    /// Console configuration defaults to environment-based settings. Use
    /// [`with_console_config`](Self::with_console_config) for programmatic control.
    #[must_use]
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self::try_new(name, version).unwrap_or_else(|error| {
            panic!("ServerBuilder::new rejected launch configuration: {error}")
        })
    }

    /// Creates a new server builder after validating the reserved launch policy.
    ///
    /// This is the typed construction boundary for applications that need to
    /// report a malformed or unavailable launch policy instead of panicking.
    pub fn try_new(
        name: impl Into<String>,
        version: impl Into<String>,
    ) -> Result<Self, ServerLaunchPolicyError> {
        Self::from_launch_protocol_policy(
            name,
            version,
            protocol_policy_from_server_launch_environment(),
        )
    }

    /// Creates a builder whose protocol policy is fixed by the embedding
    /// component rather than the process launch environment.
    ///
    /// The selected policy is validated against the compiled feature set, but
    /// this constructor deliberately does not read `FASTMCP_PROTOCOL_POLICY`.
    /// It also reserves the selected policy, so a later
    /// [`protocol_policy`](Self::protocol_policy) call validates its argument
    /// without changing the fixed selection. This is intended for sealed
    /// component facades that expose only one protocol era.
    pub fn try_new_with_fixed_protocol_policy(
        name: impl Into<String>,
        version: impl Into<String>,
        policy: ProtocolPolicy,
    ) -> Result<Self, ServerLaunchPolicyError> {
        let policy = resolve_protocol_policy(Some(policy), legacy_protocol_is_available())?;
        Ok(Self::with_protocol_policy(
            name,
            version,
            policy,
            Some(policy),
        ))
    }

    pub(crate) fn from_launch_protocol_policy(
        name: impl Into<String>,
        version: impl Into<String>,
        launch_protocol_policy: Result<Option<ProtocolPolicy>, ServerLaunchPolicyError>,
    ) -> Result<Self, ServerLaunchPolicyError> {
        let launch_protocol_policy = launch_protocol_policy?;
        let protocol_policy =
            resolve_protocol_policy(launch_protocol_policy, legacy_protocol_is_available())?;
        Ok(Self::with_protocol_policy(
            name,
            version,
            protocol_policy,
            launch_protocol_policy,
        ))
    }

    fn with_protocol_policy(
        name: impl Into<String>,
        version: impl Into<String>,
        protocol_policy: ProtocolPolicy,
        launch_protocol_policy: Option<ProtocolPolicy>,
    ) -> Self {
        let console_config = ConsoleConfig::from_env();
        let logging = LoggingConfig::from(&console_config);
        Self {
            info: ServerInfo {
                name: name.into(),
                version: version.into(),
            },
            capabilities: ServerCapabilities {
                logging: Some(LoggingCapability::default()),
                ..ServerCapabilities::default()
            },
            router: Router::new(),
            instructions: None,
            title: None,
            description: None,
            website_url: None,
            icons: Vec::new(),
            request_timeout_secs: DEFAULT_REQUEST_TIMEOUT_SECS,
            stats_enabled: true,
            mask_error_details: false, // Disabled by default for development
            logging,
            console_config,
            lifespan: LifespanHooks::default(),
            auth_provider: None,
            middleware: Vec::new(),
            #[cfg(all(test, feature = "tasks"))]
            task_manager: None,
            on_duplicate: DuplicateBehavior::default(),
            strict_input_validation: false,
            max_bidirectional_requests_per_connection:
                crate::bidirectional::DEFAULT_MAX_IN_FLIGHT_REQUESTS,
            protocol_policy,
            #[cfg(any(feature = "legacy-2024-11-05", test))]
            legacy_application_tool_content: false,
            launch_protocol_policy,
            http_config: HttpServerConfig::default(),
            oauth_http_routes: None,
            extension_runtime: None,
            #[cfg(feature = "tasks")]
            final_task_runtime: None,
            #[cfg(all(feature = "proxy", feature = "tasks"))]
            final_task_relay: None,
            refused_registrations: Vec::new(),
        }
    }

    /// Sets the behavior when registering duplicate component names.
    ///
    /// Controls what happens when a tool, resource, resource template, prompt,
    /// or mounted component is registered with an identifier that already
    /// exists:
    ///
    /// - [`DuplicateBehavior::Error`]: Refuse the conflicting registration;
    ///   [`try_build`](Self::try_build) then fails naming it
    /// - [`DuplicateBehavior::Warn`]: Log warning, keep original (default)
    /// - [`DuplicateBehavior::Replace`]: Replace with new component
    /// - [`DuplicateBehavior::Ignore`]: Silently keep original
    ///
    /// # Example
    ///
    /// ```ignore
    /// Server::new("demo", "1.0")
    ///     .on_duplicate(DuplicateBehavior::Error)  // Strict mode
    ///     .tool(handler1)
    ///     .tool(handler2)  // Refused if the name conflicts
    ///     .try_build()?;   // Fails, naming the refused tool
    /// ```
    #[must_use]
    pub fn on_duplicate(mut self, behavior: DuplicateBehavior) -> Self {
        self.on_duplicate = behavior;
        self
    }

    /// Sets an authentication provider.
    #[must_use]
    pub fn auth_provider<P: AuthProvider + 'static>(mut self, provider: P) -> Self {
        self.auth_provider = Some(Arc::new(provider));
        self
    }

    /// Disables statistics collection.
    ///
    /// Use this for performance-critical scenarios where the overhead
    /// of atomic operations for stats tracking is undesirable.
    /// The overhead is minimal (typically nanoseconds per request),
    /// so this is rarely needed.
    #[must_use]
    pub fn without_stats(mut self) -> Self {
        self.stats_enabled = false;
        self
    }

    /// Sets the request timeout in seconds.
    ///
    /// Set to 0 to omit the server-owned ceiling. Ambient/request and handler
    /// deadlines are still composed into admission checks, cooperative
    /// checkpoints, and late-result rejection; this setting cannot relax them.
    /// A deadline does not preempt blocking synchronous code or imply that
    /// descendant work has been cancelled and drained. Default is 30 seconds.
    #[must_use]
    pub fn request_timeout(mut self, secs: u64) -> Self {
        self.request_timeout_secs = secs;
        self
    }

    /// Sets the maximum number of in-flight server-to-client requests for one
    /// transport connection.
    ///
    /// # Errors
    ///
    /// Returns `InvalidParams` when `max` is zero or exceeds the hard safety
    /// limit enforced by the bidirectional request tracker.
    pub fn max_bidirectional_requests_per_connection(mut self, max: usize) -> McpResult<Self> {
        crate::bidirectional::PendingRequests::validate_max_in_flight(max)?;
        self.max_bidirectional_requests_per_connection = max;
        Ok(self)
    }

    /// Sets the pagination page size for list methods.
    ///
    /// When set, list methods will return up to `page_size` items and provide an
    /// opaque `nextCursor` for retrieving the next page. When not set (default),
    /// list methods return all items in a single response.
    #[must_use]
    pub fn list_page_size(mut self, page_size: usize) -> Self {
        self.router.set_list_page_size(Some(page_size));
        self
    }

    /// Enables or disables error detail masking.
    ///
    /// When enabled, internal error details are hidden from client responses:
    /// - Stack traces removed
    /// - File paths sanitized
    /// - Internal state not exposed
    /// - Generic "Internal server error" message returned
    ///
    /// Client errors (invalid request, method not found, etc.) are preserved
    /// since they don't contain sensitive internal details.
    ///
    /// Default is `false` (disabled) for development convenience.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let server = Server::new("api", "1.0")
    ///     .mask_error_details(true)  // Always mask in production
    ///     .build();
    /// ```
    #[must_use]
    pub fn mask_error_details(mut self, enabled: bool) -> Self {
        self.mask_error_details = enabled;
        self
    }

    /// Automatically masks error details based on environment.
    ///
    /// Masking is enabled when:
    /// - `FASTMCP_ENV` is set to "production"
    /// - `FASTMCP_MASK_ERRORS` is set to "true" or "1"
    /// - The build is a release build (`cfg!(not(debug_assertions))`)
    ///
    /// Masking is explicitly disabled when:
    /// - `FASTMCP_MASK_ERRORS` is set to "false" or "0"
    ///
    /// # Example
    ///
    /// ```ignore
    /// let server = Server::new("api", "1.0")
    ///     .auto_mask_errors()
    ///     .build();
    /// ```
    #[must_use]
    pub fn auto_mask_errors(mut self) -> Self {
        // Check for explicit override first
        if let Ok(val) = std::env::var("FASTMCP_MASK_ERRORS") {
            match val.to_lowercase().as_str() {
                "true" | "1" | "yes" => {
                    self.mask_error_details = true;
                    return self;
                }
                "false" | "0" | "no" => {
                    self.mask_error_details = false;
                    return self;
                }
                _ => {} // Fall through to other checks
            }
        }

        // Check for production environment
        if let Ok(env) = std::env::var("FASTMCP_ENV") {
            if env.to_lowercase() == "production" {
                self.mask_error_details = true;
                return self;
            }
        }

        // Default: mask in release builds, don't mask in debug builds
        self.mask_error_details = cfg!(not(debug_assertions));
        self
    }

    /// Returns whether error masking is enabled.
    #[must_use]
    pub fn is_error_masking_enabled(&self) -> bool {
        self.mask_error_details
    }

    /// Enables or disables strict input validation.
    ///
    /// When enabled, tool input validation will reject any properties not
    /// explicitly defined in the tool's input schema (enforces `additionalProperties: false`).
    ///
    /// When disabled (default), extra properties are allowed unless the schema
    /// explicitly sets `additionalProperties: false`.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let server = Server::new("api", "1.0")
    ///     .strict_input_validation(true)  // Reject unknown properties
    ///     .build();
    /// ```
    #[must_use]
    pub fn strict_input_validation(mut self, enabled: bool) -> Self {
        self.strict_input_validation = enabled;
        self
    }

    /// Returns whether strict input validation is enabled.
    #[must_use]
    pub fn is_strict_input_validation_enabled(&self) -> bool {
        self.strict_input_validation
    }

    /// Selects the immutable MCP protocol-era policy for live stdio and runtime connections.
    ///
    /// With the exact legacy adapter enabled, the default [`ProtocolPolicy::Auto`] classifies the
    /// first accepted opening frame and then pins that connection to its selected era. Without
    /// that adapter, construction defaults to [`ProtocolPolicy::ModernOnly`]. `ModernOnly` and
    /// `LegacyOnly` reject an opening frame from the other exact supported era before it can enter
    /// request dispatch. In a no-legacy production build, `Auto` and `LegacyOnly` return
    /// [`ServerLaunchPolicyError::FeatureUnavailable`] before either can be stored.
    pub fn protocol_policy(
        mut self,
        policy: ProtocolPolicy,
    ) -> Result<Self, ServerLaunchPolicyError> {
        self.try_set_protocol_policy(policy)?;
        Ok(self)
    }

    /// Preserves the broader application [`fastmcp_protocol::Content`]
    /// vocabulary in legacy `tools/call` responses.
    ///
    /// This explicit compatibility option permits audio and resources with
    /// either, both, or neither optional payload. It does not claim those
    /// responses conform to exact MCP 2024-11-05. Request admission, authority,
    /// cancellation, response metadata validation, and other methods retain
    /// their existing rules. The default and typed exact-legacy router APIs
    /// continue to require exact legacy content.
    #[cfg(any(feature = "legacy-2024-11-05", test))]
    #[must_use]
    pub fn legacy_application_tool_content(mut self, enabled: bool) -> Self {
        self.legacy_application_tool_content = enabled;
        self
    }

    /// Attempts to select the immutable MCP protocol-era policy without
    /// consuming the builder.
    ///
    /// A policy unavailable in the compiled feature set is rejected before
    /// this builder is changed. A reserved launch or component policy still
    /// takes precedence over an explicit builder selection.
    pub fn try_set_protocol_policy(
        &mut self,
        policy: ProtocolPolicy,
    ) -> Result<(), ServerLaunchPolicyError> {
        resolve_protocol_policy(Some(policy), legacy_protocol_is_available())?;
        if self.launch_protocol_policy.is_none() {
            self.protocol_policy = policy;
        }
        Ok(())
    }

    /// Returns the currently configured [`ProtocolPolicy`].
    pub const fn configured_protocol_policy(&self) -> ProtocolPolicy {
        self.protocol_policy
    }

    /// Installs the server's modern-only extension handlers and discovery settings.
    ///
    /// Descriptor registration remains mutable only until [`Self::build`]. The
    /// builder validates the advertised identifiers immediately, then freezes
    /// the handler and descriptor registries together while building the
    /// immutable [`Server`]. Exact MCP 2024-11-05 remains outside this path.
    pub fn extension_registry<R>(
        mut self,
        handlers: ExtensionHandlerRegistry,
        server_discovery: ServerExtensionDiscovery,
        resolver: R,
    ) -> Result<Self, ServerExtensionConfigurationError>
    where
        R: ExtensionSettingsCompatibilityResolver + Send + 'static,
    {
        if self.extension_runtime.is_some() {
            return Err(ServerExtensionConfigurationError::AlreadyInstalled);
        }
        #[cfg(feature = "apps")]
        if let Some(settings) = server_discovery
            .extensions
            .get(&official_mcp_apps_extension_id())
        {
            validate_official_mcp_apps_server_settings(settings)
                .map_err(ServerExtensionConfigurationError::Registry)?;
        }
        #[cfg(feature = "tasks")]
        let mut extension_runtime =
            ServerExtensionRuntime::new(handlers, server_discovery, resolver)?;
        #[cfg(not(feature = "tasks"))]
        let extension_runtime = ServerExtensionRuntime::new(handlers, server_discovery, resolver)?;
        #[cfg(feature = "tasks")]
        if let Some(task_runtime) = self.final_task_runtime.as_ref() {
            extension_runtime.install_final_tasks(task_runtime)?;
        }
        #[cfg(all(feature = "proxy", feature = "tasks"))]
        if self.final_task_runtime.is_none() {
            if let Some(task_relay) = self.final_task_relay.as_ref() {
                extension_runtime.install_proxy_final_tasks(Arc::clone(task_relay))?;
            }
        }
        self.extension_runtime = Some(extension_runtime);
        Ok(self)
    }

    /// Installs the official MCP Apps descriptor and empty server discovery marker.
    ///
    /// Apps owns no client-to-server JSON-RPC method on this surface. This
    /// opt-in therefore configures bilateral capability negotiation and
    /// `server/discover` metadata only. It composes in either order with
    /// [`Self::final_tasks`], preserving an Apps inactive disposition when a
    /// client does not advertise the Apps HTML MIME type.
    #[cfg(feature = "apps")]
    pub fn mcp_apps(mut self) -> Result<Self, ServerExtensionConfigurationError> {
        if let Some(extension_runtime) = self.extension_runtime.as_mut() {
            extension_runtime.install_official_mcp_apps()?;
        } else {
            #[cfg(feature = "tasks")]
            let mut extension_runtime = ServerExtensionRuntime::with_official_mcp_apps()?;
            #[cfg(not(feature = "tasks"))]
            let extension_runtime = ServerExtensionRuntime::with_official_mcp_apps()?;
            #[cfg(feature = "tasks")]
            if let Some(task_runtime) = self.final_task_runtime.as_ref() {
                extension_runtime.install_final_tasks(task_runtime)?;
            }
            #[cfg(all(feature = "proxy", feature = "tasks"))]
            if self.final_task_runtime.is_none() {
                if let Some(task_relay) = self.final_task_relay.as_ref() {
                    extension_runtime.install_proxy_final_tasks(Arc::clone(task_relay))?;
                }
            }
            self.extension_runtime = Some(extension_runtime);
        }
        Ok(self)
    }

    /// Returns whether this destination owns an active, exact official Apps
    /// capability configuration. Merely advertising the official identifier
    /// is insufficient: the frozen descriptor, local enablement, and exact
    /// empty server settings must all be present.
    #[cfg(feature = "apps")]
    fn has_active_official_mcp_apps(&self) -> bool {
        let apps_id = official_mcp_apps_extension_id();
        self.extension_runtime.as_ref().is_some_and(|runtime| {
            runtime.local_enablement.is_enabled(&apps_id)
                && runtime
                    .handlers
                    .descriptor_registry()
                    .descriptor(&apps_id)
                    .is_some_and(|descriptor| {
                        validate_official_mcp_apps_descriptor(descriptor).is_ok()
                    })
                && runtime
                    .server_discovery
                    .extensions
                    .get(&apps_id)
                    .is_some_and(|settings| {
                        validate_official_mcp_apps_server_settings(settings).is_ok()
                    })
        })
    }

    /// Registers one final-only `ui://` HTML document for a negotiated MCP Apps View.
    ///
    /// This resource is deliberately absent from exact MCP 2024-11-05
    /// discovery and reads. Call [`Self::mcp_apps`] first so the server also
    /// advertises the matching modern bilateral extension capability.
    #[cfg(feature = "apps")]
    pub fn mcp_apps_ui_resource(mut self, resource: McpAppsUiResource) -> McpResult<Self> {
        if !self.has_active_official_mcp_apps() {
            return Err(fastmcp_core::McpError::invalid_request(
                "MCP Apps UI resources require ServerBuilder::mcp_apps first",
            ));
        }
        self.router
            .add_mcp_apps_ui_resource_with_behavior(resource, self.on_duplicate)?;
        self.advertise_legacy_resource_subscriptions();
        Ok(self)
    }

    /// Registers a tool with final-only MCP Apps metadata.
    ///
    /// Register the linked [`McpAppsUiResource`] first. The router validates
    /// the tool's typed `_meta.ui.resourceUri` against that exact final Apps
    /// HTML resource before either tool catalog is changed. Unlike
    /// [`Self::tool`], this Apps-specific entry point returns the registration
    /// failure directly.
    #[cfg(feature = "apps")]
    pub fn mcp_apps_tool<H: ToolHandler + 'static>(mut self, handler: H) -> McpResult<Self> {
        if !self.has_active_official_mcp_apps() {
            return Err(fastmcp_core::McpError::invalid_request(
                "MCP Apps tools require ServerBuilder::mcp_apps first",
            ));
        }
        self.router
            .add_mcp_apps_tool_with_behavior(handler, self.on_duplicate)?;
        self.advertise_legacy_tools_list_changed();
        Ok(self)
    }

    /// Installs the official final Tasks extension around application-owned state.
    ///
    /// The supplied runtime owns neither an executor nor a task region. Its
    /// store and notification emitter remain application-owned, while a
    /// caller-owned structured supervisor advances worker state through the
    /// [`FinalTaskRuntime`] retained on the built [`Server`]. This builder
    /// installs the official descriptor, its three client-to-server request
    /// handlers, and its `notifications/tasks` delivery path together.
    #[cfg(feature = "tasks")]
    pub fn final_tasks(
        mut self,
        task_runtime: FinalTaskRuntime,
    ) -> Result<Self, ServerExtensionConfigurationError> {
        if self.final_task_runtime.is_some() || {
            #[cfg(feature = "proxy")]
            {
                self.final_task_relay.is_some()
            }
            #[cfg(not(feature = "proxy"))]
            {
                false
            }
        } {
            return Err(ServerExtensionConfigurationError::FinalTasksAlreadyInstalled);
        }
        if let Some(extension_runtime) = self.extension_runtime.as_mut() {
            extension_runtime.install_final_tasks(&task_runtime)?;
        }
        self.final_task_runtime = Some(task_runtime);
        Ok(self)
    }

    /// Installs a process-local official Tasks runtime when the builder has
    /// no Tasks owner yet.
    ///
    /// A caller-supplied [`Self::final_tasks`] runtime, a proxy Tasks relay,
    /// or the test-only legacy task-manager adapter all suppress this
    /// default. If an existing extension registry already owns the official
    /// Tasks methods, the default is left uninstalled rather than panicking.
    #[cfg(feature = "tasks")]
    fn install_default_in_memory_final_tasks(&mut self) {
        if self.final_task_runtime.is_some() {
            return;
        }
        #[cfg(all(test, feature = "tasks"))]
        if self.task_manager.is_some() {
            return;
        }
        #[cfg(feature = "proxy")]
        if self.final_task_relay.is_some() {
            return;
        }
        let runtime = FinalTaskRuntime::in_memory(
            FinalTaskRuntimeConfig::new(60_000, Some(5_000))
                .expect("default in-memory Tasks timing policy is valid"),
            Arc::new(|_| {}),
        );
        if let Some(extension_runtime) = self.extension_runtime.as_mut() {
            if extension_runtime.install_final_tasks(&runtime).is_err() {
                return;
            }
        }
        self.final_task_runtime = Some(runtime);
    }

    /// Sets configuration for the live dual-era HTTP endpoint.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use fastmcp_server::HttpServerConfig;
    ///
    /// Server::new("demo", "1.0")
    ///     .http_config(HttpServerConfig::new().mcp_path("/api/mcp").max_connections(128))
    ///     .build();
    /// ```
    #[must_use]
    pub fn http_config(mut self, config: HttpServerConfig) -> Self {
        self.http_config = config;
        self
    }

    /// Installs immutable OAuth authorization, token, and revocation routes
    /// into the native HTTP listener.
    ///
    /// The routes retain an explicit public HTTPS endpoint base and are
    /// admitted before MCP request conversion. OIDC/JWKS/ID-token routes are
    /// deliberately not installed here.
    #[must_use]
    pub fn oauth_http_routes(mut self, routes: OAuthHttpRoutes) -> Self {
        self.oauth_http_routes = Some(routes);
        self
    }

    /// Builds a live modern Streamable HTTP endpoint.
    #[cfg(not(any(feature = "legacy-2024-11-05", test)))]
    pub fn build_http_endpoint(
        self,
    ) -> Result<crate::ServerHttpEndpoint, crate::ServerHttpEndpointError> {
        self.try_build()
            .map_err(|error| {
                crate::ServerHttpEndpointError::InvalidConfiguration(error.to_string())
            })?
            .into_http_endpoint()
    }

    /// Builds a live dual-era HTTP endpoint with an exact legacy SSE origin.
    ///
    /// The modern route remains at [`HttpServerConfig::mcp_path`], while the
    /// exact MCP 2024-11-05 SSE route advertises `legacy_origin` plus the
    /// configured legacy message path.
    #[cfg(any(feature = "legacy-2024-11-05", test))]
    pub fn build_http_endpoint(
        self,
        legacy_origin: impl Into<String>,
    ) -> Result<crate::ServerHttpEndpoint, crate::ServerHttpEndpointError> {
        self.try_build()
            .map_err(|error| {
                crate::ServerHttpEndpointError::InvalidConfiguration(error.to_string())
            })?
            .into_http_endpoint(legacy_origin)
    }

    /// Registers a middleware.
    #[must_use]
    pub fn middleware<M: crate::Middleware + 'static>(mut self, middleware: M) -> Self {
        self.middleware.push(Box::new(middleware));
        self
    }

    /// Registers response caching middleware.
    #[must_use]
    pub fn response_caching(self, caching: crate::caching::ResponseCachingMiddleware) -> Self {
        self.middleware(caching)
    }

    /// Registers rate limiting middleware.
    #[must_use]
    pub fn rate_limiting(
        self,
        rate_limiting: crate::rate_limiting::RateLimitingMiddleware,
    ) -> Self {
        self.middleware(rate_limiting)
    }

    /// Registers a transformed tool handler.
    #[must_use]
    pub fn transformed_tool(self, tool: crate::transform::TransformedTool) -> Self {
        self.tool(tool)
    }

    /// Registers a tool handler.
    ///
    /// Duplicate handling is controlled by [`on_duplicate`](Self::on_duplicate).
    /// A refused registration (an inadmissible definition, or a duplicate under
    /// [`DuplicateBehavior::Error`]) is recorded and fails
    /// [`try_build`](Self::try_build); the server never starts without it.
    #[must_use]
    pub fn tool<H: ToolHandler + 'static>(mut self, handler: H) -> Self {
        let name = registration_name(|| handler.definition().name);
        match self
            .router
            .add_tool_with_behavior(handler, self.on_duplicate)
        {
            Ok(()) => self.advertise_legacy_tools_list_changed(),
            Err(error) => self.refuse(RegistrationKind::Tool, name, error),
        }
        self
    }

    fn refuse(&mut self, kind: RegistrationKind, name: String, error: McpError) {
        self.refused_registrations.push(RefusedRegistration {
            kind,
            name,
            reason: error.message,
        });
    }

    fn refuse_mount(&mut self, prefix: Option<&str>, reason: String) {
        self.refused_registrations.push(RefusedRegistration {
            kind: RegistrationKind::Mount,
            name: prefix.unwrap_or_default().to_owned(),
            reason,
        });
    }

    /// Registers an intentionally exact MCP 2024-11-05-only tool handler.
    ///
    /// This does not depend on builder call order or the connection protocol
    /// policy. The tool is available through exact legacy list/call routes and
    /// omitted from all MCP 2026-07-28 catalogs and dispatch. Use [`Self::tool`]
    /// for ordinary dual-era registration; it never falls back to this path
    /// when final schema admission fails.
    #[must_use]
    pub fn legacy_tool<H: ToolHandler + 'static>(mut self, handler: H) -> Self {
        let name = registration_name(|| handler.definition().name);
        match self
            .router
            .add_legacy_tool_with_behavior(handler, self.on_duplicate)
        {
            Ok(()) => self.advertise_legacy_tools_list_changed(),
            Err(error) => self.refuse(RegistrationKind::LegacyTool, name, error),
        }
        self
    }

    /// Registers a resource handler.
    ///
    /// Duplicate handling is controlled by [`on_duplicate`](Self::on_duplicate).
    /// A refused registration is recorded and fails
    /// [`try_build`](Self::try_build), as for [`Self::tool`].
    #[must_use]
    pub fn resource<H: ResourceHandler + 'static>(mut self, handler: H) -> Self {
        let name = registration_name(|| handler.definition().uri);
        match self
            .router
            .add_resource_with_behavior(handler, self.on_duplicate)
        {
            Ok(()) => self.advertise_legacy_resource_subscriptions(),
            Err(error) => self.refuse(RegistrationKind::Resource, name, error),
        }
        self
    }

    /// Advertises the `resources.subscribe` capability so clients may
    /// subscribe to registered resource URIs.
    ///
    /// Registering a resource or template already advertises subscribe because
    /// session and exact-2024 dispatch serve `resources/subscribe` for those
    /// URIs. This remains for servers that want the capability visible before
    /// any catalog entry is installed.
    #[must_use]
    pub fn resource_subscriptions(mut self) -> Self {
        self.advertise_legacy_resource_subscriptions();
        self
    }

    fn advertise_legacy_resource_subscriptions(&mut self) {
        let resources = self
            .capabilities
            .resources
            .get_or_insert_with(ResourcesCapability::default);
        resources.subscribe = true;
        resources.list_changed = true;
    }

    fn advertise_legacy_tools_list_changed(&mut self) {
        self.capabilities
            .tools
            .get_or_insert_with(ToolsCapability::default)
            .list_changed = true;
    }

    fn advertise_legacy_prompts_list_changed(&mut self) {
        self.capabilities
            .prompts
            .get_or_insert_with(PromptsCapability::default)
            .list_changed = true;
    }

    fn advertise_completions(&mut self) {
        self.capabilities.completions = Some(fastmcp_protocol::CompletionsCapability::default());
    }

    /// Registers an intentionally exact MCP 2024-11-05-only resource.
    #[must_use]
    pub fn legacy_resource<H: ResourceHandler + 'static>(mut self, handler: H) -> Self {
        let name = registration_name(|| handler.definition().uri);
        match self
            .router
            .add_legacy_resource_with_behavior(handler, self.on_duplicate)
        {
            Ok(()) => self.advertise_legacy_resource_subscriptions(),
            Err(error) => self.refuse(RegistrationKind::LegacyResource, name, error),
        }
        self
    }

    /// Registers a resource template.
    ///
    /// Duplicate handling is controlled by [`on_duplicate`](Self::on_duplicate).
    /// A refused template is recorded and fails [`try_build`](Self::try_build).
    #[must_use]
    pub fn resource_template(mut self, template: ResourceTemplate) -> Self {
        let name = template.uri_template.clone();
        match self
            .router
            .add_resource_template_with_behavior(template, self.on_duplicate)
        {
            Ok(()) => self.advertise_legacy_resource_subscriptions(),
            Err(error) => self.refuse(RegistrationKind::ResourceTemplate, name, error),
        }
        self
    }

    /// Registers an intentionally exact MCP 2024-11-05-only resource template.
    #[must_use]
    pub fn legacy_resource_template(mut self, template: ResourceTemplate) -> Self {
        let name = template.uri_template.clone();
        match self
            .router
            .add_legacy_resource_template_with_behavior(template, self.on_duplicate)
        {
            Ok(()) => self.advertise_legacy_resource_subscriptions(),
            Err(error) => self.refuse(RegistrationKind::LegacyResourceTemplate, name, error),
        }
        self
    }

    /// Registers a prompt handler.
    ///
    /// Duplicate handling is controlled by [`on_duplicate`](Self::on_duplicate).
    /// A refused registration is recorded and fails
    /// [`try_build`](Self::try_build), as for [`Self::tool`].
    #[must_use]
    pub fn prompt<H: PromptHandler + 'static>(mut self, handler: H) -> Self {
        let name = registration_name(|| handler.definition().name);
        match self
            .router
            .add_prompt_with_behavior(handler, self.on_duplicate)
        {
            Ok(()) => self.advertise_legacy_prompts_list_changed(),
            Err(error) => self.refuse(RegistrationKind::Prompt, name, error),
        }
        self
    }

    /// Registers an intentionally exact MCP 2024-11-05-only prompt.
    #[must_use]
    pub fn legacy_prompt<H: PromptHandler + 'static>(mut self, handler: H) -> Self {
        let name = registration_name(|| handler.definition().name);
        match self
            .router
            .add_legacy_prompt_with_behavior(handler, self.on_duplicate)
        {
            Ok(()) => self.advertise_legacy_prompts_list_changed(),
            Err(error) => self.refuse(RegistrationKind::LegacyPrompt, name, error),
        }
        self
    }

    /// Registers the server-wide `completion/complete` handler.
    ///
    /// The handler receives disjoint exact-legacy and final request parameter
    /// types. Building with a handler installs the real router dispatch target,
    /// which is the sole condition that enables final discovery's
    /// `capabilities.completions` claim.
    #[must_use]
    pub fn completion_handler<H: CompletionHandler + 'static>(mut self, handler: H) -> Self {
        self.router.add_completion_handler(handler);
        self.advertise_completions();
        self
    }

    /// Registers a completion handler for exact MCP 2024-11-05 dispatch only.
    ///
    /// Initialize advertises `capabilities.completions` so a 2024-11-05 client
    /// can discover `completion/complete`. Final `server/discover` still omits
    /// completions unless a modern handler is also installed.
    #[must_use]
    pub fn legacy_completion_handler<H: CompletionHandler + 'static>(mut self, handler: H) -> Self {
        self.router.add_legacy_completion_handler(handler);
        self.advertise_completions();
        self
    }

    /// Registers a final completion provider for one exact prompt name.
    ///
    /// Final `completion/complete` dispatch validates the referenced prompt
    /// and argument before selecting this provider. Exact MCP 2024-11-05
    /// completion remains on [`Self::completion_handler`] or
    /// [`Self::legacy_completion_handler`].
    #[must_use]
    pub fn prompt_completion_handler<H: CompletionHandler + 'static>(
        mut self,
        prompt_name: impl Into<String>,
        handler: H,
    ) -> Self {
        self.router
            .add_prompt_completion_handler(prompt_name, handler);
        self
    }

    /// Registers a final completion provider for one exact resource-template URI.
    ///
    /// Final dispatch admits the registered resource template and requested
    /// template variable before selecting this provider.
    #[must_use]
    pub fn resource_template_completion_handler<H: CompletionHandler + 'static>(
        mut self,
        uri_template: impl Into<String>,
        handler: H,
    ) -> Self {
        self.router
            .add_resource_template_completion_handler(uri_template, handler);
        self
    }

    /// Registers an exact MCP 2024-11-05 completion provider for one resource
    /// template URI.
    ///
    /// Exact-2024 dispatch selects this provider before the server-wide
    /// [`Self::legacy_completion_handler`] fallback. Initialize advertises
    /// `capabilities.completions` so a 2024-11-05 client can discover
    /// `completion/complete`.
    #[must_use]
    pub fn legacy_resource_template_completion_handler<H: CompletionHandler + 'static>(
        mut self,
        uri_template: impl Into<String>,
        handler: H,
    ) -> Self {
        self.router
            .add_legacy_resource_template_completion_handler(uri_template, handler);
        self.advertise_completions();
        self
    }

    /// Returns whether this proxy registration will own the exact prompt
    /// target after the configured duplicate policy is applied.
    ///
    /// Router registration intentionally returns `Ok(())` when `Warn` or
    /// `Ignore` retains an existing target. Completion providers must not use
    /// that successful no-op as authorization to bind an upstream to the
    /// retained local target.
    #[cfg(feature = "proxy")]
    fn proxy_prompt_wins_admission(&self, name: &str) -> bool {
        self.router.get_prompt(name).is_none() || self.on_duplicate == DuplicateBehavior::Replace
    }

    /// Returns whether this proxy registration will own the exact resource
    /// template target after the configured duplicate policy is applied.
    ///
    /// See [`Self::proxy_prompt_wins_admission`] for why `Ok(())` alone is
    /// insufficient for proxy completion binding.
    #[cfg(feature = "proxy")]
    fn proxy_resource_template_wins_admission(&self, uri_template: &str) -> bool {
        self.router.get_resource_template(uri_template).is_none()
            || self.on_duplicate == DuplicateBehavior::Replace
    }

    /// Skips one colliding catalog member under [`DuplicateBehavior::Error`].
    /// Other admission failures still fail `proxy` / `as_proxy_typed` so a
    /// schema or metadata defect is not logged away as a duplicate skip.
    #[cfg(feature = "proxy")]
    fn absorb_prefixed_proxy_duplicate(
        error: fastmcp_core::McpError,
    ) -> Result<(), fastmcp_core::McpError> {
        if crate::router::is_duplicate_registration_error(&error) {
            Ok(())
        } else {
            Err(error)
        }
    }

    /// Installs one route-bound modern Tasks relay into every server surface
    /// that owns Tasks discovery and dispatch. Exact-2024 never supplies a
    /// relay, so callers pass `None` for its selected upstream era.
    #[cfg(all(feature = "proxy", feature = "tasks"))]
    fn install_proxy_final_tasks_relay(
        &mut self,
        task_relay: Option<Arc<ProxyFinalTaskRelay>>,
    ) -> McpResult<()> {
        let Some(task_relay) = task_relay else {
            return Ok(());
        };
        if self.final_task_runtime.is_some() || self.final_task_relay.is_some() {
            return Err(fastmcp_core::McpError::invalid_request(
                "a server may install only one local or route-bound final Tasks service",
            ));
        }
        if let Some(extension_runtime) = self.extension_runtime.as_mut() {
            extension_runtime
                .install_proxy_final_tasks(Arc::clone(&task_relay))
                .map_err(|error| fastmcp_core::McpError::invalid_request(error.to_string()))?;
        }
        self.router
            .set_final_task_relay(Some(Arc::clone(&task_relay)));
        self.final_task_relay = Some(task_relay);
        Ok(())
    }

    /// Registers proxy handlers for a remote MCP server.
    ///
    /// Use [`ProxyClient::catalog`] to fetch definitions before calling this
    /// method. A caller-supplied catalog must agree with an already-selected
    /// transport binding or backend-observed era; it cannot bind an unbound
    /// proxy route itself.
    ///
    /// # Errors
    ///
    /// Returns an error when the catalog is malformed, contradicts the route,
    /// or attempts to establish an era without upstream evidence.
    #[cfg(feature = "proxy")]
    pub fn proxy(mut self, client: ProxyClient, catalog: ProxyCatalog) -> McpResult<Self> {
        client.admit_catalog(&catalog)?;
        let catalog_era = catalog.era()?;
        let completion_supported = match client.supports_completion() {
            Ok(supported) => supported,
            Err(error) => {
                log::error!(
                    target: "fastmcp_rust::builder",
                    "Failed to determine proxied completion support; code={:?}",
                    error.code
                );
                false
            }
        };
        // An admitted final-only catalog owns the same public core capability
        // claims as its legacy counterpart. Registration below remains era
        // separated; this only makes discovery reflect the handlers that were
        // actually installed.
        let has_tools = !catalog.tools.is_empty() || !catalog.final_tools.is_empty();
        let has_resources = !catalog.resources.is_empty()
            || !catalog.resource_templates.is_empty()
            || !catalog.final_resources.is_empty()
            || !catalog.final_resource_templates.is_empty();
        let has_prompts = !catalog.prompts.is_empty() || !catalog.final_prompts.is_empty();
        #[cfg(feature = "tasks")]
        let task_relay = if catalog_era == ProtocolEra::Modern2026 {
            client.final_tasks_relay()?
        } else {
            None
        };
        #[cfg(feature = "tasks")]
        self.install_proxy_final_tasks_relay(task_relay.clone())?;
        #[cfg(not(feature = "tasks"))]
        let final_handlers = catalog.final_tool_handlers(client.clone())?;
        #[cfg(feature = "tasks")]
        let final_handlers = catalog
            .final_tools
            .iter()
            .cloned()
            .map(|tool| match task_relay.as_ref() {
                Some(task_relay) => ProxyToolHandler::from_final_with_task_relay(
                    tool,
                    client.clone(),
                    Arc::clone(task_relay),
                ),
                None => ProxyToolHandler::from_final(tool, client.clone()),
            })
            .collect::<McpResult<Vec<_>>>()?;

        // Legacy catalog entries register exact-2024-only: dual-era proxy
        // composition must not promote a legacy upstream's components into
        // the final catalog beside a modern upstream's own registrations.
        for tool in catalog.tools {
            if let Err(error) = self.router.add_legacy_tool_with_behavior(
                ProxyToolHandler::new(tool, client.clone()),
                self.on_duplicate,
            ) {
                log::error!(
                    target: "fastmcp_rust::builder",
                    "Failed to register proxied tool; code={:?}",
                    error.code
                );
                Self::absorb_prefixed_proxy_duplicate(error)?;
            }
        }

        for handler in final_handlers {
            match self
                .router
                .add_final_tool_with_behavior(handler, self.on_duplicate)
            {
                Ok(()) => {}
                Err(error) => {
                    log::error!(
                        target: "fastmcp_rust::builder",
                        "Failed to register exact-final proxied tool; code={:?}",
                        error.code
                    );
                    Self::absorb_prefixed_proxy_duplicate(error)?;
                }
            }
        }

        for resource in catalog.resources {
            if let Err(error) = self.router.add_legacy_resource_with_behavior(
                ProxyResourceHandler::new(resource, client.clone()),
                self.on_duplicate,
            ) {
                log::error!(
                    target: "fastmcp_rust::builder",
                    "Failed to register proxied resource; code={:?}",
                    error.code
                );
                Self::absorb_prefixed_proxy_duplicate(error)?;
            }
        }

        for template in catalog.resource_templates {
            let downstream_uri = template.uri_template.clone();
            let completion_target_admitted =
                self.proxy_resource_template_wins_admission(&downstream_uri);
            match self.router.add_legacy_resource_with_behavior(
                ProxyResourceHandler::from_template(template, client.clone()),
                self.on_duplicate,
            ) {
                Ok(()) if completion_target_admitted => {
                    if completion_supported && catalog_era == ProtocolEra::Legacy2024 {
                        self.router.add_legacy_resource_template_completion_handler(
                            downstream_uri.clone(),
                            ProxyCompletionHandler::for_resource_template(
                                client.clone(),
                                downstream_uri.clone(),
                                downstream_uri,
                            ),
                        );
                        self.advertise_completions();
                    }
                }
                Ok(()) => {}
                Err(error) => {
                    log::error!(
                        target: "fastmcp_rust::builder",
                        "Failed to register proxied resource template; code={:?}",
                        error.code
                    );
                    Self::absorb_prefixed_proxy_duplicate(error)?;
                }
            }
        }

        for prompt in catalog.prompts {
            let downstream_name = prompt.name.clone();
            let completion_target_admitted = self.proxy_prompt_wins_admission(&downstream_name);
            match self.router.add_legacy_prompt_with_behavior(
                ProxyPromptHandler::new(prompt, client.clone()),
                self.on_duplicate,
            ) {
                Ok(()) if completion_target_admitted => {
                    if completion_supported && catalog_era == ProtocolEra::Legacy2024 {
                        self.router.add_legacy_prompt_completion_handler(
                            downstream_name.clone(),
                            ProxyCompletionHandler::for_prompt(
                                client.clone(),
                                downstream_name.clone(),
                                downstream_name,
                            ),
                        );
                        self.advertise_completions();
                    }
                }
                Ok(()) => {}
                Err(error) => {
                    log::error!(
                        target: "fastmcp_rust::builder",
                        "Failed to register proxied prompt; code={:?}",
                        error.code
                    );
                    Self::absorb_prefixed_proxy_duplicate(error)?;
                }
            }
        }

        for resource in catalog.final_resources {
            if let Err(error) = self.router.add_final_resource_with_behavior(
                FinalProxyResourceHandler::new(resource, client.clone()),
                self.on_duplicate,
            ) {
                log::error!(
                    target: "fastmcp_rust::builder",
                    "Failed to register exact-final proxied resource; code={:?}",
                    error.code
                );
                Self::absorb_prefixed_proxy_duplicate(error)?;
            }
        }

        for template in catalog.final_resource_templates {
            let downstream_uri = template.uri_template.clone();
            let completion_target_admitted =
                self.proxy_resource_template_wins_admission(&downstream_uri);
            match self.router.add_final_resource_with_behavior(
                FinalProxyResourceTemplateHandler::new(template, client.clone()),
                self.on_duplicate,
            ) {
                Ok(())
                    if completion_target_admitted
                        && completion_supported
                        && catalog_era == ProtocolEra::Modern2026 =>
                {
                    self.router.add_resource_template_completion_handler(
                        downstream_uri.clone(),
                        ProxyCompletionHandler::for_resource_template(
                            client.clone(),
                            downstream_uri.clone(),
                            downstream_uri,
                        ),
                    );
                    self.advertise_completions();
                }
                Ok(()) => {}
                Err(error) => {
                    log::error!(
                        target: "fastmcp_rust::builder",
                        "Failed to register exact-final proxied resource template; code={:?}",
                        error.code
                    );
                    Self::absorb_prefixed_proxy_duplicate(error)?;
                }
            }
        }

        for prompt in catalog.final_prompts {
            let downstream_name = prompt.name.clone();
            let completion_target_admitted = self.proxy_prompt_wins_admission(&downstream_name);
            match self.router.add_final_prompt_with_behavior(
                FinalProxyPromptHandler::new(prompt, client.clone()),
                self.on_duplicate,
            ) {
                Ok(())
                    if completion_target_admitted
                        && completion_supported
                        && catalog_era == ProtocolEra::Modern2026 =>
                {
                    self.router.add_prompt_completion_handler(
                        downstream_name.clone(),
                        ProxyCompletionHandler::for_prompt(
                            client.clone(),
                            downstream_name.clone(),
                            downstream_name,
                        ),
                    );
                    self.advertise_completions();
                }
                Ok(()) => {}
                Err(error) => {
                    log::error!(
                        target: "fastmcp_rust::builder",
                        "Failed to register exact-final proxied prompt; code={:?}",
                        error.code
                    );
                    Self::absorb_prefixed_proxy_duplicate(error)?;
                }
            }
        }

        if has_tools {
            self.advertise_legacy_tools_list_changed();
        }
        if has_resources {
            self.advertise_legacy_resource_subscriptions();
        }
        if has_prompts {
            self.advertise_legacy_prompts_list_changed();
        }

        Ok(self)
    }

    /// Creates a proxy to an external MCP server with automatic discovery.
    ///
    /// This is a convenience method that combines connection, discovery, and
    /// handler registration. The client should already be initialized (connected
    /// to the server).
    ///
    /// All tools, resources, and prompts from the external server are registered
    /// as proxy handlers with the specified prefix.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use fastmcp_client::Client;
    ///
    /// // Create and initialize client
    /// let mut client = Client::new(transport)?;
    /// client.initialize()?;
    ///
    /// // Create main server with proxy to external
    /// let main = Server::new("main", "1.0")
    ///     .tool(local_tool)
    ///     .as_proxy("ext", client)?    // ext/external_tool, etc.
    ///     .build();
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error if the catalog fetch fails. Duplicate-policy
    /// rejections of individual proxied components are logged and skipped so
    /// the rest of the catalog still installs.
    #[cfg(feature = "proxy")]
    pub fn as_proxy(
        self,
        prefix: &str,
        client: fastmcp_client::Client,
    ) -> Result<Self, fastmcp_core::McpError> {
        let proxy_client = ProxyClient::from_client(client)?;
        let catalog = proxy_client.catalog_typed()?;
        self.register_prefixed_typed_proxy_catalog(prefix, proxy_client, catalog)
    }

    /// Registers one already-negotiated typed upstream catalog under a caller
    /// prefix. Tools and prompts take `{prefix}/{name}`; exact-final resource
    /// URIs stay unprefixed so they remain absolute.
    ///
    /// This is the HTTP-capable counterpart to [`as_proxy`](Self::as_proxy):
    /// a live `ProxyClient` from `connect_http_with_protocol_plan` already
    /// carries a ModernHttp/LegacyHttpSse binding, so it must not be rebuilt
    /// through `from_client` (which labels the adapter as stdio).
    ///
    /// # Errors
    ///
    /// Returns an error if the typed catalog is mixed-era or contradicts the
    /// selected route. Duplicate-policy rejections of individual proxied
    /// components are logged and skipped so the rest of the catalog still
    /// installs.
    #[cfg(feature = "proxy")]
    pub fn as_proxy_typed(
        self,
        prefix: &str,
        proxy_client: ProxyClient,
        catalog: ProxyTypedCatalog,
    ) -> Result<Self, fastmcp_core::McpError> {
        self.register_prefixed_typed_proxy_catalog(prefix, proxy_client, catalog)
    }

    /// Copies nonempty upstream initialize/discover instructions onto this
    /// gateway when the builder has not already set its own string.
    #[cfg(feature = "proxy")]
    fn adopt_upstream_proxy_instructions(
        &mut self,
        proxy_client: &ProxyClient,
    ) -> Result<(), fastmcp_core::McpError> {
        if self
            .instructions
            .as_ref()
            .is_some_and(|instructions| !instructions.is_empty())
        {
            return Ok(());
        }
        if let Some(instructions) = proxy_client.upstream_instructions()?
            && !instructions.is_empty()
        {
            self.instructions = Some(instructions);
        }
        Ok(())
    }

    /// Copies upstream modern Implementation extras onto this gateway when the
    /// builder has not already set title, description, website, or icons.
    ///
    /// The gateway keeps its own `name`/`version`. Exact-2024 initialize stays
    /// name/version-only.
    #[cfg(feature = "proxy")]
    fn adopt_upstream_proxy_implementation(
        &mut self,
        proxy_client: &ProxyClient,
    ) -> Result<(), fastmcp_core::McpError> {
        let gateway_has_extras = self.title.as_ref().is_some_and(|title| !title.is_empty())
            || self
                .description
                .as_ref()
                .is_some_and(|description| !description.is_empty())
            || self
                .website_url
                .as_ref()
                .is_some_and(|website| !website.is_empty())
            || !self.icons.is_empty();
        if gateway_has_extras {
            return Ok(());
        }
        let Some(implementation) = proxy_client.upstream_implementation()? else {
            return Ok(());
        };
        if implementation.title.is_none()
            && implementation.description.is_none()
            && implementation.website_url.is_none()
            && implementation.icons.is_empty()
        {
            return Ok(());
        }
        self.title = implementation.title;
        self.description = implementation.description;
        self.website_url = implementation
            .website_url
            .map(|uri| uri.as_str().to_owned());
        self.icons = implementation.icons;
        Ok(())
    }

    #[cfg(feature = "proxy")]
    fn register_prefixed_typed_proxy_catalog(
        mut self,
        prefix: &str,
        proxy_client: ProxyClient,
        catalog: ProxyTypedCatalog,
    ) -> Result<Self, fastmcp_core::McpError> {
        proxy_client.admit_typed_catalog(&catalog)?;
        self.adopt_upstream_proxy_instructions(&proxy_client)?;
        self.adopt_upstream_proxy_implementation(&proxy_client)?;
        let completion_supported = proxy_client.supports_completion()?;
        #[cfg(feature = "tasks")]
        let catalog_era = catalog.era()?;
        #[cfg(feature = "tasks")]
        let task_relay = if catalog_era == ProtocolEra::Modern2026 {
            proxy_client.final_tasks_relay()?
        } else {
            None
        };
        #[cfg(feature = "tasks")]
        self.install_proxy_final_tasks_relay(task_relay.clone())?;
        let (tool_count, resource_count, template_count, prompt_count) = match catalog {
            ProxyTypedCatalog {
                tools: ProxyToolCatalog::Legacy(tools),
                resources: ProxyResourceCatalog::Legacy(resources),
                resource_templates: ProxyResourceTemplateCatalog::Legacy(resource_templates),
                prompts: ProxyPromptCatalog::Legacy(prompts),
            } => {
                let counts = (
                    tools.len(),
                    resources.len(),
                    resource_templates.len(),
                    prompts.len(),
                );
                // Prefixed legacy keys (`{prefix}/{uri}`) are not absolute
                // final URIs. Register them exact-2024-only so they are not
                // projected into the modern catalog.
                for tool in tools {
                    if let Err(error) = self.router.add_legacy_tool_with_behavior(
                        ProxyToolHandler::with_prefix(tool, prefix, proxy_client.clone()),
                        self.on_duplicate,
                    ) {
                        log::error!(
                            target: "fastmcp_rust::builder",
                            "Failed to register prefixed proxied tool; code={:?}",
                            error.code
                        );
                        Self::absorb_prefixed_proxy_duplicate(error)?;
                    }
                }
                for resource in resources {
                    if let Err(error) = self.router.add_legacy_resource_with_behavior(
                        ProxyResourceHandler::with_prefix(resource, prefix, proxy_client.clone()),
                        self.on_duplicate,
                    ) {
                        log::error!(
                            target: "fastmcp_rust::builder",
                            "Failed to register prefixed proxied resource; code={:?}",
                            error.code
                        );
                        Self::absorb_prefixed_proxy_duplicate(error)?;
                    }
                }
                for template in resource_templates {
                    let upstream_uri = template.uri_template.clone();
                    let downstream_uri = format!("{prefix}/{upstream_uri}");
                    let completion_target_admitted =
                        self.proxy_resource_template_wins_admission(&downstream_uri);
                    match self.router.add_legacy_resource_with_behavior(
                        ProxyResourceHandler::from_template_with_prefix(
                            template,
                            prefix,
                            proxy_client.clone(),
                        ),
                        self.on_duplicate,
                    ) {
                        Ok(()) if completion_target_admitted && completion_supported => {
                            self.router.add_legacy_resource_template_completion_handler(
                                downstream_uri.clone(),
                                ProxyCompletionHandler::for_resource_template(
                                    proxy_client.clone(),
                                    downstream_uri,
                                    upstream_uri,
                                ),
                            );
                            self.advertise_completions();
                        }
                        Ok(()) => {}
                        Err(error) => {
                            log::error!(
                                target: "fastmcp_rust::builder",
                                "Failed to register prefixed proxied resource template; code={:?}",
                                error.code
                            );
                            Self::absorb_prefixed_proxy_duplicate(error)?;
                        }
                    }
                }
                for prompt in prompts {
                    let upstream_name = prompt.name.clone();
                    let downstream_name = format!("{prefix}/{upstream_name}");
                    let completion_target_admitted =
                        self.proxy_prompt_wins_admission(&downstream_name);
                    match self.router.add_legacy_prompt_with_behavior(
                        ProxyPromptHandler::with_prefix(prompt, prefix, proxy_client.clone()),
                        self.on_duplicate,
                    ) {
                        Ok(()) if completion_target_admitted && completion_supported => {
                            self.router.add_legacy_prompt_completion_handler(
                                downstream_name.clone(),
                                ProxyCompletionHandler::for_prompt(
                                    proxy_client.clone(),
                                    downstream_name,
                                    upstream_name,
                                ),
                            );
                            self.advertise_completions();
                        }
                        Ok(()) => {}
                        Err(error) => {
                            log::error!(
                                target: "fastmcp_rust::builder",
                                "Failed to register prefixed proxied prompt; code={:?}",
                                error.code
                            );
                            Self::absorb_prefixed_proxy_duplicate(error)?;
                        }
                    }
                }
                counts
            }
            ProxyTypedCatalog {
                tools: ProxyToolCatalog::Final(tools),
                resources: ProxyResourceCatalog::Final(resources),
                resource_templates: ProxyResourceTemplateCatalog::Final(resource_templates),
                prompts: ProxyPromptCatalog::Final(prompts),
            } => {
                // Tools and prompts take the caller prefix. Exact-final resource
                // URIs stay unprefixed so they remain absolute; prefixing them
                // as `{prefix}/{uri}` would drop them from the modern catalog.
                let counts = (
                    tools.len(),
                    resources.len(),
                    resource_templates.len(),
                    prompts.len(),
                );
                for tool in tools {
                    #[cfg(feature = "tasks")]
                    let handler = match task_relay.as_ref() {
                        Some(task_relay) => ProxyToolHandler::with_prefix_final_with_task_relay(
                            tool,
                            prefix,
                            proxy_client.clone(),
                            Arc::clone(task_relay),
                        )?,
                        None => {
                            ProxyToolHandler::with_prefix_final(tool, prefix, proxy_client.clone())?
                        }
                    };
                    #[cfg(not(feature = "tasks"))]
                    let handler =
                        ProxyToolHandler::with_prefix_final(tool, prefix, proxy_client.clone())?;
                    if let Err(error) = self
                        .router
                        .add_final_tool_with_behavior(handler, self.on_duplicate)
                    {
                        log::error!(
                            target: "fastmcp_rust::builder",
                            "Failed to register prefixed exact-final proxied tool; code={:?}",
                            error.code
                        );
                        Self::absorb_prefixed_proxy_duplicate(error)?;
                    }
                }
                for resource in resources {
                    if let Err(error) = self.router.add_final_resource_with_behavior(
                        FinalProxyResourceHandler::new(resource, proxy_client.clone()),
                        self.on_duplicate,
                    ) {
                        log::error!(
                            target: "fastmcp_rust::builder",
                            "Failed to register prefixed exact-final proxied resource; code={:?}",
                            error.code
                        );
                        Self::absorb_prefixed_proxy_duplicate(error)?;
                    }
                }
                for template in resource_templates {
                    let downstream_uri = template.uri_template.clone();
                    let completion_target_admitted =
                        self.proxy_resource_template_wins_admission(&downstream_uri);
                    match self.router.add_final_resource_with_behavior(
                        FinalProxyResourceTemplateHandler::new(template, proxy_client.clone()),
                        self.on_duplicate,
                    ) {
                        Ok(()) if completion_target_admitted && completion_supported => {
                            self.router.add_resource_template_completion_handler(
                                downstream_uri.clone(),
                                ProxyCompletionHandler::for_resource_template(
                                    proxy_client.clone(),
                                    downstream_uri.clone(),
                                    downstream_uri,
                                ),
                            );
                            self.advertise_completions();
                        }
                        Ok(()) => {}
                        Err(error) => {
                            log::error!(
                                target: "fastmcp_rust::builder",
                                "Failed to register prefixed exact-final proxied resource template; code={:?}",
                                error.code
                            );
                            Self::absorb_prefixed_proxy_duplicate(error)?;
                        }
                    }
                }
                for prompt in prompts {
                    let upstream_name = prompt.name.clone();
                    let downstream_name = format!("{prefix}/{upstream_name}");
                    let completion_target_admitted =
                        self.proxy_prompt_wins_admission(&downstream_name);
                    match self.router.add_final_prompt_with_behavior(
                        FinalProxyPromptHandler::with_prefix(prompt, prefix, proxy_client.clone()),
                        self.on_duplicate,
                    ) {
                        Ok(()) if completion_target_admitted && completion_supported => {
                            self.router.add_prompt_completion_handler(
                                downstream_name.clone(),
                                ProxyCompletionHandler::for_prompt(
                                    proxy_client.clone(),
                                    downstream_name,
                                    upstream_name,
                                ),
                            );
                            self.advertise_completions();
                        }
                        Ok(()) => {}
                        Err(error) => {
                            log::error!(
                                target: "fastmcp_rust::builder",
                                "Failed to register prefixed exact-final proxied prompt; code={:?}",
                                error.code
                            );
                            Self::absorb_prefixed_proxy_duplicate(error)?;
                        }
                    }
                }
                counts
            }
            _ => {
                return Err(fastmcp_core::McpError::invalid_request(
                    "proxy typed catalog mixes legacy and final component vectors",
                ));
            }
        };

        let has_tools = tool_count > 0;
        let has_resources = resource_count > 0 || template_count > 0;
        let has_prompts = prompt_count > 0;

        // Update capabilities
        if has_tools {
            self.advertise_legacy_tools_list_changed();
        }
        if has_resources {
            self.advertise_legacy_resource_subscriptions();
        }
        if has_prompts {
            self.advertise_legacy_prompts_list_changed();
        }

        log::info!(
            target: "fastmcp_rust::proxy",
            "Proxied {} tools, {} resources, {} templates, and {} prompts with a configured prefix",
            tool_count,
            resource_count,
            template_count,
            prompt_count
        );

        Ok(self)
    }

    /// Creates a proxy to an external MCP server without a prefix.
    ///
    /// Similar to [`as_proxy`](Self::as_proxy), but tools/resources/prompts
    /// keep their original names. Use this when proxying a single external
    /// server or when you don't need namespace separation.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let main = Server::new("main", "1.0")
    ///     .as_proxy_raw(client)?  // External tools appear with original names
    ///     .build();
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error if catalog discovery fails or the configured duplicate
    /// policy rejects any unprefixed proxy component.
    #[cfg(feature = "proxy")]
    pub fn as_proxy_raw(
        self,
        client: fastmcp_client::Client,
    ) -> Result<Self, fastmcp_core::McpError> {
        self.into_proxy_raw_with_proxy_client(ProxyClient::from_client(client)?)
    }

    /// Registers one already-negotiated, typed upstream catalog without any
    /// legacy projection. Final components are visible only to final routes;
    /// legacy components remain visible only to exact MCP 2024-11-05 routes.
    ///
    /// A caller-provided catalog is accepted only when the client already has
    /// immutable transport binding or backend-observed era evidence. Call
    /// [`ProxyClient::catalog_typed`] first for an unbound custom backend.
    ///
    /// # Errors
    ///
    /// Returns an error when the typed catalog is mixed-era, contradicts the
    /// selected route, or tries to bind an otherwise unbound route.
    #[cfg(feature = "proxy")]
    pub fn proxy_typed(
        self,
        proxy_client: ProxyClient,
        catalog: ProxyTypedCatalog,
    ) -> Result<Self, fastmcp_core::McpError> {
        self.register_raw_typed_proxy_catalog(proxy_client, catalog)
    }

    #[cfg(feature = "proxy")]
    fn into_proxy_raw_with_proxy_client(
        self,
        proxy_client: ProxyClient,
    ) -> Result<Self, fastmcp_core::McpError> {
        let catalog = proxy_client.catalog_typed()?;
        self.register_raw_typed_proxy_catalog(proxy_client, catalog)
    }

    /// Registers one already-negotiated typed upstream catalog without
    /// projecting final resource/template/prompt definitions through their
    /// narrower legacy models.
    #[cfg(feature = "proxy")]
    fn register_raw_typed_proxy_catalog(
        mut self,
        proxy_client: ProxyClient,
        catalog: ProxyTypedCatalog,
    ) -> Result<Self, fastmcp_core::McpError> {
        // `catalog_typed` seals backend-observed evidence itself. A
        // caller-provided typed catalog must instead prove it agrees with an
        // already-selected transport or an earlier observed catalog; it may
        // not establish an era for an unbound proxy route.
        proxy_client.admit_typed_catalog(&catalog)?;
        self.adopt_upstream_proxy_instructions(&proxy_client)?;
        self.adopt_upstream_proxy_implementation(&proxy_client)?;
        #[cfg(feature = "tasks")]
        let catalog_era = catalog.era()?;
        #[cfg(not(feature = "tasks"))]
        catalog.era()?;
        let completion_supported = proxy_client.supports_completion()?;
        #[cfg(feature = "tasks")]
        let task_relay = if catalog_era == ProtocolEra::Modern2026 {
            proxy_client.final_tasks_relay()?
        } else {
            None
        };
        #[cfg(feature = "tasks")]
        self.install_proxy_final_tasks_relay(task_relay.clone())?;
        match (
            catalog.tools,
            catalog.resources,
            catalog.resource_templates,
            catalog.prompts,
        ) {
            (
                ProxyToolCatalog::Legacy(tools),
                ProxyResourceCatalog::Legacy(resources),
                ProxyResourceTemplateCatalog::Legacy(resource_templates),
                ProxyPromptCatalog::Legacy(prompts),
            ) => {
                let has_tools = !tools.is_empty();
                let has_resources = !resources.is_empty() || !resource_templates.is_empty();
                let has_prompts = !prompts.is_empty();
                // A legacy upstream catalog registers exact-2024-only: dual-era
                // proxy composition must not promote its entries into the
                // final catalog beside a modern upstream's own registrations.
                for tool in tools {
                    self.router.add_legacy_tool_with_behavior(
                        ProxyToolHandler::new(tool, proxy_client.clone()),
                        self.on_duplicate,
                    )?;
                }
                for resource in resources {
                    self.router.add_legacy_resource_with_behavior(
                        ProxyResourceHandler::new(resource, proxy_client.clone()),
                        self.on_duplicate,
                    )?;
                }
                for template in resource_templates {
                    let downstream_uri = template.uri_template.clone();
                    let completion_target_admitted =
                        self.proxy_resource_template_wins_admission(&downstream_uri);
                    self.router.add_legacy_resource_with_behavior(
                        ProxyResourceHandler::from_template(template, proxy_client.clone()),
                        self.on_duplicate,
                    )?;
                    if completion_target_admitted && completion_supported {
                        self.router.add_legacy_resource_template_completion_handler(
                            downstream_uri.clone(),
                            ProxyCompletionHandler::for_resource_template(
                                proxy_client.clone(),
                                downstream_uri.clone(),
                                downstream_uri,
                            ),
                        );
                        self.advertise_completions();
                    }
                }
                for prompt in prompts {
                    let downstream_name = prompt.name.clone();
                    let completion_target_admitted =
                        self.proxy_prompt_wins_admission(&downstream_name);
                    self.router.add_legacy_prompt_with_behavior(
                        ProxyPromptHandler::new(prompt, proxy_client.clone()),
                        self.on_duplicate,
                    )?;
                    if completion_target_admitted && completion_supported {
                        self.router.add_legacy_prompt_completion_handler(
                            downstream_name.clone(),
                            ProxyCompletionHandler::for_prompt(
                                proxy_client.clone(),
                                downstream_name.clone(),
                                downstream_name,
                            ),
                        );
                        self.advertise_completions();
                    }
                }
                if has_tools {
                    self.advertise_legacy_tools_list_changed();
                }
                if has_resources {
                    self.advertise_legacy_resource_subscriptions();
                }
                if has_prompts {
                    self.advertise_legacy_prompts_list_changed();
                }
            }
            (
                ProxyToolCatalog::Final(tools),
                ProxyResourceCatalog::Final(resources),
                ProxyResourceTemplateCatalog::Final(resource_templates),
                ProxyPromptCatalog::Final(prompts),
            ) => {
                let has_tools = !tools.is_empty();
                let has_resources = !resources.is_empty() || !resource_templates.is_empty();
                let has_prompts = !prompts.is_empty();
                for tool in tools {
                    // Complete-only registration: a live echo/tool catalog must
                    // not require the official Tasks client extension merely
                    // because the upstream server advertises Tasks. The route
                    // still installs `final_task_relay` for `tasks/*` controls.
                    let handler = ProxyToolHandler::from_final(tool, proxy_client.clone())?;
                    self.router
                        .add_final_tool_with_behavior(handler, self.on_duplicate)?;
                }
                for resource in resources {
                    self.router.add_final_resource_with_behavior(
                        FinalProxyResourceHandler::new(resource, proxy_client.clone()),
                        self.on_duplicate,
                    )?;
                }
                for template in resource_templates {
                    let downstream_uri = template.uri_template.clone();
                    let completion_target_admitted =
                        self.proxy_resource_template_wins_admission(&downstream_uri);
                    self.router.add_final_resource_with_behavior(
                        FinalProxyResourceTemplateHandler::new(template, proxy_client.clone()),
                        self.on_duplicate,
                    )?;
                    if completion_target_admitted && completion_supported {
                        self.router.add_resource_template_completion_handler(
                            downstream_uri.clone(),
                            ProxyCompletionHandler::for_resource_template(
                                proxy_client.clone(),
                                downstream_uri.clone(),
                                downstream_uri,
                            ),
                        );
                        self.advertise_completions();
                    }
                }
                for prompt in prompts {
                    let downstream_name = prompt.name.clone();
                    let completion_target_admitted =
                        self.proxy_prompt_wins_admission(&downstream_name);
                    self.router.add_final_prompt_with_behavior(
                        FinalProxyPromptHandler::new(prompt, proxy_client.clone()),
                        self.on_duplicate,
                    )?;
                    if completion_target_admitted && completion_supported {
                        self.router.add_prompt_completion_handler(
                            downstream_name.clone(),
                            ProxyCompletionHandler::for_prompt(
                                proxy_client.clone(),
                                downstream_name.clone(),
                                downstream_name,
                            ),
                        );
                        self.advertise_completions();
                    }
                }
                if has_tools {
                    self.advertise_legacy_tools_list_changed();
                }
                if has_resources {
                    self.advertise_legacy_resource_subscriptions();
                }
                if has_prompts {
                    self.advertise_legacy_prompts_list_changed();
                }
            }
            _ => {
                return Err(fastmcp_core::McpError::invalid_request(
                    "proxy typed catalog mixes legacy and final component vectors",
                ));
            }
        }
        Ok(self)
    }

    // ─────────────────────────────────────────────────
    // Server Composition (Mount)
    // ─────────────────────────────────────────────────

    /// Mounts another server's components into this server with an optional prefix.
    ///
    /// This consumes the source server and moves all its tools, resources, and prompts
    /// into this server. Names/URIs are prefixed with `prefix/` if a prefix is provided.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let db_server = Server::new("db", "1.0")
    ///     .tool(query_tool)
    ///     .tool(insert_tool)
    ///     .build();
    ///
    /// let api_server = Server::new("api", "1.0")
    ///     .tool(endpoint_tool)
    ///     .build();
    ///
    /// let main = Server::new("main", "1.0")
    ///     .mount(db_server, Some("db"))      // db/query, db/insert
    ///     .mount(api_server, Some("api"))    // api/endpoint
    ///     .build();
    /// ```
    ///
    /// # Prefix Rules
    ///
    /// - Prefixes must be alphanumeric plus underscores and hyphens
    /// - Prefixes cannot contain slashes
    /// - With prefix `"db"`, tool `"query"` becomes `"db/query"`
    /// - Without prefix, names are preserved (may cause conflicts)
    ///
    /// Duplicate handling follows [`on_duplicate`](Self::on_duplicate). With
    /// [`DuplicateBehavior::Error`], any conflict rejects the complete mount.
    /// A rejected mount is recorded and fails [`try_build`](Self::try_build).
    #[must_use]
    pub fn mount(mut self, server: crate::Server, prefix: Option<&str>) -> Self {
        #[cfg(feature = "apps")]
        if server.router.has_mcp_apps_bound_components() && !self.has_active_official_mcp_apps() {
            self.refuse_mount(prefix, MOUNT_APPS_REFUSAL.to_owned());
            return self;
        }

        let has_tools = server.has_tools();
        let has_resources = server.has_resources();
        let has_prompts = server.has_prompts();

        let source_router = server.into_router();
        let result = self
            .router
            .mount_with_behavior(source_router, prefix, self.on_duplicate);

        // Log warnings if any
        for warning in &result.warnings {
            log::warn!(target: "fastmcp_rust::mount", "{}", warning);
        }
        for error in &result.errors {
            self.refuse_mount(prefix, error.clone());
        }

        // Update capabilities based on what was mounted
        if has_tools && result.tools > 0 {
            self.advertise_legacy_tools_list_changed();
        }
        if has_resources && (result.resources > 0 || result.resource_templates > 0) {
            self.advertise_legacy_resource_subscriptions();
        }
        if has_prompts && result.prompts > 0 {
            self.advertise_legacy_prompts_list_changed();
        }
        if self.router.has_completion_handler() {
            self.advertise_completions();
        }

        self
    }

    /// Mounts tools and prompts with an optional name prefix, and keeps
    /// resource and template URIs exact.
    ///
    /// Use this when the destination must remain a modern catalog: a nonempty
    /// `{prefix}/{uri}` key is not an absolute final URI.
    #[must_use]
    pub fn mount_preserving_resource_uris(
        mut self,
        server: crate::Server,
        prefix: Option<&str>,
    ) -> Self {
        #[cfg(feature = "apps")]
        if server.router.has_mcp_apps_bound_components() && !self.has_active_official_mcp_apps() {
            self.refuse_mount(prefix, MOUNT_APPS_REFUSAL.to_owned());
            return self;
        }

        let has_tools = server.has_tools();
        let has_resources = server.has_resources();
        let has_prompts = server.has_prompts();

        let source_router = server.into_router();
        let result =
            self.router
                .mount_namespaced_with_behavior(source_router, prefix, self.on_duplicate);

        for warning in &result.warnings {
            log::warn!(target: "fastmcp_rust::mount", "{}", warning);
        }
        for error in &result.errors {
            self.refuse_mount(prefix, error.clone());
        }

        if has_tools && result.tools > 0 {
            self.advertise_legacy_tools_list_changed();
        }
        if has_resources && (result.resources > 0 || result.resource_templates > 0) {
            self.advertise_legacy_resource_subscriptions();
        }
        if has_prompts && result.prompts > 0 {
            self.advertise_legacy_prompts_list_changed();
        }
        if self.router.has_completion_handler() {
            self.advertise_completions();
        }

        self
    }

    /// Mounts only tools from another server with an optional prefix.
    ///
    /// Similar to [`mount`](Self::mount), but only transfers tools, ignoring
    /// resources and prompts.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let utils_server = Server::new("utils", "1.0")
    ///     .tool(format_tool)
    ///     .tool(parse_tool)
    ///     .resource(config_resource)  // Will NOT be mounted
    ///     .build();
    ///
    /// let main = Server::new("main", "1.0")
    ///     .mount_tools(utils_server, Some("utils"))  // Only tools
    ///     .build();
    /// ```
    ///
    /// Duplicate handling follows [`on_duplicate`](Self::on_duplicate).
    #[must_use]
    pub fn mount_tools(mut self, server: crate::Server, prefix: Option<&str>) -> Self {
        #[cfg(feature = "apps")]
        if server.router.has_mcp_apps_bound_components() && !self.has_active_official_mcp_apps() {
            self.refuse_mount(prefix, MOUNT_APPS_REFUSAL.to_owned());
            return self;
        }

        let source_router = server.into_router();
        let result =
            self.router
                .mount_tools_with_behavior(source_router, prefix, self.on_duplicate);

        // Log warnings if any
        for warning in &result.warnings {
            log::warn!(target: "fastmcp_rust::mount", "{}", warning);
        }
        for error in &result.errors {
            self.refuse_mount(prefix, error.clone());
        }

        // Update capabilities if tools were mounted
        if result.tools > 0 {
            self.advertise_legacy_tools_list_changed();
        }

        self
    }

    /// Mounts only resources from another server with an optional prefix.
    ///
    /// Similar to [`mount`](Self::mount), but only transfers resources,
    /// ignoring tools and prompts.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let data_server = Server::new("data", "1.0")
    ///     .resource(config_resource)
    ///     .resource(schema_resource)
    ///     .tool(query_tool)  // Will NOT be mounted
    ///     .build();
    ///
    /// let main = Server::new("main", "1.0")
    ///     .mount_resources(data_server, Some("data"))  // Only resources
    ///     .build();
    /// ```
    ///
    /// Duplicate handling follows [`on_duplicate`](Self::on_duplicate) for
    /// both static resources and resource templates.
    #[must_use]
    pub fn mount_resources(mut self, server: crate::Server, prefix: Option<&str>) -> Self {
        #[cfg(feature = "apps")]
        if server.router.has_mcp_apps_bound_components() && !self.has_active_official_mcp_apps() {
            self.refuse_mount(prefix, MOUNT_APPS_REFUSAL.to_owned());
            return self;
        }

        let source_router = server.into_router();
        let result =
            self.router
                .mount_resources_with_behavior(source_router, prefix, self.on_duplicate);

        // Log warnings if any
        for warning in &result.warnings {
            log::warn!(target: "fastmcp_rust::mount", "{}", warning);
        }
        for error in &result.errors {
            self.refuse_mount(prefix, error.clone());
        }

        // Update capabilities if resources were mounted
        if result.resources > 0 || result.resource_templates > 0 {
            self.advertise_legacy_resource_subscriptions();
        }
        if self.router.has_completion_handler() {
            self.advertise_completions();
        }

        self
    }

    /// Mounts only prompts from another server with an optional prefix.
    ///
    /// Similar to [`mount`](Self::mount), but only transfers prompts,
    /// ignoring tools and resources.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let templates_server = Server::new("templates", "1.0")
    ///     .prompt(greeting_prompt)
    ///     .prompt(error_prompt)
    ///     .tool(format_tool)  // Will NOT be mounted
    ///     .build();
    ///
    /// let main = Server::new("main", "1.0")
    ///     .mount_prompts(templates_server, Some("tmpl"))  // Only prompts
    ///     .build();
    /// ```
    ///
    /// Duplicate handling follows [`on_duplicate`](Self::on_duplicate).
    #[must_use]
    pub fn mount_prompts(mut self, server: crate::Server, prefix: Option<&str>) -> Self {
        let source_router = server.into_router();
        let result =
            self.router
                .mount_prompts_with_behavior(source_router, prefix, self.on_duplicate);

        // Log warnings if any
        for warning in &result.warnings {
            log::warn!(target: "fastmcp_rust::mount", "{}", warning);
        }
        for error in &result.errors {
            self.refuse_mount(prefix, error.clone());
        }

        // Update capabilities if prompts were mounted
        if result.prompts > 0 {
            self.advertise_legacy_prompts_list_changed();
        }
        if self.router.has_completion_handler() {
            self.advertise_completions();
        }

        self
    }

    /// Sets custom server instructions.
    #[must_use]
    pub fn instructions(mut self, instructions: impl Into<String>) -> Self {
        self.instructions = Some(instructions.into());
        self
    }

    /// Sets the modern discovery title. Exact-2024 initialize stays name/version.
    #[must_use]
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Sets the modern discovery description.
    #[must_use]
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Sets the modern discovery website URL.
    #[must_use]
    pub fn website_url(mut self, website_url: impl Into<String>) -> Self {
        self.website_url = Some(website_url.into());
        self
    }

    /// Sets the modern discovery icon set.
    #[must_use]
    pub fn icons(mut self, icons: Vec<fastmcp_protocol::common_types::RawIcon>) -> Self {
        self.icons = icons;
        self
    }

    /// Sets the log level.
    ///
    /// Default is read from `FASTMCP_LOG` environment variable, or `INFO` if not set.
    #[must_use]
    pub fn log_level(mut self, level: Level) -> Self {
        let filter = level.to_level_filter();
        self.logging.level = filter;
        self.console_config.log_level = filter;
        self
    }

    /// Sets the log level from a filter, including [`LevelFilter::Off`].
    #[must_use]
    pub fn log_level_filter(mut self, filter: LevelFilter) -> Self {
        self.logging.level = filter;
        self.console_config.log_level = filter;
        self
    }

    /// Sets whether to show timestamps in logs.
    ///
    /// Default is `true`.
    #[must_use]
    pub fn log_timestamps(mut self, show: bool) -> Self {
        self.logging.timestamps = show;
        self.console_config.log_timestamps = show;
        self
    }

    /// Sets whether to show target/module paths in logs.
    ///
    /// Default is `true`.
    #[must_use]
    pub fn log_targets(mut self, show: bool) -> Self {
        self.logging.targets = show;
        self.console_config.log_targets = show;
        self
    }

    /// Sets whether to show source file and line in logs.
    #[must_use]
    pub fn log_file_line(mut self, show: bool) -> Self {
        self.logging.file_line = show;
        self.console_config.log_file_line = show;
        self
    }

    /// Sets the full logging configuration.
    #[must_use]
    pub fn logging(mut self, config: LoggingConfig) -> Self {
        self.console_config.log_level = config.level;
        self.console_config.log_timestamps = config.timestamps;
        self.console_config.log_targets = config.targets;
        self.console_config.log_file_line = config.file_line;
        self.logging = config;
        self
    }

    // ─────────────────────────────────────────────────
    // Console Configuration
    // ─────────────────────────────────────────────────

    /// Sets the complete console configuration.
    ///
    /// This controls server-owned console output, including the banner,
    /// traffic logging, logger formatting, and rich/plain display mode.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use fastmcp_console::config::{ConsoleConfig, BannerStyle};
    ///
    /// Server::new("demo", "1.0.0")
    ///     .with_console_config(
    ///         ConsoleConfig::new()
    ///             .with_banner(BannerStyle::Compact)
    ///             .plain_mode()
    ///     )
    ///     .build();
    /// ```
    #[must_use]
    pub fn with_console_config(mut self, config: ConsoleConfig) -> Self {
        self.logging = LoggingConfig::from(&config);
        self.console_config = config;
        self
    }

    /// Sets the banner style.
    ///
    /// Controls how the startup banner is displayed.
    /// Default is `BannerStyle::Full`.
    #[must_use]
    pub fn with_banner(mut self, style: BannerStyle) -> Self {
        self.console_config = self.console_config.with_banner(style);
        self
    }

    /// Disables the startup banner.
    #[must_use]
    pub fn without_banner(mut self) -> Self {
        self.console_config = self.console_config.without_banner();
        self
    }

    /// Enables request/response traffic logging.
    ///
    /// Controls the verbosity of traffic logging:
    /// - `None`: No traffic logging (default)
    /// - `Summary`: Method name and timing only
    /// - `Full`: Full request/response bodies
    #[must_use]
    pub fn with_traffic_logging(mut self, verbosity: TrafficVerbosity) -> Self {
        self.console_config = self.console_config.with_traffic(verbosity);
        self
    }

    /// Forces plain text output (no colors/styling).
    ///
    /// Useful for CI environments, logging to files, or when running
    /// as an MCP server where rich output might interfere with the
    /// JSON-RPC protocol.
    #[must_use]
    pub fn plain_mode(mut self) -> Self {
        self.console_config = self.console_config.plain_mode();
        self
    }

    /// Forces color output even in non-TTY environments.
    #[must_use]
    pub fn force_color(mut self) -> Self {
        self.console_config = self.console_config.force_color(true);
        self
    }

    /// Returns a reference to the current console configuration.
    #[must_use]
    pub fn console_config(&self) -> &ConsoleConfig {
        &self.console_config
    }

    // ─────────────────────────────────────────────────
    // Lifecycle Hooks
    // ─────────────────────────────────────────────────

    /// Registers a startup hook that runs before the server starts accepting connections.
    ///
    /// The hook can perform initialization tasks like:
    /// - Opening database connections
    /// - Loading configuration files
    /// - Initializing caches
    ///
    /// If the hook returns an error, the server will not start.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Inside the application's async entry point, with its supplied `cx`.
    /// ServerBuilder::new("demo", "1.0.0")
    ///     .on_startup(|| {
    ///         eprintln!("Server starting up...");
    ///         Ok::<(), std::io::Error>(())
    ///     })
    ///     .build()
    ///     .run_stdio_with_cx(cx)
    ///     .await;
    /// ```
    #[must_use]
    pub fn on_startup<F, E>(mut self, hook: F) -> Self
    where
        F: FnOnce() -> Result<(), E> + Send + 'static,
        E: std::error::Error + Send + Sync + 'static,
    {
        self.lifespan.on_startup = Some(Box::new(move || {
            hook().map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
        }));
        self
    }

    /// Registers a shutdown hook that runs when the server is shutting down.
    ///
    /// The hook can perform cleanup tasks like:
    /// - Closing database connections
    /// - Flushing caches
    /// - Saving state
    ///
    /// Shutdown hooks are run on a best-effort basis. If the process is
    /// forcefully terminated, hooks may not run.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Inside the application's async entry point, with its supplied `cx`.
    /// ServerBuilder::new("demo", "1.0.0")
    ///     .on_shutdown(|| {
    ///         eprintln!("Server shutting down...");
    ///     })
    ///     .build()
    ///     .run_stdio_with_cx(cx)
    ///     .await;
    /// ```
    #[must_use]
    pub fn on_shutdown<F>(mut self, hook: F) -> Self
    where
        F: FnOnce() + Send + 'static,
    {
        self.lifespan.on_shutdown = Some(Box::new(hook));
        self
    }

    /// Retains the legacy task manager for unit-test archaeology only.
    /// Production builds expose no task-manager builder edge.
    #[cfg(all(test, feature = "tasks"))]
    #[must_use]
    pub(crate) fn with_task_manager(mut self, task_manager: SharedTaskManager) -> Self {
        self.task_manager = Some(task_manager);
        // Fail closed even if future builder composition adds another path
        // that mutates the capability object before this call.
        self.capabilities.tasks = None;
        self
    }

    /// Returns the current request timeout.
    #[cfg(test)]
    fn request_timeout_secs(&self) -> u64 {
        self.request_timeout_secs
    }

    /// Builds the server after a valid builder-level configuration.
    ///
    /// Both [`Self::try_new`] and [`Self::protocol_policy`] reject invalid or
    /// unavailable policy selections before a builder exists.
    ///
    /// When the `tasks` feature is enabled and the builder has not already
    /// installed a local or proxy Tasks owner, this installs a process-local
    /// in-memory official Tasks runtime so `tasks/get`, `tasks/update`, and
    /// `tasks/cancel` are served. Call [`Self::final_tasks`] to replace that
    /// default with an application-owned store. The historical task-manager
    /// adapter is test-only and does not serve the official methods.
    ///
    /// # Panics
    ///
    /// Panics with the [`ServerBuildError`] when any registration was refused,
    /// rather than starting a server that silently lacks it. Use
    /// [`Self::try_build`] to handle the refusal.
    #[must_use]
    pub fn build(self) -> Server {
        match self.try_build() {
            Ok(server) => server,
            Err(error) => panic!("{error}"),
        }
    }

    /// Builds the server, or fails naming every refused registration.
    ///
    /// # Errors
    ///
    /// Returns [`ServerBuildError::InvalidConfiguration`] listing each refused
    /// tool, resource, resource template, prompt or mount, with its reason.
    /// No server is built.
    pub fn try_build(mut self) -> Result<Server, ServerBuildError> {
        if !self.refused_registrations.is_empty() {
            return Err(ServerBuildError::InvalidConfiguration(std::mem::take(
                &mut self.refused_registrations,
            )));
        }
        Ok(self.build_admitted())
    }

    fn build_admitted(mut self) -> Server {
        // Configure router with strict input validation setting
        self.router
            .set_strict_input_validation(self.strict_input_validation);
        let console = fastmcp_console::console::FastMcpConsole::with_enabled(
            self.console_config.should_use_rich(),
        );
        let final_subscriptions = Arc::new(FinalSubscriptionRegistry::default());
        #[cfg(feature = "tasks")]
        self.install_default_in_memory_final_tasks();
        #[cfg(feature = "tasks")]
        let final_task_runtime = self.final_task_runtime.clone();
        #[cfg(all(feature = "proxy", feature = "tasks"))]
        let final_task_relay = self.final_task_relay.clone();
        #[cfg(feature = "tasks")]
        if let Some(task_runtime) = final_task_runtime.as_ref() {
            let subscriptions = Arc::clone(&final_subscriptions);
            task_runtime.add_owned_notification_emitter(Arc::new(
                move |notification, principal| {
                    if subscriptions
                        .publish_owned_task(notification, principal)
                        .is_err()
                    {
                        log::error!(
                            target: "fastmcp_rust::server",
                            "Failed to publish a typed final Task notification"
                        );
                    }
                },
            ));
        }
        #[cfg(feature = "tasks")]
        self.router
            .set_final_task_runtime(final_task_runtime.clone());
        #[cfg(all(feature = "proxy", feature = "tasks"))]
        self.router.set_final_task_relay(final_task_relay.clone());
        let extension_runtime = match self.extension_runtime {
            Some(mut runtime) => {
                runtime
                    .freeze()
                    .expect("validated server extension descriptors must freeze");
                Some(Arc::new(runtime))
            }
            #[cfg(all(feature = "proxy", feature = "tasks"))]
            None => match (final_task_runtime.as_ref(), final_task_relay.as_ref()) {
                (Some(task_runtime), None) => {
                    let mut runtime = ServerExtensionRuntime::with_final_tasks(task_runtime)
                        .expect("final Tasks must install into an empty extension registry");
                    runtime
                        .freeze()
                        .expect("final Tasks extension descriptors must freeze");
                    Some(Arc::new(runtime))
                }
                (None, Some(task_relay)) => {
                    let mut runtime =
                        ServerExtensionRuntime::with_proxy_final_tasks(Arc::clone(task_relay))
                            .expect(
                                "proxy final Tasks must install into an empty extension registry",
                            );
                    runtime
                        .freeze()
                        .expect("proxy final Tasks extension descriptors must freeze");
                    Some(Arc::new(runtime))
                }
                (None, None) => None,
                (Some(_), Some(_)) => unreachable!("builder rejects mixed final Tasks owners"),
            },
            #[cfg(all(feature = "tasks", not(feature = "proxy")))]
            None => match final_task_runtime.as_ref() {
                Some(task_runtime) => {
                    let mut runtime = ServerExtensionRuntime::with_final_tasks(task_runtime)
                        .expect("final Tasks must install into an empty extension registry");
                    runtime
                        .freeze()
                        .expect("final Tasks extension descriptors must freeze");
                    Some(Arc::new(runtime))
                }
                None => None,
            },
            #[cfg(not(feature = "tasks"))]
            None => None,
        };

        Server {
            info: self.info,
            title: self.title,
            description: self.description,
            website_url: self.website_url,
            icons: self.icons,
            capabilities: self.capabilities,
            router: Arc::new(self.router),
            instructions: self.instructions,
            request_timeout_secs: self.request_timeout_secs,
            stats: if self.stats_enabled {
                Some(ServerStats::new())
            } else {
                None
            },
            mask_error_details: self.mask_error_details,
            logging: self.logging,
            console_config: self.console_config,
            console,
            lifespan: Mutex::new(Some(self.lifespan)),
            auth_provider: self.auth_provider,
            middleware: Arc::new(self.middleware),
            active_requests: Arc::new(Mutex::new(HashMap::new())),
            #[cfg(all(test, feature = "tasks"))]
            task_manager: self.task_manager,
            max_bidirectional_requests_per_connection: self
                .max_bidirectional_requests_per_connection,
            protocol_policy: self.protocol_policy,
            #[cfg(any(feature = "legacy-2024-11-05", test))]
            legacy_application_tool_content: self.legacy_application_tool_content,
            http_config: self.http_config,
            oauth_http_routes: self.oauth_http_routes,
            extension_runtime,
            #[cfg(feature = "tasks")]
            final_task_runtime: self.final_task_runtime,
            #[cfg(all(feature = "proxy", feature = "tasks"))]
            final_task_relay,
            final_subscriptions,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(all(feature = "proxy", feature = "tasks"))]
    use crate::proxy::{ProxyFinalTaskListener, ProxyFinalTaskListenerEvent};
    #[cfg(feature = "proxy")]
    use asupersync::Cx;
    #[cfg(all(feature = "proxy", feature = "tasks"))]
    use fastmcp_client::FinalToolCallOutcome;
    #[cfg(all(feature = "proxy", feature = "tasks"))]
    use fastmcp_core::block_on;
    use fastmcp_core::{McpContext, McpResult};
    #[cfg(feature = "apps")]
    use fastmcp_protocol::FinalTool;
    #[cfg(feature = "apps")]
    use fastmcp_protocol::common_types::AbsoluteUri;
    #[cfg(feature = "proxy")]
    use fastmcp_protocol::common_types::{ContentBlock, Implementation};
    #[cfg(feature = "apps")]
    use fastmcp_protocol::extensions::ExtensionNegotiationError;
    use fastmcp_protocol::protocol_policy::ProtocolPolicy;
    #[cfg(feature = "proxy")]
    use fastmcp_protocol::protocol_policy::{ProtocolEra, StdioOpeningFrame};
    #[cfg(feature = "proxy")]
    use fastmcp_protocol::{
        CallToolResult, CompleteResult, CoreResult, FinalCallToolResult, FinalCoreResult,
        JsonRpcRequest, LegacyContent, LegacyCoreResult, ResultMeta,
    };
    use fastmcp_protocol::{Content, Prompt, Resource, ResourceContent, Tool};
    #[cfg(all(feature = "proxy", feature = "tasks"))]
    use fastmcp_protocol::{
        CoreResultDiscriminatorPolicy, CreateTaskResult, DecodedResult, EmptyTaskResult,
        ResultPeerEra, SubscriptionFilter, decode_peer_result,
    };

    // ── Stub handlers ────────────────────────────────────────────────

    struct TestTool;
    impl crate::ToolHandler for TestTool {
        fn definition(&self) -> Tool {
            Tool {
                name: "test_tool".to_string(),
                description: Some("a test tool".to_string()),
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: None,
                icon: None,
                version: None,
                tags: vec![],
                annotations: None,
            }
        }
        fn call(&self, _ctx: &McpContext, _args: serde_json::Value) -> McpResult<Vec<Content>> {
            Ok(vec![Content::text("ok")])
        }
    }

    struct ExactLegacyOnlyTool;
    impl crate::ToolHandler for ExactLegacyOnlyTool {
        fn definition(&self) -> Tool {
            Tool {
                name: "exact_legacy_only".to_owned(),
                description: Some("exact 2024-only test tool".to_owned()),
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: Some(serde_json::json!(false)),
                icon: None,
                version: None,
                tags: Vec::new(),
                annotations: None,
            }
        }

        fn call(&self, _ctx: &McpContext, _args: serde_json::Value) -> McpResult<Vec<Content>> {
            Ok(vec![Content::text("legacy")])
        }
    }

    #[cfg(feature = "apps")]
    struct MountedAppsTool;

    #[cfg(feature = "apps")]
    impl crate::ToolHandler for MountedAppsTool {
        fn definition(&self) -> Tool {
            Tool {
                name: "mounted_apps_tool".to_owned(),
                description: Some("final-only Apps mount fixture".to_owned()),
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: None,
                icon: None,
                version: None,
                tags: Vec::new(),
                annotations: None,
            }
        }

        fn final_definition(&self) -> Option<FinalTool> {
            let metadata = fastmcp_protocol::McpAppsToolMetadata::try_new(
                Some(
                    AbsoluteUri::parse("ui://mount/dashboard")
                        .expect("fixed Apps mount URI is valid"),
                ),
                None,
            )
            .expect("fixed Apps mount metadata is valid")
            .to_open_metadata()
            .expect("fixed Apps mount metadata serializes");
            Some(FinalTool {
                name: "mounted_apps_tool".to_owned(),
                title: None,
                description: Some("final-only Apps mount fixture".to_owned()),
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: None,
                annotations: None,
                icons: None,
                meta: Some(metadata),
            })
        }

        fn call(&self, _ctx: &McpContext, _args: serde_json::Value) -> McpResult<Vec<Content>> {
            Ok(vec![Content::text("Apps mount fixture")])
        }
    }

    #[cfg(feature = "apps")]
    fn apps_mount_child() -> crate::Server {
        let resource = McpAppsUiResource::try_new(
            AbsoluteUri::parse("ui://mount/dashboard").expect("fixed Apps mount URI is valid"),
            "mounted-apps-dashboard",
            "<main>Apps mount fixture</main>",
        )
        .expect("fixed Apps mount resource is valid");
        ServerBuilder::new("apps-child", "1.0")
            .mcp_apps()
            .expect("child Apps extension installs")
            .mcp_apps_ui_resource(resource)
            .expect("child Apps resource registers")
            .mcp_apps_tool(MountedAppsTool)
            .expect("child Apps tool registers")
            .build()
    }

    #[cfg(feature = "apps")]
    fn apps_resource_bound_tool_mount_child() -> crate::Server {
        let resource = McpAppsUiResource::try_new(
            AbsoluteUri::parse("ui://mount/dashboard").expect("fixed Apps mount URI is valid"),
            "mounted-apps-dashboard",
            "<main>Apps mount fixture</main>",
        )
        .expect("fixed Apps mount resource is valid");
        ServerBuilder::new("apps-resource-child", "1.0")
            .mcp_apps()
            .expect("child Apps extension installs")
            .mcp_apps_ui_resource(resource)
            .expect("child Apps resource registers")
            .tool(TestTool)
            .build()
    }

    struct TestResource;
    impl crate::ResourceHandler for TestResource {
        fn definition(&self) -> Resource {
            Resource {
                uri: "file:///test".to_string(),
                name: "test_res".to_string(),
                description: None,
                mime_type: None,
                icon: None,
                version: None,
                tags: vec![],
            }
        }
        fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
            Ok(vec![ResourceContent {
                uri: "file:///test".to_string(),
                mime_type: None,
                text: Some("content".to_string()),
                blob: None,
            }])
        }
    }

    struct TestPrompt;
    impl crate::PromptHandler for TestPrompt {
        fn definition(&self) -> Prompt {
            Prompt {
                name: "test_prompt".to_string(),
                description: None,
                arguments: vec![],
                icon: None,
                version: None,
                tags: vec![],
            }
        }
        fn get(
            &self,
            _ctx: &McpContext,
            _args: std::collections::HashMap<String, String>,
        ) -> McpResult<Vec<fastmcp_protocol::PromptMessage>> {
            Ok(vec![])
        }
    }

    struct TestCompletion;

    impl crate::handler::CompletionHandler for TestCompletion {
        fn complete_legacy(
            &self,
            _ctx: &McpContext,
            _params: fastmcp_protocol::LegacyCompletionParams,
        ) -> McpResult<fastmcp_protocol::CompletionValues> {
            Ok(fastmcp_protocol::CompletionValues {
                values: vec!["staging".to_string()],
                total: Some(1),
                has_more: Some(false),
            })
        }

        fn complete_final(
            &self,
            _ctx: &McpContext,
            _params: fastmcp_protocol::FinalCompletionParams,
        ) -> McpResult<fastmcp_protocol::FinalCompletionValues> {
            Ok(fastmcp_protocol::FinalCompletionValues {
                values: vec!["staging".to_string()],
                total: Some(fastmcp_protocol::JsonInteger::from(1_i64)),
                has_more: Some(false),
            })
        }
    }

    struct CountingCompletion(std::sync::Arc<std::sync::atomic::AtomicUsize>);

    impl crate::handler::CompletionHandler for CountingCompletion {
        fn complete_legacy(
            &self,
            _ctx: &McpContext,
            _params: fastmcp_protocol::LegacyCompletionParams,
        ) -> McpResult<fastmcp_protocol::CompletionValues> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(fastmcp_protocol::CompletionValues {
                values: vec!["staging".to_string()],
                total: Some(1),
                has_more: Some(false),
            })
        }

        fn complete_final(
            &self,
            _ctx: &McpContext,
            _params: fastmcp_protocol::FinalCompletionParams,
        ) -> McpResult<fastmcp_protocol::FinalCompletionValues> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(fastmcp_protocol::FinalCompletionValues {
                values: vec!["staging".to_string()],
                total: Some(fastmcp_protocol::JsonInteger::from(1_i64)),
                has_more: Some(false),
            })
        }
    }

    struct MarkedTool(&'static str);

    impl crate::ToolHandler for MarkedTool {
        fn definition(&self) -> Tool {
            Tool {
                name: "duplicate_tool".to_string(),
                description: Some(self.0.to_string()),
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: None,
                icon: None,
                version: None,
                tags: vec![self.0.to_string()],
                annotations: None,
            }
        }

        fn call(&self, _ctx: &McpContext, _args: serde_json::Value) -> McpResult<Vec<Content>> {
            Ok(vec![Content::text(self.0)])
        }
    }

    struct MarkedResource(&'static str);

    impl crate::ResourceHandler for MarkedResource {
        fn definition(&self) -> Resource {
            Resource {
                uri: "duplicate://resource".to_string(),
                name: self.0.to_string(),
                description: Some(self.0.to_string()),
                mime_type: None,
                icon: None,
                version: None,
                tags: vec![self.0.to_string()],
            }
        }

        fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
            Ok(vec![])
        }
    }

    struct MarkedPrompt(&'static str);

    impl crate::PromptHandler for MarkedPrompt {
        fn definition(&self) -> Prompt {
            Prompt {
                name: "duplicate_prompt".to_string(),
                description: Some(self.0.to_string()),
                arguments: vec![],
                icon: None,
                version: None,
                tags: vec![self.0.to_string()],
            }
        }

        fn get(
            &self,
            _ctx: &McpContext,
            _args: std::collections::HashMap<String, String>,
        ) -> McpResult<Vec<fastmcp_protocol::PromptMessage>> {
            Ok(vec![])
        }
    }

    fn marked_resource_template(marker: &str) -> ResourceTemplate {
        ResourceTemplate {
            uri_template: "mcp://duplicate-item/{item}".to_string(),
            name: marker.to_string(),
            description: Some(marker.to_string()),
            mime_type: None,
            icon: None,
            version: None,
            tags: vec![marker.to_string()],
        }
    }

    fn marked_builder(marker: &'static str) -> ServerBuilder {
        ServerBuilder::new("marked", "1.0")
            .tool(MarkedTool(marker))
            .resource(MarkedResource(marker))
            .resource_template(marked_resource_template(marker))
            .prompt(MarkedPrompt(marker))
    }

    fn assert_marked_server(server: &crate::Server, marker: &str) {
        assert_eq!(server.tools().len(), 1);
        assert_eq!(server.resources().len(), 1);
        assert_eq!(server.resource_templates().len(), 1);
        assert_eq!(server.prompts().len(), 1);
        assert_eq!(server.tools()[0].tags, vec![marker.to_string()]);
        assert_eq!(server.resources()[0].tags, vec![marker.to_string()]);
        assert_eq!(
            server.resource_templates()[0].tags,
            vec![marker.to_string()]
        );
        assert_eq!(server.prompts()[0].tags, vec![marker.to_string()]);
    }

    #[cfg(feature = "proxy")]
    struct DuplicatePolicyProxyBackend;

    #[cfg(feature = "proxy")]
    impl crate::proxy::ProxyBackend for DuplicatePolicyProxyBackend {
        fn list_tools(&mut self) -> McpResult<Vec<Tool>> {
            Ok(duplicate_policy_proxy_catalog().tools)
        }

        fn list_resources(&mut self) -> McpResult<Vec<Resource>> {
            Ok(duplicate_policy_proxy_catalog().resources)
        }

        fn list_resource_templates(&mut self) -> McpResult<Vec<ResourceTemplate>> {
            Ok(duplicate_policy_proxy_catalog().resource_templates)
        }

        fn list_prompts(&mut self) -> McpResult<Vec<Prompt>> {
            Ok(duplicate_policy_proxy_catalog().prompts)
        }

        fn call_tool(&mut self, _: &str, _: serde_json::Value) -> McpResult<Vec<Content>> {
            Ok(vec![])
        }

        fn call_tool_with_progress(
            &mut self,
            _: &str,
            _: serde_json::Value,
            _: crate::proxy::ProgressCallback<'_>,
        ) -> McpResult<Vec<Content>> {
            Ok(vec![])
        }

        fn read_resource(&mut self, _: &str) -> McpResult<Vec<ResourceContent>> {
            Ok(vec![])
        }

        fn get_prompt(
            &mut self,
            _: &str,
            _: std::collections::HashMap<String, String>,
        ) -> McpResult<Vec<fastmcp_protocol::PromptMessage>> {
            Ok(vec![])
        }
    }

    #[cfg(all(feature = "proxy", feature = "tasks"))]
    struct OrdinaryProxyTasksListener {
        accepted: Option<SubscriptionFilter>,
    }

    #[cfg(all(feature = "proxy", feature = "tasks"))]
    impl ProxyFinalTaskListener for OrdinaryProxyTasksListener {
        fn next(
            &mut self,
            _cx: &Cx,
            _request_cancellation: &fastmcp_core::McpRequestCancellation,
        ) -> McpResult<ProxyFinalTaskListenerEvent> {
            match self.accepted.take() {
                Some(accepted) => Ok(ProxyFinalTaskListenerEvent::Acknowledged(accepted)),
                None => Ok(ProxyFinalTaskListenerEvent::Terminal),
            }
        }
    }

    /// A modern-only upstream whose public final tools/call path can return
    /// either official Tasks or MRTR input_required results. The test uses it
    /// only through the ordinary public `ServerBuilder::proxy` entry point.
    #[cfg(all(feature = "proxy", feature = "tasks"))]
    struct OrdinaryProxyTasksBackend {
        calls: Arc<Mutex<Vec<String>>>,
        updates: Arc<Mutex<Vec<serde_json::Value>>>,
        task: CreateTaskResult,
    }

    #[cfg(all(feature = "proxy", feature = "tasks"))]
    impl crate::proxy::ProxyBackend for OrdinaryProxyTasksBackend {
        fn list_tools(&mut self) -> McpResult<Vec<Tool>> {
            Ok(Vec::new())
        }

        fn list_resources(&mut self) -> McpResult<Vec<Resource>> {
            Ok(Vec::new())
        }

        fn list_resource_templates(&mut self) -> McpResult<Vec<ResourceTemplate>> {
            Ok(Vec::new())
        }

        fn list_prompts(&mut self) -> McpResult<Vec<Prompt>> {
            Ok(Vec::new())
        }

        fn call_tool(
            &mut self,
            _name: &str,
            _arguments: serde_json::Value,
        ) -> McpResult<Vec<Content>> {
            Err(fastmcp_core::McpError::internal_error(
                "ordinary Tasks proxy test must retain the final result algebra",
            ))
        }

        fn call_tool_with_progress(
            &mut self,
            name: &str,
            arguments: serde_json::Value,
            _on_progress: crate::proxy::ProgressCallback<'_>,
        ) -> McpResult<Vec<Content>> {
            self.call_tool(name, arguments)
        }

        fn read_resource(&mut self, _uri: &str) -> McpResult<Vec<ResourceContent>> {
            Err(fastmcp_core::McpError::internal_error("not used"))
        }

        fn get_prompt(
            &mut self,
            _name: &str,
            _arguments: std::collections::HashMap<String, String>,
        ) -> McpResult<Vec<fastmcp_protocol::PromptMessage>> {
            Err(fastmcp_core::McpError::internal_error("not used"))
        }

        fn supports_final_tasks_relay(&mut self) -> McpResult<bool> {
            Ok(true)
        }

        fn call_tool_final_outcome(
            &mut self,
            name: &str,
            arguments: serde_json::Value,
        ) -> McpResult<FinalToolCallOutcome> {
            self.calls
                .lock()
                .expect("ordinary proxy task call log is not poisoned")
                .push(format!("tools/call:{name}"));
            if arguments.get("outcome") == Some(&serde_json::json!("task")) {
                return Ok(FinalToolCallOutcome::Task(self.task.clone()));
            }
            let (decoded, diagnostic) = decode_peer_result(
                r#"{"resultType":"input_required","requestState":"upstream-forged-state"}"#,
                ResultPeerEra::Modern,
                &CoreResultDiscriminatorPolicy,
            )
            .map_err(|error| fastmcp_core::McpError::invalid_request(error.to_string()))?;
            assert!(diagnostic.is_none(), "the fixed final fixture is explicit");
            let DecodedResult::InputRequired(result) = decoded else {
                return Err(fastmcp_core::McpError::internal_error(
                    "ordinary proxy test fixture must decode as input_required",
                ));
            };
            Ok(FinalToolCallOutcome::InputRequired(result))
        }

        fn update_final_task(
            &mut self,
            task: &fastmcp_protocol::Task,
            input_responses: fastmcp_protocol::TaskInputResponses,
        ) -> McpResult<fastmcp_protocol::UpdateTaskResult> {
            self.calls
                .lock()
                .expect("ordinary proxy task call log is not poisoned")
                .push("tasks/update".to_owned());
            assert_eq!(
                task.base().task_id.as_str(),
                self.task.task.base().task_id.as_str(),
                "the relay supplies the retained upstream task rather than admitting an arbitrary id"
            );
            self.updates
                .lock()
                .expect("ordinary proxy task update receipt is not poisoned")
                .push(
                    serde_json::to_value(&input_responses)
                        .expect("the exact public Tasks input response map serializes"),
                );
            Ok(EmptyTaskResult::default())
        }

        fn open_final_task_listener(
            &mut self,
            notifications: SubscriptionFilter,
        ) -> McpResult<Box<dyn ProxyFinalTaskListener>> {
            self.calls
                .lock()
                .expect("ordinary proxy task call log is not poisoned")
                .push("subscriptions/listen".to_owned());
            Ok(Box::new(OrdinaryProxyTasksListener {
                accepted: Some(notifications),
            }))
        }
    }

    #[cfg(feature = "proxy")]
    fn duplicate_policy_proxy_catalog() -> ProxyCatalog {
        ProxyCatalog {
            tool_catalog_era: Some(ProtocolEra::Legacy2024),
            tools: vec![Tool {
                name: "test_tool".to_string(),
                description: Some("proxied tool".to_string()),
                // Final projection of a proxied legacy tool requires an
                // object-typed input schema.
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: None,
                icon: None,
                version: None,
                tags: vec![],
                annotations: None,
            }],
            resources: vec![Resource {
                uri: "file:///test".to_string(),
                name: "proxied resource".to_string(),
                description: Some("proxied resource".to_string()),
                mime_type: None,
                icon: None,
                version: None,
                tags: vec![],
            }],
            prompts: vec![Prompt {
                name: "test_prompt".to_string(),
                description: Some("proxied prompt".to_string()),
                arguments: vec![],
                icon: None,
                version: None,
                tags: vec![],
            }],
            ..ProxyCatalog::default()
        }
    }

    // ── Builder defaults ─────────────────────────────────────────────

    #[test]
    fn builder_new_sets_info() {
        let builder = ServerBuilder::new("my-server", "2.0.0");
        let server = builder.build();
        assert_eq!(server.info().name, "my-server");
        assert_eq!(server.info().version, "2.0.0");
    }

    #[test]
    fn builder_default_has_logging_capability() {
        let builder = ServerBuilder::new("srv", "1.0");
        let server = builder.build();
        assert!(server.capabilities().logging.is_some());
    }

    #[test]
    fn builder_default_has_no_tool_resource_prompt_capabilities() {
        let builder = ServerBuilder::new("srv", "1.0");
        let server = builder.build();
        assert!(server.capabilities().tools.is_none());
        assert!(server.capabilities().resources.is_none());
        assert!(server.capabilities().prompts.is_none());
    }

    #[cfg(feature = "apps")]
    #[test]
    fn builder_manual_apps_discovery_requires_the_exact_empty_marker_before_build() {
        let mut accepted_descriptors = fastmcp_protocol::ExtensionDescriptorRegistry::new();
        let accepted_id =
            fastmcp_protocol::register_official_mcp_apps_extension(&mut accepted_descriptors)
                .expect("the official Apps descriptor registers");
        let accepted = ServerBuilder::new("apps-marker", "1.0").extension_registry(
            crate::ExtensionHandlerRegistry::new(accepted_descriptors),
            ServerExtensionDiscovery {
                extensions: std::collections::BTreeMap::from([(
                    accepted_id,
                    fastmcp_protocol::official_mcp_apps_empty_server_settings(),
                )]),
            },
            |_descriptor: &fastmcp_protocol::ExtensionDescriptor,
             _client: &fastmcp_protocol::ExtensionSettings,
             _server: &fastmcp_protocol::ExtensionSettings|
             -> Result<fastmcp_protocol::ExtensionSettings, ExtensionNegotiationError> {
                Ok(fastmcp_protocol::official_mcp_apps_empty_server_settings())
            },
        );
        assert!(
            accepted.is_ok(),
            "the exact empty official Apps marker is accepted during builder configuration"
        );

        let mut rejected_descriptors = fastmcp_protocol::ExtensionDescriptorRegistry::new();
        let rejected_id =
            fastmcp_protocol::register_official_mcp_apps_extension(&mut rejected_descriptors)
                .expect("the official Apps descriptor registers");
        let rejected = ServerBuilder::new("apps-marker", "1.0").extension_registry(
            crate::ExtensionHandlerRegistry::new(rejected_descriptors),
            ServerExtensionDiscovery {
                extensions: std::collections::BTreeMap::from([(
                    rejected_id,
                    fastmcp_protocol::ExtensionSettings::new(serde_json::json!({
                        "unexpected": true,
                    }))
                    .expect("the one-field alternate is generic extension metadata"),
                )]),
            },
            |_descriptor: &fastmcp_protocol::ExtensionDescriptor,
             _client: &fastmcp_protocol::ExtensionSettings,
             _server: &fastmcp_protocol::ExtensionSettings|
             -> Result<fastmcp_protocol::ExtensionSettings, ExtensionNegotiationError> {
                Ok(fastmcp_protocol::official_mcp_apps_empty_server_settings())
            },
        );
        assert!(
            matches!(
                rejected,
                Err(crate::ServerExtensionConfigurationError::Registry(
                    fastmcp_protocol::ExtensionRegistryError::OfficialMcpAppsServerSettingsNotEmpty
                ))
            ),
            "adding one discovery setting is rejected by extension_registry before build"
        );
    }

    #[test]
    fn builder_completion_handler_activates_exact_discovery_capability() {
        let server = ServerBuilder::new("srv", "1.0")
            .completion_handler(TestCompletion)
            .build();
        let discovery = server
            .server_discovery()
            .expect("installed completion handler produces discovery");
        let wire = serde_json::to_value(discovery).expect("discovery serializes");

        assert_eq!(wire["capabilities"]["completions"], serde_json::json!({}));
    }

    #[test]
    fn builder_provider_specific_completion_requires_an_admitted_final_target() {
        let unmatched = ServerBuilder::new("srv", "1.0")
            .prompt_completion_handler("duplicate_prompt", TestCompletion)
            .build();
        let unmatched_discovery = unmatched
            .server_discovery()
            .expect("the unmatched provider-only server still discovers");
        let unmatched_wire =
            serde_json::to_value(unmatched_discovery).expect("discovery serializes");
        assert!(
            unmatched_wire["capabilities"].get("completions").is_none(),
            "an unbound provider-only registration must not advertise completion"
        );

        let matched = ServerBuilder::new("srv", "1.0")
            .prompt(MarkedPrompt("provider-target"))
            .prompt_completion_handler("duplicate_prompt", TestCompletion)
            .build();
        let matched_discovery = matched
            .server_discovery()
            .expect("an admitted final prompt activates its provider route");
        let matched_wire = serde_json::to_value(matched_discovery).expect("discovery serializes");

        assert_eq!(
            matched_wire["capabilities"]["completions"],
            serde_json::json!({}),
            "adding only the final prompt target makes the provider discoverable"
        );
    }

    #[test]
    fn builder_without_completion_handler_omits_discovery_capability() {
        let server = ServerBuilder::new("srv", "1.0").build();
        let discovery = server
            .server_discovery()
            .expect("server without completion handler still discovers");
        let wire = serde_json::to_value(discovery).expect("discovery serializes");

        assert!(
            wire["capabilities"].get("completions").is_none(),
            "absence of the handler must not advertise completion"
        );
    }

    #[test]
    fn builder_completion_handler_rejects_final_metadata_before_handler_state_changes() {
        let invocations = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let server = ServerBuilder::new("srv", "1.0")
            .completion_handler(CountingCompletion(std::sync::Arc::clone(&invocations)))
            .build();
        let request_ctx = McpContext::new(asupersync::Cx::for_testing(), 93);
        let baseline = fastmcp_protocol::JsonRpcRequest::new(
            "completion/complete",
            Some(serde_json::json!({
                "ref": {"type": "ref/prompt", "name": "deploy"},
                "argument": {"name": "environment", "value": "sta"},
            })),
            93_i64,
        );
        let mut planted = baseline.clone();
        planted
            .params
            .as_mut()
            .and_then(serde_json::Value::as_object_mut)
            .expect("completion parameters are an object")
            .insert(
                "_meta".to_string(),
                serde_json::json!({
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                }),
            );

        let baseline_result = fastmcp_core::block_on(
            server
                .router
                .dispatch_legacy_completion(&request_ctx, &baseline),
        )
        .expect("baseline legacy completion reaches the builder-installed handler");
        assert_eq!(
            invocations.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the accepted request invokes the installed handler once"
        );
        let planted_before = serde_json::to_vec(&planted).expect("planted request serializes");

        let error = fastmcp_core::block_on(
            server
                .router
                .dispatch_legacy_completion(&request_ctx, &planted),
        )
        .expect_err("the sole final metadata field is refused in the exact legacy route");
        assert_eq!(error.code, fastmcp_core::McpErrorCode::InvalidParams);
        assert_eq!(
            invocations.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "cross-era rejection occurs before handler-owned state can change"
        );
        assert_eq!(
            serde_json::to_vec(&planted).expect("rejected request serializes"),
            planted_before,
            "the rejected request remains caller-owned and unchanged"
        );
        assert_eq!(
            fastmcp_core::block_on(
                server
                    .router
                    .dispatch_legacy_completion(&request_ctx, &baseline),
            )
            .expect("baseline remains dispatchable after the rejection"),
            baseline_result,
            "the rejected one-field variant cannot alter the accepted completion result"
        );
        assert_eq!(
            invocations.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "only accepted requests mutate handler-owned state"
        );
    }

    #[test]
    fn builder_default_stats_enabled() {
        let server = ServerBuilder::new("srv", "1.0").build();
        assert!(server.stats().is_some());
    }

    #[test]
    fn builder_default_request_timeout() {
        let builder = ServerBuilder::new("srv", "1.0");
        assert_eq!(builder.request_timeout_secs(), DEFAULT_REQUEST_TIMEOUT_SECS);
    }

    #[test]
    fn builder_bidirectional_limit_has_exact_validated_boundaries() {
        let default = ServerBuilder::new("srv", "1.0");
        assert_eq!(
            default.max_bidirectional_requests_per_connection,
            crate::bidirectional::DEFAULT_MAX_IN_FLIGHT_REQUESTS
        );

        for valid in [1, crate::bidirectional::HARD_MAX_IN_FLIGHT_REQUESTS] {
            let server = ServerBuilder::new("srv", "1.0")
                .max_bidirectional_requests_per_connection(valid)
                .expect("boundary must be valid")
                .build();
            assert_eq!(server.max_bidirectional_requests_per_connection, valid);
            assert_eq!(
                server.new_pending_requests_for_connection().max_in_flight(),
                valid
            );
        }

        for invalid in [0, crate::bidirectional::HARD_MAX_IN_FLIGHT_REQUESTS + 1] {
            let Err(error) =
                ServerBuilder::new("srv", "1.0").max_bidirectional_requests_per_connection(invalid)
            else {
                panic!("out-of-range limit must fail closed");
            };
            assert_eq!(error.code, fastmcp_core::McpErrorCode::InvalidParams);
        }
    }

    #[test]
    fn builder_default_error_masking_disabled() {
        let builder = ServerBuilder::new("srv", "1.0");
        assert!(!builder.is_error_masking_enabled());
    }

    #[test]
    fn builder_default_strict_validation_disabled() {
        let builder = ServerBuilder::new("srv", "1.0");
        assert!(!builder.is_strict_input_validation_enabled());
    }

    #[test]
    fn explicit_protocol_policy_applies_without_reserved_launch_setting() {
        let server = ServerBuilder::from_launch_protocol_policy("srv", "1.0", Ok(None))
            .expect("unset launch policy must construct a builder")
            .protocol_policy(ProtocolPolicy::ModernOnly)
            .expect("ModernOnly must be available to this test build")
            .try_build()
            .expect("unset launch policy must permit a server");

        assert_eq!(server.protocol_policy(), ProtocolPolicy::ModernOnly);
    }

    #[test]
    fn launch_policy_unset_defaults_to_auto() {
        assert_eq!(protocol_policy_from_server_launch_value(None), Ok(None));
    }

    #[cfg(not(feature = "legacy-2024-11-05"))]
    #[test]
    fn no_legacy_public_builder_defaults_to_modern_only() {
        let server = ServerBuilder::try_new("srv", "1.0")
            .expect("no-legacy construction must succeed")
            .try_build()
            .expect("the default no-legacy builder must build");

        assert_eq!(server.protocol_policy(), ProtocolPolicy::ModernOnly);
    }

    #[cfg(not(feature = "legacy-2024-11-05"))]
    #[test]
    fn no_legacy_source_exposes_a_modern_endpoint_builder_and_gates_the_dual_era_one() {
        let source = include_str!("builder.rs").replace("\r\n", "\n");
        assert!(source.contains(
            "/// Builds a live modern Streamable HTTP endpoint.\n    #[cfg(not(any(feature = \"legacy-2024-11-05\", test)))]\n    pub fn build_http_endpoint"
        ));
        assert!(
            source.contains(
                "/// Builds a live dual-era HTTP endpoint with an exact legacy SSE origin."
            )
        );
        assert!(source.contains("crate::ServerHttpEndpointError"));
        assert!(!source.contains("fastmcp_transport::http::DualEraHttpEndpointError"));
    }

    #[cfg(not(feature = "legacy-2024-11-05"))]
    #[test]
    fn no_legacy_public_builder_rejects_legacy_policies_without_mutation() {
        for policy in [ProtocolPolicy::Auto, ProtocolPolicy::LegacyOnly] {
            let mut builder =
                ServerBuilder::try_new("srv", "1.0").expect("no-legacy construction must succeed");

            assert_eq!(
                builder.try_set_protocol_policy(policy),
                Err(ServerLaunchPolicyError::FeatureUnavailable),
                "{policy:?} must reject before changing a no-legacy builder"
            );

            let server = builder
                .try_build()
                .expect("a rejected policy must leave the builder buildable");
            assert_eq!(
                server.protocol_policy(),
                ProtocolPolicy::ModernOnly,
                "{policy:?} differs from the default only by requiring unavailable legacy behavior"
            );
        }
    }

    #[cfg(feature = "legacy-2024-11-05")]
    #[test]
    fn legacy_enabled_public_builder_preserves_auto() {
        let mut builder =
            ServerBuilder::try_new("srv", "1.0").expect("legacy-enabled construction must succeed");
        builder
            .try_set_protocol_policy(ProtocolPolicy::Auto)
            .expect("Auto must remain available with the legacy adapter enabled");

        let server = builder
            .try_build()
            .expect("legacy-enabled Auto builder must build");
        assert_eq!(server.protocol_policy(), ProtocolPolicy::Auto);
    }

    #[test]
    fn launch_policy_accepts_exact_public_values() {
        assert_eq!(
            protocol_policy_from_server_launch_value(Some(OsStr::new("auto"))),
            Ok(Some(ProtocolPolicy::Auto))
        );
        assert_eq!(
            protocol_policy_from_server_launch_value(Some(OsStr::new("modern-only"))),
            Ok(Some(ProtocolPolicy::ModernOnly))
        );
        assert_eq!(
            protocol_policy_from_server_launch_value(Some(OsStr::new("legacy-only"))),
            Ok(Some(ProtocolPolicy::LegacyOnly))
        );
    }

    #[cfg(feature = "legacy-2024-11-05")]
    #[test]
    fn valid_launch_policy_wins_over_explicit_builder_policy() {
        let server = ServerBuilder::from_launch_protocol_policy(
            "srv",
            "1.0",
            Ok(Some(ProtocolPolicy::ModernOnly)),
        )
        .expect("valid launch policy must construct a builder")
        .protocol_policy(ProtocolPolicy::LegacyOnly)
        .expect("the unit-test dual-era build supports LegacyOnly")
        .try_build()
        .expect("valid launch policy must build");

        assert_eq!(server.protocol_policy(), ProtocolPolicy::ModernOnly);
    }

    #[cfg(feature = "legacy-2024-11-05")]
    #[test]
    fn fixed_policy_constructor_reserves_policy_against_later_setter() {
        let server = ServerBuilder::try_new_with_fixed_protocol_policy(
            "srv",
            "1.0",
            ProtocolPolicy::ModernOnly,
        )
        .expect("ModernOnly is available in every feature profile")
        .protocol_policy(ProtocolPolicy::LegacyOnly)
        .expect("the later policy is valid in this dual-era test build")
        .try_build()
        .expect("the fixed-policy builder remains buildable");

        assert_eq!(
            server.protocol_policy(),
            ProtocolPolicy::ModernOnly,
            "the fixed component policy must not be replaced by a later builder setter"
        );
    }

    #[cfg(not(feature = "legacy-2024-11-05"))]
    #[test]
    fn fixed_policy_constructor_rejects_unavailable_policy_before_construction() {
        assert!(matches!(
            ServerBuilder::try_new_with_fixed_protocol_policy("srv", "1.0", ProtocolPolicy::Auto,),
            Err(ServerLaunchPolicyError::FeatureUnavailable)
        ));
    }

    // FROZEN-ID ALIAS -- ONE BEHAVIOUR, TWO RUNNABLE NAMES.
    // ahet.3.3's acceptance names this behaviour under two conventions, so both
    // are made runnable over a single body rather than duplicated: duplicated
    // bodies drift, and a frozen pair that has quietly stopped asserting the
    // same thing is worse than a single test.
    // A runner counting IDs reports 2 passed here. That is 1 behaviour.
    #[test]
    fn fnd_03_i_unit_positive() {
        fnd_03_integration_unit_positive();
    }

    #[test]
    fn fnd_03_integration_unit_positive() {
        use fastmcp_protocol::protocol_policy::{ProtocolVersion, ProtocolVersionError};

        let builder = ServerBuilder::try_new("fnd03-integration", "1.0.0")
            .expect("server builder must construct through public try_new");

        #[cfg(feature = "legacy-2024-11-05")]
        assert_eq!(builder.configured_protocol_policy(), ProtocolPolicy::Auto);
        #[cfg(not(feature = "legacy-2024-11-05"))]
        assert_eq!(
            builder.configured_protocol_policy(),
            ProtocolPolicy::ModernOnly
        );

        let mut modern_builder = builder;
        modern_builder
            .try_set_protocol_policy(ProtocolPolicy::ModernOnly)
            .expect("ModernOnly must be settable on any build");
        assert_eq!(
            modern_builder.configured_protocol_policy(),
            ProtocolPolicy::ModernOnly
        );

        let server = modern_builder.try_build().expect("modern server builds");
        assert_eq!(server.protocol_policy(), ProtocolPolicy::ModernOnly);

        assert_eq!(
            ProtocolVersion::parse("2026-07-28"),
            Ok(ProtocolVersion::MODERN_2026)
        );
        #[cfg(feature = "legacy-2024-11-05")]
        assert_eq!(
            ProtocolVersion::parse("2024-11-05"),
            Ok(ProtocolVersion::LEGACY_2024)
        );
        assert_eq!(
            ProtocolVersion::parse("2025-11-25"),
            Err(ProtocolVersionError::UnsupportedVersion {
                received: "2025-11-25".to_owned(),
            })
        );
    }

    // FROZEN-ID ALIAS -- ONE BEHAVIOUR, TWO RUNNABLE NAMES. See the note on
    // `fnd_03_i_unit_positive` above.
    // A runner counting IDs reports 2 passed here. That is 1 behaviour.
    #[test]
    fn fnd_03_i_unit_planted_negative() {
        fnd_03_integration_unit_planted_negative();
    }

    #[test]
    fn fnd_03_integration_unit_planted_negative() {
        use fastmcp_protocol::protocol_policy::{ProtocolVersion, ProtocolVersionError};

        let mut builder = ServerBuilder::try_new("fnd03-integration-neg", "1.0.0")
            .expect("server builder must construct through public try_new");
        let initial_policy = builder.configured_protocol_policy();

        let parse_err = ProtocolVersion::parse("2025-11-25")
            .expect_err("unsupported 2025-11-25 must fail at mirror/policy boundary");
        assert_eq!(
            parse_err,
            ProtocolVersionError::UnsupportedVersion {
                received: "2025-11-25".to_owned(),
            }
        );

        assert_eq!(builder.configured_protocol_policy(), initial_policy);

        #[cfg(not(feature = "legacy-2024-11-05"))]
        {
            let err = builder
                .try_set_protocol_policy(ProtocolPolicy::Auto)
                .expect_err("Auto must fail on feature-stripped builds");
            assert_eq!(err, ServerLaunchPolicyError::FeatureUnavailable);
            assert_eq!(
                builder.configured_protocol_policy(),
                ProtocolPolicy::ModernOnly
            );
        }

        #[cfg(feature = "legacy-2024-11-05")]
        {
            let set_result = builder.try_set_protocol_policy(ProtocolPolicy::Auto);
            assert!(set_result.is_ok());
        }
    }

    #[test]
    fn invalid_launch_policy_is_rejected_before_builder_construction() {
        let result = ServerBuilder::from_launch_protocol_policy(
            "srv",
            "1.0",
            Err(ServerLaunchPolicyError::InvalidValue),
        );

        assert!(matches!(result, Err(ServerLaunchPolicyError::InvalidValue)));
    }

    #[test]
    fn launch_policy_parser_rejects_unknown_value_without_auto_fallback() {
        assert_eq!(
            protocol_policy_from_server_launch_value(Some(OsStr::new("mcp-2025-11-25"))),
            Err(ServerLaunchPolicyError::InvalidValue)
        );
    }

    #[cfg(unix)]
    #[test]
    fn launch_policy_parser_rejects_non_unicode_value_without_panic() {
        use std::os::unix::ffi::OsStrExt;

        assert_eq!(
            protocol_policy_from_server_launch_value(Some(OsStr::from_bytes(b"modern-only\xff"))),
            Err(ServerLaunchPolicyError::NonUnicode)
        );
    }

    // ── Fluent API setters ───────────────────────────────────────────

    #[test]
    fn builder_request_timeout() {
        let builder = ServerBuilder::new("srv", "1.0").request_timeout(60);
        assert_eq!(builder.request_timeout_secs(), 60);
    }

    #[test]
    fn builder_request_timeout_zero_omits_server_ceiling() {
        let builder = ServerBuilder::new("srv", "1.0").request_timeout(0);
        assert_eq!(builder.request_timeout_secs(), 0);
    }

    #[test]
    fn builder_without_stats() {
        let server = ServerBuilder::new("srv", "1.0").without_stats().build();
        assert!(server.stats().is_none());
    }

    #[test]
    fn builder_mask_error_details() {
        let builder = ServerBuilder::new("srv", "1.0").mask_error_details(true);
        assert!(builder.is_error_masking_enabled());
    }

    #[test]
    fn builder_strict_input_validation() {
        let builder = ServerBuilder::new("srv", "1.0").strict_input_validation(true);
        assert!(builder.is_strict_input_validation_enabled());
    }

    #[test]
    fn builder_instructions() {
        let server = ServerBuilder::new("srv", "1.0")
            .instructions("Use this server wisely")
            .build();
        // instructions stored internally - verify build succeeds
        let _ = server;
    }

    #[test]
    fn builder_log_level() {
        let builder = ServerBuilder::new("srv", "1.0").log_level(Level::Debug);
        assert_eq!(builder.logging.level, LevelFilter::Debug);
        assert_eq!(builder.console_config.log_level, LevelFilter::Debug);
    }

    #[test]
    fn builder_log_level_filter() {
        let builder = ServerBuilder::new("srv", "1.0").log_level_filter(LevelFilter::Warn);
        assert_eq!(builder.logging.level, LevelFilter::Warn);
        assert_eq!(builder.console_config.log_level, LevelFilter::Warn);
    }

    #[test]
    fn builder_log_timestamps_and_targets() {
        let builder = ServerBuilder::new("srv", "1.0")
            .log_timestamps(false)
            .log_targets(false)
            .log_file_line(true);
        assert!(!builder.logging.timestamps);
        assert!(!builder.console_config.log_timestamps);
        assert!(!builder.logging.targets);
        assert!(!builder.console_config.log_targets);
        assert!(builder.logging.file_line);
        assert!(builder.console_config.log_file_line);
    }

    // ── Console configuration ────────────────────────────────────────

    #[test]
    fn builder_without_banner() {
        let builder = ServerBuilder::new("srv", "1.0").without_banner();
        let config = builder.console_config();
        assert_eq!(config.banner_style, BannerStyle::None);
    }

    #[test]
    fn builder_with_banner_compact() {
        let builder = ServerBuilder::new("srv", "1.0").with_banner(BannerStyle::Compact);
        let config = builder.console_config();
        assert_eq!(config.banner_style, BannerStyle::Compact);
    }

    #[test]
    fn builder_plain_mode() {
        let builder = ServerBuilder::new("srv", "1.0").plain_mode();
        let _config = builder.console_config();
    }

    // ── Handler registration ─────────────────────────────────────────

    #[test]
    fn builder_tool_enables_capability() {
        let server = ServerBuilder::new("srv", "1.0").tool(TestTool).build();
        assert!(server.capabilities().tools.is_some());
        assert!(
            server
                .capabilities()
                .tools
                .as_ref()
                .is_some_and(|tools| tools.list_changed),
            "registering a tool must advertise tools.listChanged so clients can watch catalog mutations"
        );
        assert!(server.has_tools());
    }

    #[cfg(feature = "apps")]
    #[test]
    fn builder_mcp_apps_tool_requires_apps_opt_in() {
        let Err(error) = ServerBuilder::new("srv", "1.0").mcp_apps_tool(TestTool) else {
            panic!("Apps tools must not register before Apps negotiation is configured");
        };
        assert_eq!(error.code, fastmcp_core::McpErrorCode::InvalidRequest);
    }

    #[test]
    fn builder_legacy_tool_is_explicit_and_does_not_claim_modern_tools() {
        let server = ServerBuilder::new("srv", "1.0")
            .legacy_tool(ExactLegacyOnlyTool)
            .build();
        assert!(server.capabilities().tools.is_some());
        assert!(server.has_tools());
        let router = server.into_router();
        assert_eq!(router.tools_count(), 1);
        assert!(
            !router
                .server_discovery_behavior_registry()
                .contains(fastmcp_protocol::ServerBehavior::ToolsList)
        );
    }

    #[test]
    fn builder_resource_enables_capability() {
        let server = ServerBuilder::new("srv", "1.0")
            .resource(TestResource)
            .build();
        assert!(server.capabilities().resources.is_some());
        assert!(
            server
                .capabilities()
                .resources
                .as_ref()
                .is_some_and(|resources| resources.subscribe && resources.list_changed),
            "registering a resource must advertise subscribe and listChanged"
        );
        assert!(server.has_resources());
    }

    #[test]
    fn builder_prompt_enables_capability() {
        let server = ServerBuilder::new("srv", "1.0").prompt(TestPrompt).build();
        assert!(server.capabilities().prompts.is_some());
        assert!(
            server
                .capabilities()
                .prompts
                .as_ref()
                .is_some_and(|prompts| prompts.list_changed),
            "registering a prompt must advertise prompts.listChanged"
        );
        assert!(server.has_prompts());
    }

    #[test]
    fn builder_all_handlers() {
        let server = ServerBuilder::new("srv", "1.0")
            .tool(TestTool)
            .resource(TestResource)
            .prompt(TestPrompt)
            .build();
        assert!(server.has_tools());
        assert!(server.has_resources());
        assert!(server.has_prompts());
    }

    #[test]
    fn builder_no_handlers_means_no_capabilities() {
        let server = ServerBuilder::new("srv", "1.0").build();
        assert!(!server.has_tools());
        assert!(!server.has_resources());
        assert!(!server.has_prompts());
    }

    // ── Duplicate behavior ───────────────────────────────────────────

    #[test]
    fn builder_on_duplicate_default_is_warn() {
        let _builder = ServerBuilder::new("srv", "1.0");
        // DuplicateBehavior::default() is Warn - builder should use that
    }

    #[test]
    fn builder_on_duplicate_ignore() {
        let server = ServerBuilder::new("srv", "1.0")
            .on_duplicate(DuplicateBehavior::Ignore)
            .tool(TestTool)
            .build();
        assert!(server.has_tools());
    }

    #[test]
    fn builder_on_duplicate_replace() {
        let server = ServerBuilder::new("srv", "1.0")
            .on_duplicate(DuplicateBehavior::Replace)
            .tool(TestTool)
            .build();
        assert!(server.has_tools());
    }

    #[test]
    fn builder_resource_template_honors_duplicate_policy() {
        for behavior in [
            DuplicateBehavior::Warn,
            DuplicateBehavior::Ignore,
            DuplicateBehavior::Error,
        ] {
            let server = ServerBuilder::new("srv", "1.0")
                .on_duplicate(behavior)
                .resource_template(marked_resource_template("original"))
                .resource_template(marked_resource_template("incoming"))
                .build();
            assert_eq!(server.resource_templates().len(), 1);
            assert_eq!(server.resource_templates()[0].name, "original");
        }

        let server = ServerBuilder::new("srv", "1.0")
            .on_duplicate(DuplicateBehavior::Replace)
            .resource_template(marked_resource_template("original"))
            .resource_template(marked_resource_template("incoming"))
            .build();
        assert_eq!(server.resource_templates().len(), 1);
        assert_eq!(server.resource_templates()[0].name, "incoming");
    }

    // ── Lifecycle hooks ──────────────────────────────────────────────

    #[test]
    fn builder_on_startup_builds() {
        let server = ServerBuilder::new("srv", "1.0")
            .on_startup(|| -> Result<(), std::io::Error> { Ok(()) })
            .build();
        let _ = server;
    }

    #[test]
    fn builder_on_shutdown_builds() {
        let server = ServerBuilder::new("srv", "1.0").on_shutdown(|| {}).build();
        let _ = server;
    }

    // ── Console config on built server ───────────────────────────────

    #[test]
    fn built_server_console_config_matches_builder() {
        let server = ServerBuilder::new("srv", "1.0").without_banner().build();
        assert_eq!(server.console_config().banner_style, BannerStyle::None);
    }

    // ── Chaining ─────────────────────────────────────────────────────

    #[test]
    fn builder_chaining_fluent_api() {
        let server = ServerBuilder::new("chain", "3.0")
            .request_timeout(120)
            .mask_error_details(true)
            .strict_input_validation(true)
            .without_banner()
            .plain_mode()
            .tool(TestTool)
            .resource(TestResource)
            .prompt(TestPrompt)
            .on_shutdown(|| {})
            .build();

        assert_eq!(server.info().name, "chain");
        assert_eq!(server.info().version, "3.0");
        assert!(server.has_tools());
        assert!(server.has_resources());
        assert!(server.has_prompts());
    }

    // ── Console configuration extended ─────────────────────────────

    #[test]
    fn builder_with_console_config() {
        let mut config = ConsoleConfig::new().with_banner(BannerStyle::None);
        config.log_level = LevelFilter::Trace;
        config.log_timestamps = false;
        config.log_targets = false;
        config.log_file_line = true;
        let builder = ServerBuilder::new("srv", "1.0").with_console_config(config);
        assert_eq!(builder.console_config().banner_style, BannerStyle::None);
        assert_eq!(builder.logging.level, LevelFilter::Trace);
        assert!(!builder.logging.timestamps);
        assert!(!builder.logging.targets);
        assert!(builder.logging.file_line);
    }

    #[test]
    fn logging_and_console_setters_keep_one_effective_configuration() {
        let console = ConsoleConfig::new().with_log_level_filter(LevelFilter::Off);
        let builder = ServerBuilder::new("srv", "1.0")
            .log_level(Level::Debug)
            .with_console_config(console);
        assert_eq!(builder.logging.level, LevelFilter::Off);
        assert_eq!(builder.console_config.log_level, LevelFilter::Off);

        let builder = builder.log_level_filter(LevelFilter::Warn);
        assert_eq!(builder.logging.level, LevelFilter::Warn);
        assert_eq!(builder.console_config.log_level, LevelFilter::Warn);
    }

    #[test]
    fn builder_with_traffic_logging() {
        let builder = ServerBuilder::new("srv", "1.0").with_traffic_logging(TrafficVerbosity::Full);
        let config = builder.console_config();
        assert_eq!(config.traffic_verbosity, TrafficVerbosity::Full);
    }

    #[test]
    fn builder_force_color() {
        let builder = ServerBuilder::new("srv", "1.0").force_color();
        let _config = builder.console_config();
        // Just verify the chain completes without panic
    }

    // ── Logging config ─────────────────────────────────────────────

    #[test]
    fn builder_logging_full_config() {
        let config = LoggingConfig {
            level: LevelFilter::Trace,
            timestamps: false,
            targets: false,
            file_line: true,
        };
        let _builder = ServerBuilder::new("srv", "1.0").logging(config);
    }

    // ── List page size ─────────────────────────────────────────────

    #[test]
    fn builder_list_page_size() {
        let server = ServerBuilder::new("srv", "1.0")
            .list_page_size(50)
            .tool(TestTool)
            .build();
        assert!(server.has_tools());
    }

    // ── Resource template ──────────────────────────────────────────

    #[test]
    fn builder_resource_template_enables_capability() {
        let template = ResourceTemplate {
            uri_template: "file://{path}".to_string(),
            name: "Template".to_string(),
            description: None,
            mime_type: None,
            icon: None,
            version: None,
            tags: vec![],
        };
        let server = ServerBuilder::new("srv", "1.0")
            .resource_template(template)
            .build();
        assert!(server.capabilities().resources.is_some());
    }

    // ── Middleware ──────────────────────────────────────────────────

    struct NoopMiddleware;
    impl crate::Middleware for NoopMiddleware {}

    #[test]
    fn builder_middleware() {
        let server = ServerBuilder::new("srv", "1.0")
            .middleware(NoopMiddleware)
            .build();
        let _ = server;
    }

    #[test]
    fn builder_multiple_middleware() {
        let server = ServerBuilder::new("srv", "1.0")
            .middleware(NoopMiddleware)
            .middleware(NoopMiddleware)
            .build();
        let _ = server;
    }

    // ── Auth provider ──────────────────────────────────────────────

    struct TestAuthProvider;
    impl crate::AuthProvider for TestAuthProvider {
        fn authenticate(
            &self,
            _ctx: &McpContext,
            _request: crate::auth::AuthRequest<'_>,
        ) -> McpResult<fastmcp_core::AuthContext> {
            Ok(fastmcp_core::AuthContext::with_subject("test-user"))
        }
    }

    #[test]
    fn builder_auth_provider() {
        let server = ServerBuilder::new("srv", "1.0")
            .auth_provider(TestAuthProvider)
            .build();
        let _ = server;
    }

    // ── auto_mask_errors ───────────────────────────────────────────

    #[test]
    fn builder_auto_mask_errors() {
        // In debug builds (test mode), auto_mask_errors defaults to false
        let builder = ServerBuilder::new("srv", "1.0").auto_mask_errors();
        // In debug_assertions mode, masking should be disabled
        assert!(!builder.is_error_masking_enabled());
    }

    // ── Duplicate behavior error ───────────────────────────────────

    struct DupTool(&'static str);
    impl crate::ToolHandler for DupTool {
        fn definition(&self) -> Tool {
            Tool {
                name: self.0.to_string(),
                description: None,
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: None,
                icon: None,
                version: None,
                tags: vec![],
                annotations: None,
            }
        }
        fn call(&self, _ctx: &McpContext, _args: serde_json::Value) -> McpResult<Vec<Content>> {
            Ok(vec![Content::text("ok")])
        }
    }

    /// The refusals a builder must fail with; building must not succeed.
    fn refused_registrations(builder: ServerBuilder) -> Vec<RefusedRegistration> {
        match builder.try_build() {
            Ok(_) => panic!("a refused registration must fail try_build"),
            Err(ServerBuildError::InvalidConfiguration(refused)) => refused,
        }
    }

    #[test]
    fn builder_on_duplicate_error_refuses_the_build() {
        let refused = refused_registrations(
            ServerBuilder::new("srv", "1.0")
                .on_duplicate(DuplicateBehavior::Error)
                .tool(DupTool("dup"))
                .tool(DupTool("dup")),
        );
        assert_eq!(refused.len(), 1);
        assert_eq!(
            (refused[0].kind, refused[0].name.as_str()),
            (RegistrationKind::Tool, "dup")
        );
    }

    struct AnnotatedTool(serde_json::Value);
    impl crate::ToolHandler for AnnotatedTool {
        fn definition(&self) -> Tool {
            Tool {
                name: "lookup".to_string(),
                description: None,
                input_schema: self.0.clone(),
                output_schema: None,
                icon: None,
                version: None,
                tags: vec![],
                annotations: None,
            }
        }
        fn call(&self, _ctx: &McpContext, _args: serde_json::Value) -> McpResult<Vec<Content>> {
            Ok(vec![Content::text("ok")])
        }
    }

    /// A `region` property mirrored as `Mcp-Param-Region`; only its type varies.
    fn region_schema(region_type: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {
            "region": {"type": region_type, "x-mcp-header": "Region"}
        }})
    }

    #[test]
    fn builder_admits_a_valid_annotated_tool_into_the_catalog() {
        let server = ServerBuilder::new("srv", "1.0")
            .tool(AnnotatedTool(region_schema(serde_json::json!("string"))))
            .try_build()
            .expect("a valid annotated tool must build");
        let tools = server.tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "lookup");
        assert_eq!(
            tools[0].input_schema["properties"]["region"]["x-mcp-header"],
            "Region"
        );
    }

    #[test]
    fn builder_refuses_an_invalid_annotation_naming_the_tool_and_reason() {
        // Identical to the positive except the annotated type is nullable.
        let refused = refused_registrations(ServerBuilder::new("srv", "1.0").tool(AnnotatedTool(
            region_schema(serde_json::json!(["string", "null"])),
        )));
        assert_eq!(refused.len(), 1);
        assert_eq!(
            (refused[0].kind, refused[0].name.as_str()),
            (RegistrationKind::Tool, "lookup")
        );
        assert_eq!(
            refused[0].reason,
            "tool declares an invalid final input schema"
        );
    }

    #[test]
    #[should_panic(expected = "tool \"lookup\": tool declares an invalid final input schema")]
    fn builder_build_panics_instead_of_dropping_a_refused_tool() {
        let _ = ServerBuilder::new("srv", "1.0")
            .tool(AnnotatedTool(region_schema(serde_json::json!([
                "string", "null"
            ]))))
            .build();
    }

    // ── Mount ──────────────────────────────────────────────────────

    #[test]
    fn builder_mount_with_prefix() {
        let source = ServerBuilder::new("sub", "1.0")
            .tool(TestTool)
            .resource(TestResource)
            .prompt(TestPrompt)
            .build();

        let main = ServerBuilder::new("main", "1.0")
            .mount(source, Some("sub"))
            .build();

        assert!(main.has_tools());
        assert!(main.has_resources());
        assert!(main.has_prompts());
    }

    #[test]
    fn builder_mount_without_prefix() {
        let source = ServerBuilder::new("sub", "1.0").tool(TestTool).build();

        let main = ServerBuilder::new("main", "1.0")
            .mount(source, None)
            .build();

        assert!(main.has_tools());
    }

    #[cfg(feature = "apps")]
    #[test]
    fn builder_mount_refuses_the_build_for_apps_bound_child_without_apps_opt_in() {
        let refused = refused_registrations(
            ServerBuilder::new("main", "1.0")
                .tool(TestTool)
                .resource(TestResource)
                .mount(apps_mount_child(), None),
        );
        assert_single_mount_refusal(&refused, "", "MCP Apps");
    }

    /// Exactly one refused mount, named by its prefix, for the given cause.
    fn assert_single_mount_refusal(refused: &[RefusedRegistration], prefix: &str, cause: &str) {
        assert_eq!(refused.len(), 1, "{refused:?}");
        assert_eq!(refused[0].kind, RegistrationKind::Mount);
        assert_eq!(refused[0].name, prefix);
        assert!(refused[0].reason.contains(cause), "{refused:?}");
    }

    #[cfg(feature = "apps")]
    #[test]
    fn builder_mount_accepts_apps_bound_child_with_active_apps_opt_in() {
        let main = ServerBuilder::new("main", "1.0")
            .mcp_apps()
            .expect("parent Apps extension installs")
            .mount(apps_mount_child(), None)
            .build();

        assert!(main.has_tools());
        assert!(main.has_resources());
        assert_eq!(
            main.extension_registry_receipt()
                .expect("parent retains its Apps extension runtime")
                .descriptor_count(),
            1 + usize::from(cfg!(feature = "tasks")),
            "mounting does not transfer or merge the child extension runtime"
        );

        let router = main.into_router();
        assert_eq!(router.tools_count(), 1);
        assert_eq!(router.resources_count(), 1);
    }

    #[cfg(feature = "apps")]
    #[test]
    fn builder_mount_tools_refuses_the_build_for_apps_bound_child_without_apps_opt_in() {
        let refused = refused_registrations(
            ServerBuilder::new("main", "1.0")
                .resource(TestResource)
                .mount_tools(apps_resource_bound_tool_mount_child(), None),
        );
        assert_single_mount_refusal(&refused, "", "MCP Apps");
    }

    #[cfg(feature = "apps")]
    #[test]
    fn builder_mount_tools_accepts_apps_bound_child_with_active_apps_opt_in() {
        let resource = McpAppsUiResource::try_new(
            AbsoluteUri::parse("ui://mount/dashboard").expect("fixed Apps mount URI is valid"),
            "mounted-apps-dashboard",
            "<main>Apps mount fixture</main>",
        )
        .expect("fixed Apps mount resource is valid");
        let main = ServerBuilder::new("main", "1.0")
            .mcp_apps()
            .expect("parent Apps extension installs")
            .mcp_apps_ui_resource(resource)
            .expect("parent Apps resource registers")
            .mount_tools(apps_mount_child(), None)
            .build();

        assert!(main.has_tools());
        assert!(main.has_resources());
        assert_eq!(
            main.extension_registry_receipt()
                .expect("parent retains its Apps extension runtime")
                .descriptor_count(),
            1 + usize::from(cfg!(feature = "tasks")),
            "mounting does not transfer or merge the child extension runtime"
        );

        let router = main.into_router();
        assert_eq!(router.tools_count(), 1);
        assert_eq!(router.resources_count(), 1);
    }

    #[cfg(feature = "apps")]
    #[test]
    fn builder_mount_resources_refuses_the_build_for_apps_bound_child_without_apps_opt_in() {
        let refused = refused_registrations(
            ServerBuilder::new("main", "1.0")
                .tool(TestTool)
                .mount_resources(apps_mount_child(), None),
        );
        assert_single_mount_refusal(&refused, "", "MCP Apps");
    }

    #[cfg(feature = "apps")]
    #[test]
    fn builder_mount_resources_accepts_apps_bound_child_with_active_apps_opt_in() {
        let main = ServerBuilder::new("main", "1.0")
            .mcp_apps()
            .expect("parent Apps extension installs")
            .mount_resources(apps_mount_child(), None)
            .build();

        assert!(!main.has_tools());
        assert!(main.has_resources());
        assert_eq!(
            main.extension_registry_receipt()
                .expect("parent retains its Apps extension runtime")
                .descriptor_count(),
            1 + usize::from(cfg!(feature = "tasks")),
            "mounting does not transfer or merge the child extension runtime"
        );

        let router = main.into_router();
        assert_eq!(router.tools_count(), 0);
        assert_eq!(router.resources_count(), 1);
    }

    #[test]
    fn builder_mount_tools_only() {
        let source = ServerBuilder::new("sub", "1.0")
            .tool(TestTool)
            .resource(TestResource)
            .prompt(TestPrompt)
            .build();

        let main = ServerBuilder::new("main", "1.0")
            .mount_tools(source, Some("sub"))
            .build();

        assert!(main.has_tools());
        // Resources and prompts should NOT be mounted
        assert!(!main.has_resources());
        assert!(!main.has_prompts());
    }

    #[test]
    fn builder_mount_resources_only() {
        let source = ServerBuilder::new("sub", "1.0")
            .tool(TestTool)
            .resource(TestResource)
            .prompt(TestPrompt)
            .build();

        let main = ServerBuilder::new("main", "1.0")
            .mount_resources(source, Some("data"))
            .build();

        assert!(!main.has_tools());
        assert!(main.has_resources());
        assert!(!main.has_prompts());
    }

    #[test]
    fn builder_mount_prompts_only() {
        let source = ServerBuilder::new("sub", "1.0")
            .tool(TestTool)
            .resource(TestResource)
            .prompt(TestPrompt)
            .build();

        let main = ServerBuilder::new("main", "1.0")
            .mount_prompts(source, Some("tmpl"))
            .build();

        assert!(!main.has_tools());
        assert!(!main.has_resources());
        assert!(main.has_prompts());
    }

    #[test]
    fn builder_mount_empty_server() {
        let source = ServerBuilder::new("empty", "1.0").build();

        let main = ServerBuilder::new("main", "1.0")
            .mount(source, Some("empty"))
            .build();

        assert!(!main.has_tools());
        assert!(!main.has_resources());
        assert!(!main.has_prompts());
    }

    #[test]
    fn builder_full_mount_honors_duplicate_policy_for_all_component_kinds() {
        assert_mount_duplicates_refused(refused_registrations(
            marked_builder("original")
                .on_duplicate(DuplicateBehavior::Error)
                .mount(marked_builder("incoming").build(), None),
        ));
        for behavior in [
            DuplicateBehavior::Warn,
            DuplicateBehavior::Ignore,
            DuplicateBehavior::Replace,
        ] {
            let server = marked_builder("original")
                .on_duplicate(behavior)
                .mount(marked_builder("incoming").build(), None)
                .build();
            assert_marked_server(
                &server,
                if behavior == DuplicateBehavior::Replace {
                    "incoming"
                } else {
                    "original"
                },
            );
        }
    }

    /// Under Error a colliding mount is refused, so the build fails.
    fn assert_mount_duplicates_refused(refused: Vec<RefusedRegistration>) {
        assert!(!refused.is_empty());
        assert!(
            refused
                .iter()
                .all(|registration| registration.kind == RegistrationKind::Mount
                    && registration.reason.contains("already exists")),
            "{refused:?}"
        );
    }

    #[test]
    fn builder_partial_mounts_honor_duplicate_policy() {
        let error = || marked_builder("original").on_duplicate(DuplicateBehavior::Error);
        assert_mount_duplicates_refused(refused_registrations(
            error().mount_tools(marked_builder("incoming").build(), None),
        ));
        assert_mount_duplicates_refused(refused_registrations(
            error().mount_resources(marked_builder("incoming").build(), None),
        ));
        assert_mount_duplicates_refused(refused_registrations(
            error().mount_prompts(marked_builder("incoming").build(), None),
        ));
        for behavior in [
            DuplicateBehavior::Warn,
            DuplicateBehavior::Ignore,
            DuplicateBehavior::Replace,
        ] {
            let replacement_marker = if behavior == DuplicateBehavior::Replace {
                "incoming"
            } else {
                "original"
            };

            let tools = marked_builder("original")
                .on_duplicate(behavior)
                .mount_tools(marked_builder("incoming").build(), None)
                .build();
            assert_eq!(tools.tools()[0].tags, vec![replacement_marker.to_string()]);
            assert_eq!(tools.resources()[0].tags, vec!["original".to_string()]);
            assert_eq!(
                tools.resource_templates()[0].tags,
                vec!["original".to_string()]
            );
            assert_eq!(tools.prompts()[0].tags, vec!["original".to_string()]);

            let resources = marked_builder("original")
                .on_duplicate(behavior)
                .mount_resources(marked_builder("incoming").build(), None)
                .build();
            assert_eq!(resources.tools()[0].tags, vec!["original".to_string()]);
            assert_eq!(
                resources.resources()[0].tags,
                vec![replacement_marker.to_string()]
            );
            assert_eq!(
                resources.resource_templates()[0].tags,
                vec![replacement_marker.to_string()]
            );
            assert_eq!(resources.prompts()[0].tags, vec!["original".to_string()]);

            let prompts = marked_builder("original")
                .on_duplicate(behavior)
                .mount_prompts(marked_builder("incoming").build(), None)
                .build();
            assert_eq!(prompts.tools()[0].tags, vec!["original".to_string()]);
            assert_eq!(prompts.resources()[0].tags, vec!["original".to_string()]);
            assert_eq!(
                prompts.resource_templates()[0].tags,
                vec!["original".to_string()]
            );
            assert_eq!(
                prompts.prompts()[0].tags,
                vec![replacement_marker.to_string()]
            );
        }
    }

    #[test]
    fn builder_full_and_partial_mounts_refuse_the_build_for_invalid_prefixes() {
        let replace = || marked_builder("original").on_duplicate(DuplicateBehavior::Replace);
        let incoming = || marked_builder("incoming").build();
        for refused in [
            refused_registrations(replace().mount(incoming(), Some("peer/secret"))),
            refused_registrations(replace().mount_tools(incoming(), Some("peer/secret"))),
            refused_registrations(replace().mount_resources(incoming(), Some("peer/secret"))),
            refused_registrations(replace().mount_prompts(incoming(), Some("peer/secret"))),
        ] {
            assert_single_mount_refusal(&refused, "peer/secret", "Invalid mount prefix");
        }
    }

    #[cfg(feature = "proxy")]
    mod proxy_registration_tests {
        use super::*;

        // ── Proxy registration ─────────────────────────────────────────

        struct CompletionProxyBackend {
            supported: bool,
            result: CoreResult,
            calls: Arc<Mutex<Vec<fastmcp_client::CompletionParams>>>,
        }

        struct LocalFinalCompletionPrompt;

        impl crate::PromptHandler for LocalFinalCompletionPrompt {
            fn definition(&self) -> Prompt {
                Prompt {
                    name: "final-deploy".to_owned(),
                    description: Some("local completion target".to_owned()),
                    arguments: vec![fastmcp_protocol::PromptArgument {
                        name: "environment".to_owned(),
                        description: None,
                        required: false,
                    }],
                    icon: None,
                    version: None,
                    tags: Vec::new(),
                }
            }

            fn final_definition(&self) -> Option<fastmcp_protocol::FinalPrompt> {
                Some(fastmcp_protocol::FinalPrompt {
                    name: "final-deploy".to_owned(),
                    title: Some("Local Final Deploy".to_owned()),
                    description: Some("local completion target".to_owned()),
                    icons: None,
                    arguments: Some(vec![fastmcp_protocol::FinalPromptArgument {
                        name: "environment".to_owned(),
                        title: Some("Local Environment".to_owned()),
                        description: None,
                        required: Some(false),
                    }]),
                    meta: None,
                })
            }

            fn get(
                &self,
                _ctx: &McpContext,
                _args: std::collections::HashMap<String, String>,
            ) -> McpResult<Vec<fastmcp_protocol::PromptMessage>> {
                Ok(Vec::new())
            }
        }

        struct LocalLegacyCompletionPrompt;

        impl crate::PromptHandler for LocalLegacyCompletionPrompt {
            fn definition(&self) -> Prompt {
                Prompt {
                    name: "legacy-deploy".to_owned(),
                    description: Some("local legacy completion target".to_owned()),
                    arguments: vec![fastmcp_protocol::PromptArgument {
                        name: "environment".to_owned(),
                        description: None,
                        required: false,
                    }],
                    icon: None,
                    version: None,
                    tags: Vec::new(),
                }
            }

            fn get(
                &self,
                _ctx: &McpContext,
                _args: std::collections::HashMap<String, String>,
            ) -> McpResult<Vec<fastmcp_protocol::PromptMessage>> {
                Ok(Vec::new())
            }
        }

        impl crate::proxy::ProxyBackend for CompletionProxyBackend {
            fn list_tools(&mut self) -> McpResult<Vec<Tool>> {
                Ok(Vec::new())
            }

            fn list_resources(&mut self) -> McpResult<Vec<Resource>> {
                Ok(Vec::new())
            }

            fn list_resource_templates(&mut self) -> McpResult<Vec<ResourceTemplate>> {
                Ok(Vec::new())
            }

            fn list_prompts(&mut self) -> McpResult<Vec<Prompt>> {
                Ok(Vec::new())
            }

            fn call_tool(&mut self, _: &str, _: serde_json::Value) -> McpResult<Vec<Content>> {
                Ok(Vec::new())
            }

            fn call_tool_with_progress(
                &mut self,
                _: &str,
                _: serde_json::Value,
                _: crate::proxy::ProgressCallback<'_>,
            ) -> McpResult<Vec<Content>> {
                Ok(Vec::new())
            }

            fn read_resource(&mut self, _: &str) -> McpResult<Vec<ResourceContent>> {
                Ok(Vec::new())
            }

            fn get_prompt(
                &mut self,
                _: &str,
                _: std::collections::HashMap<String, String>,
            ) -> McpResult<Vec<fastmcp_protocol::PromptMessage>> {
                Ok(Vec::new())
            }

            fn supports_completion(&mut self) -> McpResult<bool> {
                Ok(self.supported)
            }

            fn complete_result(
                &mut self,
                params: fastmcp_client::CompletionParams,
            ) -> McpResult<CoreResult> {
                self.calls
                    .lock()
                    .expect("completion proxy call log is not poisoned")
                    .push(params);
                Ok(self.result.clone())
            }
        }

        fn legacy_completion_proxy_catalog() -> ProxyTypedCatalog {
            ProxyTypedCatalog {
                tools: ProxyToolCatalog::Legacy(Vec::new()),
                resources: ProxyResourceCatalog::Legacy(Vec::new()),
                resource_templates: ProxyResourceTemplateCatalog::Legacy(Vec::new()),
                prompts: ProxyPromptCatalog::Legacy(vec![Prompt {
                    name: "legacy-deploy".to_owned(),
                    description: None,
                    arguments: Vec::new(),
                    icon: None,
                    version: None,
                    tags: Vec::new(),
                }]),
            }
        }

        fn final_completion_proxy_catalog() -> ProxyTypedCatalog {
            let prompt = serde_json::from_value(serde_json::json!({
                "name": "final-deploy",
                "title": "Upstream Final Deploy",
                "arguments": [{"name": "environment", "title": "Environment"}]
            }))
            .expect("the final completion prompt fixture is valid");
            ProxyTypedCatalog {
                tools: ProxyToolCatalog::Final(ProxyFinalCatalog::new(Vec::new())),
                resources: ProxyResourceCatalog::Final(ProxyFinalCatalog::new(Vec::new())),
                resource_templates: ProxyResourceTemplateCatalog::Final(ProxyFinalCatalog::new(
                    Vec::new(),
                )),
                prompts: ProxyPromptCatalog::Final(ProxyFinalCatalog::new(vec![prompt])),
            }
        }

        fn final_completion_proxy_template_catalog() -> ProxyTypedCatalog {
            let template = serde_json::from_value(serde_json::json!({
                "uriTemplate": "completion://{environment}",
                "name": "upstream-completion-template",
                "title": "Upstream Completion Template",
            }))
            .expect("the final completion resource-template fixture is valid");
            ProxyTypedCatalog {
                tools: ProxyToolCatalog::Final(ProxyFinalCatalog::new(Vec::new())),
                resources: ProxyResourceCatalog::Final(ProxyFinalCatalog::new(Vec::new())),
                resource_templates: ProxyResourceTemplateCatalog::Final(ProxyFinalCatalog::new(
                    vec![template],
                )),
                prompts: ProxyPromptCatalog::Final(ProxyFinalCatalog::new(Vec::new())),
            }
        }

        fn legacy_completion_proxy_template_catalog() -> ProxyTypedCatalog {
            ProxyTypedCatalog {
                tools: ProxyToolCatalog::Legacy(Vec::new()),
                resources: ProxyResourceCatalog::Legacy(Vec::new()),
                resource_templates: ProxyResourceTemplateCatalog::Legacy(vec![ResourceTemplate {
                    uri_template: "completion://{environment}".to_owned(),
                    name: "upstream-completion-template".to_owned(),
                    description: None,
                    mime_type: None,
                    icon: None,
                    version: None,
                    tags: Vec::new(),
                }]),
                prompts: ProxyPromptCatalog::Legacy(Vec::new()),
            }
        }

        fn local_completion_template() -> ResourceTemplate {
            ResourceTemplate {
                uri_template: "completion://{environment}".to_owned(),
                name: "local-completion-template".to_owned(),
                description: Some("local completion target".to_owned()),
                mime_type: None,
                icon: None,
                version: None,
                tags: Vec::new(),
            }
        }

        fn legacy_completion_proxy_result() -> CoreResult {
            CoreResult::Legacy(LegacyCoreResult::Completion(
                fastmcp_protocol::LegacyCompletionResult {
                    completion: fastmcp_protocol::CompletionValues {
                        values: vec!["legacy-staging".to_owned()],
                        total: Some(1),
                        has_more: Some(false),
                    },
                    meta: None,
                },
            ))
        }

        fn final_completion_proxy_result() -> CoreResult {
            CoreResult::Final(FinalCoreResult::Completion {
                result: CompleteResult::new(
                    fastmcp_protocol::FinalCompletionResult {
                        completion: fastmcp_protocol::FinalCompletionValues {
                            values: vec!["final-staging".to_owned()],
                            total: Some(
                                serde_json::from_str("92233720368547758081234567890")
                                    .expect("the fixed exact completion total is a JSON integer"),
                            ),
                            has_more: Some(false),
                        },
                    },
                    ResultMeta::server_generated(
                        Implementation::try_new("completion-upstream", "1.0")
                            .expect("the fixed completion implementation is valid"),
                    ),
                ),
                diagnostic: None,
            })
        }

        fn final_completion_request(id: i64) -> JsonRpcRequest {
            JsonRpcRequest::new(
                "completion/complete",
                Some(serde_json::json!({
                    "_meta": {
                        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                        "io.modelcontextprotocol/clientCapabilities": {},
                    },
                    "ref": {
                        "type": "ref/prompt",
                        "name": "final-deploy",
                        "title": "Final Deploy",
                    },
                    "argument": {"name": "environment", "value": "sta"},
                    "context": {"arguments": {"region": "us-east-1"}},
                })),
                id,
            )
        }

        fn final_resource_template_completion_request(id: i64) -> JsonRpcRequest {
            JsonRpcRequest::new(
                "completion/complete",
                Some(serde_json::json!({
                    "_meta": {
                        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                        "io.modelcontextprotocol/clientCapabilities": {},
                    },
                    "ref": {
                        "type": "ref/resource",
                        "uri": "completion://{environment}",
                    },
                    "argument": {"name": "environment", "value": "sta"},
                })),
                id,
            )
        }

        fn legacy_completion_request(id: i64, reference: serde_json::Value) -> JsonRpcRequest {
            JsonRpcRequest::new(
                "completion/complete",
                Some(serde_json::json!({
                    "ref": reference,
                    "argument": {"name": "environment", "value": "sta"},
                })),
                id,
            )
        }

        fn final_proxy_catalog() -> ProxyCatalog {
            ProxyCatalog {
                tool_catalog_era: Some(ProtocolEra::Modern2026),
                final_tools: vec![
                    serde_json::from_value(serde_json::json!({
                        "name": "weather",
                        "title": "Weather Forecast",
                        "description": "Returns a precise forecast.",
                        "icons": [{
                            "src": "https://example.test/icons/weather.svg",
                            "mimeType": "image/svg+xml",
                            "sizes": ["16x16", "32x32"],
                            "theme": "light",
                            "com.example/icon": {"retained": true}
                        }],
                        "inputSchema": {
                            "type": "object",
                            "properties": {"city": {"type": "string"}}
                        },
                        "outputSchema": {"type": "object"},
                        "annotations": {
                            "title": "Forecast",
                            "destructiveHint": false,
                            "idempotentHint": true,
                            "readOnlyHint": true,
                            "openWorldHint": false
                        },
                        "_meta": {"com.example/catalog": {"retained": true}}
                    }))
                    .expect("the exact final tool fixture is valid"),
                ],
                ..ProxyCatalog::default()
            }
        }

        fn legacy_proxy_catalog() -> ProxyCatalog {
            ProxyCatalog {
                tool_catalog_era: Some(ProtocolEra::Legacy2024),
                tools: vec![Tool {
                    name: "legacy-weather".to_owned(),
                    description: Some("Exact legacy proxy fixture".to_owned()),
                    input_schema: serde_json::json!({"type": "object"}),
                    output_schema: None,
                    icon: None,
                    version: None,
                    tags: Vec::new(),
                    annotations: None,
                }],
                ..ProxyCatalog::default()
            }
        }

        fn final_tools_list_request(id: i64) -> JsonRpcRequest {
            JsonRpcRequest::new(
                "tools/list",
                Some(serde_json::json!({
                    "_meta": {
                        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                        "io.modelcontextprotocol/clientCapabilities": {},
                    },
                })),
                id,
            )
        }

        fn final_tools_call_request(
            name: &str,
            arguments: serde_json::Value,
            id: i64,
        ) -> JsonRpcRequest {
            JsonRpcRequest::new(
                "tools/call",
                Some(serde_json::json!({
                    "_meta": {
                        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                        "io.modelcontextprotocol/clientCapabilities": {},
                    },
                    "name": name,
                    "arguments": arguments,
                })),
                id,
            )
        }

        fn bound_proxy_client<B: crate::proxy::ProxyBackend + 'static>(
            backend: B,
            era: ProtocolEra,
        ) -> ProxyClient {
            let mut bindings = ProxyClient::upstream_binding_registry();
            let policy = match era {
                ProtocolEra::Modern2026 => ProtocolPolicy::ModernOnly,
                ProtocolEra::Legacy2024 => ProtocolPolicy::LegacyOnly,
            };
            let opening = match era {
                ProtocolEra::Modern2026 => StdioOpeningFrame::ModernRequest {
                    protocol_version: era.version().as_str().to_owned(),
                },
                ProtocolEra::Legacy2024 => StdioOpeningFrame::LegacyInitialize,
            };
            let binding = bindings
                .bind_stdio(
                    "weather-route",
                    "stdio:weather",
                    "final-catalog-receipt",
                    1,
                    policy,
                    opening,
                )
                .expect("the route selects the requested exact era");
            let upstream_protocol_version = era.version().as_str().to_owned();
            ProxyClient::from_backend_with_upstream_binding(
                backend,
                binding,
                &upstream_protocol_version,
            )
            .expect("the selected upstream version matches its immutable binding")
        }

        fn final_catalog_proxy_client(era: ProtocolEra) -> ProxyClient {
            bound_proxy_client(DuplicatePolicyProxyBackend, era)
        }

        #[cfg(feature = "tasks")]
        fn ordinary_proxy_tasks_client(
            calls: Arc<Mutex<Vec<String>>>,
            updates: Arc<Mutex<Vec<serde_json::Value>>>,
        ) -> ProxyClient {
            let task = serde_json::from_value(serde_json::json!({
                "resultType": "task",
                "taskId": "ordinary-proxy-task-71",
                "status": "input_required",
                "createdAt": "2026-07-28T12:00:00Z",
                "lastUpdatedAt": "2026-07-28T12:00:00Z",
                "ttlMs": null,
                "inputRequests": {},
            }))
            .expect("the ordinary proxy Task fixture is exact");
            bound_proxy_client(
                OrdinaryProxyTasksBackend {
                    calls,
                    updates,
                    task,
                },
                ProtocolEra::Modern2026,
            )
        }

        fn discovered_duplicate_policy_proxy() -> (ProxyClient, ProxyCatalog) {
            let client = ProxyClient::from_backend(DuplicatePolicyProxyBackend);
            let catalog = client
                .catalog()
                .expect("backend discovery supplies the legacy era evidence");
            (client, catalog)
        }

        #[derive(Clone, Copy)]
        enum DualEraProxyRoute {
            Legacy,
            Final,
        }

        struct RecordingDualEraProxyBackend {
            route: DualEraProxyRoute,
            calls: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
        }

        impl RecordingDualEraProxyBackend {
            fn record(&self, name: &str, arguments: serde_json::Value) {
                self.calls
                    .lock()
                    .expect("the test call log lock is not poisoned")
                    .push((name.to_owned(), arguments));
            }
        }

        impl crate::proxy::ProxyBackend for RecordingDualEraProxyBackend {
            fn list_tools(&mut self) -> McpResult<Vec<Tool>> {
                Ok(Vec::new())
            }

            fn list_resources(&mut self) -> McpResult<Vec<Resource>> {
                Ok(Vec::new())
            }

            fn list_resource_templates(&mut self) -> McpResult<Vec<ResourceTemplate>> {
                Ok(Vec::new())
            }

            fn list_prompts(&mut self) -> McpResult<Vec<Prompt>> {
                Ok(Vec::new())
            }

            fn call_tool(
                &mut self,
                name: &str,
                arguments: serde_json::Value,
            ) -> McpResult<Vec<Content>> {
                self.record(name, arguments);
                Ok(vec![Content::text(match self.route {
                    DualEraProxyRoute::Legacy => "bound legacy proxy",
                    DualEraProxyRoute::Final => "bound final proxy",
                })])
            }

            fn call_tool_with_progress(
                &mut self,
                name: &str,
                arguments: serde_json::Value,
                _: crate::proxy::ProgressCallback<'_>,
            ) -> McpResult<Vec<Content>> {
                self.call_tool(name, arguments)
            }

            fn read_resource(&mut self, _: &str) -> McpResult<Vec<ResourceContent>> {
                Ok(Vec::new())
            }

            fn get_prompt(
                &mut self,
                _: &str,
                _: std::collections::HashMap<String, String>,
            ) -> McpResult<Vec<fastmcp_protocol::PromptMessage>> {
                Ok(Vec::new())
            }

            fn call_tool_result(
                &mut self,
                name: &str,
                arguments: serde_json::Value,
            ) -> McpResult<CoreResult> {
                self.record(name, arguments);
                match self.route {
                    DualEraProxyRoute::Legacy => Ok(CoreResult::Legacy(
                        LegacyCoreResult::ToolsCall(CallToolResult {
                            content: vec![LegacyContent::Text {
                                text: "bound legacy proxy".to_owned(),
                                annotations: None,
                                additional: std::collections::BTreeMap::new(),
                            }],
                            is_error: false,
                            meta: None,
                            additional: std::collections::BTreeMap::new(),
                        }),
                    )),
                    DualEraProxyRoute::Final => Ok(CoreResult::Final(FinalCoreResult::ToolsCall {
                        result: CompleteResult::new(
                            FinalCallToolResult {
                                content: vec![ContentBlock::text("bound final proxy")],
                                is_error: false,
                                structured_content: Some(serde_json::json!({"route": "final"})),
                            },
                            ResultMeta::server_generated(
                                Implementation::try_new("bound-final-upstream", "1.0")
                                    .expect("the fixed test implementation is valid"),
                            ),
                        ),
                        diagnostic: None,
                    })),
                }
            }
        }

        fn dual_era_proxy_client(
            route: DualEraProxyRoute,
            calls: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
        ) -> ProxyClient {
            let era = match route {
                DualEraProxyRoute::Legacy => ProtocolEra::Legacy2024,
                DualEraProxyRoute::Final => ProtocolEra::Modern2026,
            };
            let (route_id, upstream_identity) = match route {
                DualEraProxyRoute::Legacy => ("dual-era-legacy-route", "stdio:dual-era-legacy"),
                DualEraProxyRoute::Final => ("dual-era-final-route", "stdio:dual-era-final"),
            };
            let policy = match era {
                ProtocolEra::Modern2026 => ProtocolPolicy::ModernOnly,
                ProtocolEra::Legacy2024 => ProtocolPolicy::LegacyOnly,
            };
            let opening = match era {
                ProtocolEra::Modern2026 => StdioOpeningFrame::ModernRequest {
                    protocol_version: era.version().as_str().to_owned(),
                },
                ProtocolEra::Legacy2024 => StdioOpeningFrame::LegacyInitialize,
            };
            let mut bindings = ProxyClient::upstream_binding_registry();
            let binding = bindings
                .bind_stdio(
                    route_id,
                    upstream_identity,
                    "dual-era-receipt",
                    1,
                    policy,
                    opening,
                )
                .expect("the test route selects one immutable era");
            let upstream_protocol_version = era.version().as_str().to_owned();
            ProxyClient::from_backend_with_upstream_binding(
                RecordingDualEraProxyBackend { route, calls },
                binding,
                &upstream_protocol_version,
            )
            .expect("the selected upstream version matches its immutable binding")
        }

        fn dual_era_proxy_server() -> (
            crate::Server,
            Arc<Mutex<Vec<(String, serde_json::Value)>>>,
            Arc<Mutex<Vec<(String, serde_json::Value)>>>,
        ) {
            let legacy_calls = Arc::new(Mutex::new(Vec::new()));
            let final_calls = Arc::new(Mutex::new(Vec::new()));
            let server = ServerBuilder::new("srv", "1.0")
                .proxy(
                    dual_era_proxy_client(DualEraProxyRoute::Legacy, Arc::clone(&legacy_calls)),
                    legacy_proxy_catalog(),
                )
                .expect("the legacy proxy catalog agrees with its bound route")
                .proxy(
                    dual_era_proxy_client(DualEraProxyRoute::Final, Arc::clone(&final_calls)),
                    final_proxy_catalog(),
                )
                .expect("the final proxy catalog agrees with its bound route")
                .build();
            (server, legacy_calls, final_calls)
        }

        fn initialized_legacy_proxy_session(server: &crate::Server) -> crate::Session {
            let mut session =
                crate::Session::new(server.info().clone(), server.capabilities().clone());
            session.initialize(
                fastmcp_protocol::ClientInfo {
                    name: "dual-era-legacy-client".to_owned(),
                    version: "1.0".to_owned(),
                },
                fastmcp_protocol::ClientCapabilities::default(),
                "2024-11-05".to_owned(),
            );
            session
        }

        fn assert_rejected_proxy_catalog_returns_an_error(catalog: ProxyCatalog) {
            let error = match ServerBuilder::new("srv", "1.0").tool(TestTool).proxy(
                ProxyClient::from_backend(DuplicatePolicyProxyBackend),
                catalog,
            ) {
                Ok(_) => {
                    panic!("the malformed or unbound proxy catalog is rejected before registration")
                }
                Err(error) => error,
            };

            assert_eq!(error.code, fastmcp_core::McpErrorCode::InvalidRequest);
        }

        #[test]
        fn builder_proxy_accepts_a_coherent_legacy_catalog() {
            let server = ServerBuilder::new("srv", "1.0")
                .proxy(
                    final_catalog_proxy_client(ProtocolEra::Legacy2024),
                    legacy_proxy_catalog(),
                )
                .expect("the coherent legacy catalog agrees with its route")
                .build();

            assert!(server.has_tools());
            let tools = server.tools();
            assert_eq!(
                tools[0].name, "legacy-weather",
                "the public builder path registers a catalog whose marker and entries select legacy"
            );
        }

        #[test]
        fn builder_proxy_rejects_an_unbound_caller_asserted_legacy_catalog() {
            let error = match ServerBuilder::new("srv", "1.0").proxy(
                ProxyClient::from_backend(DuplicatePolicyProxyBackend),
                legacy_proxy_catalog(),
            ) {
                Ok(_) => panic!("a caller-supplied catalog cannot bind an unbound proxy route"),
                Err(error) => error,
            };

            assert_eq!(error.code, fastmcp_core::McpErrorCode::InvalidRequest);
            assert!(error.message.contains("cannot bind an unbound route"));
        }

        #[test]
        fn builder_proxy_rejects_legacy_tools_when_only_the_marker_changes_to_modern() {
            let mut catalog = legacy_proxy_catalog();
            catalog.tool_catalog_era = Some(ProtocolEra::Modern2026);

            assert_rejected_proxy_catalog_returns_an_error(catalog);
        }

        #[test]
        fn builder_proxy_rejects_final_tools_when_only_the_marker_changes_to_legacy() {
            let mut catalog = final_proxy_catalog();
            catalog.tool_catalog_era = Some(ProtocolEra::Legacy2024);

            assert_rejected_proxy_catalog_returns_an_error(catalog);
        }

        #[test]
        fn builder_proxy_rejects_mixed_vectors_when_only_final_tools_are_added() {
            let mut catalog = legacy_proxy_catalog();
            catalog.final_tools = final_proxy_catalog().final_tools;

            assert_rejected_proxy_catalog_returns_an_error(catalog);
        }

        #[test]
        fn builder_proxy_rejects_a_missing_marker() {
            let mut catalog = legacy_proxy_catalog();
            catalog.tool_catalog_era = None;

            assert_rejected_proxy_catalog_returns_an_error(catalog);
        }

        #[test]
        fn public_builder_proxy_advertises_and_lists_the_exact_final_tool_catalog() {
            let catalog = final_proxy_catalog();
            let expected = serde_json::to_value(&catalog.final_tools[0])
                .expect("the final fixture serializes");
            let server = ServerBuilder::new("srv", "1.0")
                .proxy(final_catalog_proxy_client(ProtocolEra::Modern2026), catalog)
                .expect("the final catalog agrees with its bound route")
                .build();

            assert!(server.has_tools());
            assert!(server.tools().is_empty());
            let discovery = serde_json::to_value(
                server
                    .server_discovery()
                    .expect("the public final proxy server is discoverable"),
            )
            .expect("final discovery serializes");
            assert_eq!(
                discovery.pointer("/capabilities/tools"),
                Some(&serde_json::json!({"listChanged": true}))
            );
            let inbound = crate::InboundRequestContext::new(
                Cx::for_testing(),
                701,
                crate::InboundRequestTransport::Memory,
            );
            let response = fastmcp_core::block_on(
                server.dispatch_stateless(&inbound, &final_tools_list_request(701)),
            )
            .expect("the public server path responds to the modern tools/list request");
            assert!(response.error.is_none());
            let catalog = response
                .result
                .expect("the modern tools/list response has a result payload");
            assert_eq!(catalog["tools"][0], expected);
        }

        #[test]
        fn public_builder_proxy_final_catalog_absence_changes_only_tool_advertisement_and_registry()
        {
            let mut catalog = final_proxy_catalog();
            let configured_tool_count = catalog.final_tools.len();
            catalog.final_tools.clear();
            assert_eq!(
                configured_tool_count, 1,
                "the planted negative removes only the final tool catalog entry"
            );

            let server = ServerBuilder::new("srv", "1.0")
                .proxy(final_catalog_proxy_client(ProtocolEra::Modern2026), catalog)
                .expect("an otherwise identical empty final catalog remains admissible")
                .build();

            assert!(!server.has_tools());
            let discovery = serde_json::to_value(
                server
                    .server_discovery()
                    .expect("the empty final proxy server remains discoverable"),
            )
            .expect("empty final discovery serializes");
            assert!(discovery.pointer("/capabilities/tools").is_none());
            let inbound = crate::InboundRequestContext::new(
                Cx::for_testing(),
                702,
                crate::InboundRequestTransport::Memory,
            );
            let response = fastmcp_core::block_on(
                server.dispatch_stateless(&inbound, &final_tools_list_request(702)),
            )
            .expect("the public final tools/list path responds for an empty catalog");
            assert_eq!(
                response.result,
                Some(serde_json::json!({
                    "resultType": "complete",
                    "tools": [],
                    "ttlMs": 300_000,
                    "cacheScope": "private",
                }))
            );
            assert!(
                server.tools().is_empty(),
                "removing only the final catalog entry leaves no legacy registry mutation"
            );
        }

        #[cfg(feature = "tasks")]
        #[test]
        fn public_builder_proxy_relays_modern_tasks_input_required_and_listener() {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let updates = Arc::new(Mutex::new(Vec::new()));
            let proxy = ordinary_proxy_tasks_client(Arc::clone(&calls), Arc::clone(&updates));
            let server = Arc::new(
                ServerBuilder::new("ordinary-proxy-tasks", "1.0")
                    .proxy(proxy.clone(), final_proxy_catalog())
                    .expect("the ordinary public proxy path installs the admitted modern route")
                    .build(),
            );
            let discovery = serde_json::to_value(
                server
                    .server_discovery()
                    .expect("the Tasks proxy is publicly discoverable"),
            )
            .expect("Tasks proxy discovery serializes");
            assert_eq!(
                discovery.pointer("/capabilities/extensions/io.modelcontextprotocol~1tasks"),
                Some(&serde_json::json!({})),
                "the ordinary proxy path advertises the same Tasks extension as the typed path"
            );

            let cx = Cx::for_testing();
            let connection = crate::ModernConnection::new();
            let emitted_notifications = Arc::new(Mutex::new(Vec::<JsonRpcRequest>::new()));
            let notification_sender: crate::NotificationSender = {
                let emitted_notifications = Arc::clone(&emitted_notifications);
                Arc::new(move |notification| {
                    emitted_notifications
                        .lock()
                        .expect("ordinary proxy emitted notification log is not poisoned")
                        .push(notification);
                })
            };
            let dispatch_modern = |request_id, request| {
                let inbound = crate::InboundRequestContext::with_modern_connection(
                    cx.clone(),
                    request_id,
                    crate::InboundRequestTransport::Memory,
                    &connection,
                );
                block_on(Arc::clone(&server).dispatch_with_protocol_policy_owned(
                    server.protocol_policy,
                    &inbound,
                    request,
                    None,
                    None,
                    None,
                    None,
                    fastmcp_core::McpRequestCancellation::new(),
                    None,
                    Arc::clone(&notification_sender),
                ))
                .expect("public ordinary proxy request receives a JSON-RPC response")
            };
            let task_parameters = serde_json::json!({
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {
                        "extensions": {"io.modelcontextprotocol/tasks": {}}
                    },
                },
                "name": "weather",
                "arguments": {"outcome": "task"},
            });
            let task = dispatch_modern(
                703,
                JsonRpcRequest::new("tools/call", Some(task_parameters.clone()), 703_i64),
            );
            assert_eq!(
                task.result
                    .as_ref()
                    .and_then(|result| result.get("resultType")),
                Some(&serde_json::json!("task"))
            );
            assert_eq!(
                task.result.as_ref().and_then(|result| result.get("taskId")),
                Some(&serde_json::json!("ordinary-proxy-task-71"))
            );
            let relay_after_task = proxy
                .final_task_registry_snapshot_for_test()
                .expect("the retained ordinary proxy exposes its route-local Task snapshot");
            assert_eq!(
                relay_after_task.pointer("/tasks/ordinary-proxy-task-71/status"),
                Some(&serde_json::json!("input_required")),
                "the public tools/call task result is retained by the ordinary route-local relay"
            );

            let update = dispatch_modern(
                704,
                JsonRpcRequest::new(
                    "tasks/update",
                    Some(serde_json::json!({
                        "taskId": "ordinary-proxy-task-71",
                        "inputResponses": {},
                        "_meta": task_parameters["_meta"].clone(),
                    })),
                    704_i64,
                ),
            );
            assert_eq!(
                update
                    .result
                    .as_ref()
                    .and_then(|result| result.get("resultType")),
                Some(&serde_json::json!("complete")),
                "the public Tasks update preserves the upstream final result algebra"
            );
            assert_eq!(
                updates
                    .lock()
                    .expect("ordinary proxy task update receipt is not poisoned")
                    .as_slice(),
                [serde_json::json!({})],
                "the public Tasks update reaches the upstream backend through the local relay"
            );
            assert_eq!(
                calls
                    .lock()
                    .expect("ordinary proxy task call log is not poisoned")
                    .as_slice(),
                ["tools/call:weather".to_owned(), "tasks/update".to_owned()],
                "the public update reaches the selected upstream only after local relay admission"
            );

            let mut input_required_parameters = task_parameters.clone();
            input_required_parameters["arguments"] = serde_json::json!({});
            let listened = dispatch_modern(
                705,
                JsonRpcRequest::new(
                    "subscriptions/listen",
                    Some(serde_json::json!({
                        "notifications": {"taskIds": []},
                        "_meta": task_parameters["_meta"].clone(),
                    })),
                    705_i64,
                ),
            );
            assert_eq!(
                listened
                    .result
                    .as_ref()
                    .and_then(|result| result.get("resultType")),
                Some(&serde_json::json!("complete"))
            );
            let emitted_before_rejection = emitted_notifications
                .lock()
                .expect("ordinary proxy emitted notification log is not poisoned")
                .clone();
            assert_eq!(
                emitted_before_rejection.len(),
                1,
                "the public listener emits its acknowledgement before terminal completion"
            );
            assert_eq!(
                emitted_before_rejection
                    .first()
                    .map(|notification| notification.method.as_str()),
                Some("notifications/subscriptions/acknowledged"),
                "the captured local notification is the listener acknowledgement"
            );
            let calls_before_rejection = calls
                .lock()
                .expect("ordinary proxy task call log is not poisoned")
                .clone();
            let relay_before_rejection = proxy
                .final_task_registry_snapshot_for_test()
                .expect("the ordinary proxy keeps its local final Task registry");
            let subscriptions_before_rejection = server.final_subscription_snapshot_for_test();
            let mut missing_capability = task_parameters;
            assert!(
                missing_capability
                    .pointer_mut("/_meta/io.modelcontextprotocol~1clientCapabilities/extensions")
                    .and_then(serde_json::Value::as_object_mut)
                    .expect("the admitted request has an extension map")
                    .remove("io.modelcontextprotocol/tasks")
                    .is_some(),
                "the RH-5 negative changes only the client Tasks capability"
            );
            let rejected = dispatch_modern(
                706,
                JsonRpcRequest::new("tools/call", Some(missing_capability), 706_i64),
            );
            assert!(rejected.error.is_some());
            assert_eq!(
                calls
                    .lock()
                    .expect("ordinary proxy task call log is not poisoned")
                    .clone(),
                calls_before_rejection,
                "the one-field capability rejection must not invoke or mutate the upstream route"
            );
            assert_eq!(
                proxy
                    .final_task_registry_snapshot_for_test()
                    .expect("the ordinary proxy keeps its local final Task registry"),
                relay_before_rejection,
                "the one-field capability rejection must not mutate the local relay task registry"
            );
            assert_eq!(
                server.final_subscription_snapshot_for_test(),
                subscriptions_before_rejection,
                "the one-field capability rejection must not mutate local subscription delivery state"
            );
            assert_eq!(
                serde_json::to_value(
                    emitted_notifications
                        .lock()
                        .expect("ordinary proxy emitted notification log is not poisoned")
                        .clone()
                )
                .expect("emitted notification log serializes"),
                serde_json::to_value(emitted_before_rejection)
                    .expect("baseline notification log serializes"),
                "the one-field capability rejection must not emit or alter local notification state"
            );

            let input_required = dispatch_modern(
                707,
                JsonRpcRequest::new("tools/call", Some(input_required_parameters), 707_i64),
            );
            assert_eq!(
                input_required
                    .result
                    .as_ref()
                    .and_then(|result| result.get("resultType")),
                Some(&serde_json::json!("input_required"))
            );
            assert_ne!(
                input_required
                    .result
                    .as_ref()
                    .and_then(|result| result.get("requestState")),
                Some(&serde_json::json!("upstream-forged-state")),
                "the downstream router must mint rather than replay upstream MRTR state"
            );

            assert_eq!(
                calls
                    .lock()
                    .expect("ordinary proxy task call log is not poisoned")
                    .as_slice(),
                [
                    "tools/call:weather".to_owned(),
                    "tasks/update".to_owned(),
                    "subscriptions/listen".to_owned(),
                    "tools/call:weather".to_owned(),
                ],
                "public task update, listener, and input_required requests stay on the one admitted modern upstream"
            );
        }

        #[test]
        fn builder_proxy_dual_era_preserves_final_catalog_and_routes_bound_calls() {
            let expected_final_tool = serde_json::to_value(&final_proxy_catalog().final_tools[0])
                .expect("the final fixture serializes");
            let (server, legacy_calls, final_calls) = dual_era_proxy_server();
            let mut legacy_session = initialized_legacy_proxy_session(&server);
            let notification_sender: crate::NotificationSender = Arc::new(|_| {});
            let request_sender = crate::RequestSender::new(
                Arc::new(crate::PendingRequests::new()),
                Arc::new(|message| {
                    Err(format!("unexpected outbound message in test: {message:?}"))
                }),
            );

            let legacy_catalog = fastmcp_core::block_on(server.dispatch_request(
                &Cx::for_testing(),
                &mut legacy_session,
                JsonRpcRequest::new("tools/list", Some(serde_json::json!({})), 801_i64),
                &notification_sender,
                &request_sender,
            ))
            .expect("the legacy tools/list request receives a response")
            .result
            .expect("the legacy tools/list response has a result payload");
            assert_eq!(legacy_catalog["tools"].as_array().map(Vec::len), Some(1));
            assert_eq!(legacy_catalog["tools"][0]["name"], "legacy-weather");

            let final_inbound = crate::InboundRequestContext::new(
                Cx::for_testing(),
                802,
                crate::InboundRequestTransport::Memory,
            );
            let final_catalog = fastmcp_core::block_on(
                server.dispatch_stateless(&final_inbound, &final_tools_list_request(802_i64)),
            )
            .expect("the final tools/list request receives a response")
            .result
            .expect("the final tools/list response has a result payload");
            assert_eq!(final_catalog["tools"].as_array().map(Vec::len), Some(1));
            assert_eq!(final_catalog["tools"][0]["name"], "weather");
            assert_eq!(
                serde_json::to_vec(&final_catalog["tools"][0])
                    .expect("the emitted final tool normalizes to JSON"),
                serde_json::to_vec(&expected_final_tool)
                    .expect("the exact final fixture normalizes to JSON"),
                "the final proxy path retains the full normalized FinalTool model"
            );

            let legacy_call = fastmcp_core::block_on(server.dispatch_request(
                &Cx::for_testing(),
                &mut legacy_session,
                JsonRpcRequest::new(
                    "tools/call",
                    Some(serde_json::json!({
                        "name": "legacy-weather",
                        "arguments": {"city": "Portland"},
                    })),
                    803_i64,
                ),
                &notification_sender,
                &request_sender,
            ))
            .expect("the legacy tools/call request receives a response")
            .result
            .expect("the legacy tools/call response has a result payload");
            assert_eq!(legacy_call["content"][0]["text"], "bound legacy proxy");

            let final_connection = crate::ModernConnection::new();
            let final_call_inbound = crate::InboundRequestContext::with_modern_connection(
                Cx::for_testing(),
                804,
                crate::InboundRequestTransport::Memory,
                &final_connection,
            );
            let final_call_response = fastmcp_core::block_on(server.dispatch_stateless(
                &final_call_inbound,
                &final_tools_call_request("weather", serde_json::json!({"city": "Boston"}), 804),
            ))
            .expect("the final tools/call request receives a response");
            assert!(
                final_call_response.error.is_none(),
                "the bound final proxy call must succeed: {:?}",
                final_call_response.error
            );
            let final_call = final_call_response
                .result
                .expect("the final tools/call response has a result payload");
            assert_eq!(final_call["resultType"], "complete");
            assert_eq!(final_call["content"][0]["text"], "bound final proxy");
            assert_eq!(
                final_call["structuredContent"],
                serde_json::json!({"route": "final"})
            );

            assert_eq!(
                legacy_calls
                    .lock()
                    .expect("the test call log lock is not poisoned")
                    .clone(),
                vec![(
                    "legacy-weather".to_owned(),
                    serde_json::json!({"city": "Portland"}),
                )],
                "the legacy request reaches only its bound legacy upstream"
            );
            assert_eq!(
                final_calls
                    .lock()
                    .expect("the test call log lock is not poisoned")
                    .clone(),
                vec![("weather".to_owned(), serde_json::json!({"city": "Boston"}))],
                "the final request reaches only its bound final upstream"
            );
        }

        #[test]
        fn builder_proxy_dual_era_rejects_cross_era_names_without_upstream_calls() {
            let (server, legacy_calls, final_calls) = dual_era_proxy_server();
            let mut legacy_session = initialized_legacy_proxy_session(&server);
            let notification_sender: crate::NotificationSender = Arc::new(|_| {});
            let request_sender = crate::RequestSender::new(
                Arc::new(crate::PendingRequests::new()),
                Arc::new(|message| {
                    Err(format!("unexpected outbound message in test: {message:?}"))
                }),
            );

            let legacy_rejected = fastmcp_core::block_on(server.dispatch_request(
                &Cx::for_testing(),
                &mut legacy_session,
                JsonRpcRequest::new(
                    "tools/call",
                    Some(serde_json::json!({
                        "name": "weather",
                        "arguments": {},
                    })),
                    805_i64,
                ),
                &notification_sender,
                &request_sender,
            ))
            .expect("the legacy cross-era request receives a JSON-RPC error");
            assert!(legacy_rejected.result.is_none());
            assert_eq!(
                legacy_rejected.error.and_then(|error| error.code.as_i32()),
                Some(-32601),
                "the final-only name is absent from the legacy call route"
            );

            let final_inbound = crate::InboundRequestContext::new(
                Cx::for_testing(),
                806,
                crate::InboundRequestTransport::Memory,
            );
            let final_rejected = fastmcp_core::block_on(server.dispatch_stateless(
                &final_inbound,
                &final_tools_call_request("legacy-weather", serde_json::json!({}), 806),
            ))
            .expect("the final cross-era request receives a JSON-RPC error");
            assert!(final_rejected.result.is_none());
            assert_eq!(
                final_rejected.error.and_then(|error| error.code.as_i32()),
                Some(-32602),
                "the legacy-only name is absent from the final call route"
            );

            assert!(
                legacy_calls
                    .lock()
                    .expect("the test call log lock is not poisoned")
                    .is_empty(),
                "the final-only name must be rejected before the legacy upstream is called"
            );
            assert!(
                final_calls
                    .lock()
                    .expect("the test call log lock is not poisoned")
                    .is_empty(),
                "the legacy-only name must be rejected before the final upstream is called"
            );
        }

        #[test]
        fn public_proxy_completion_forwards_exact_legacy_and_final_results() {
            let legacy_calls = Arc::new(Mutex::new(Vec::new()));
            let final_calls = Arc::new(Mutex::new(Vec::new()));
            let server = ServerBuilder::new("completion-proxy", "1.0")
                .proxy_typed(
                    bound_proxy_client(
                        CompletionProxyBackend {
                            supported: true,
                            result: legacy_completion_proxy_result(),
                            calls: Arc::clone(&legacy_calls),
                        },
                        ProtocolEra::Legacy2024,
                    ),
                    legacy_completion_proxy_catalog(),
                )
                .expect("the exact legacy completion proxy installs")
                .proxy_typed(
                    bound_proxy_client(
                        CompletionProxyBackend {
                            supported: true,
                            result: final_completion_proxy_result(),
                            calls: Arc::clone(&final_calls),
                        },
                        ProtocolEra::Modern2026,
                    ),
                    final_completion_proxy_catalog(),
                )
                .expect("the exact final completion proxy installs")
                .build();
            assert!(
                server.capabilities().completions.is_some(),
                "installing a proxied completion provider must advertise initialize completions"
            );

            let mut legacy_session = initialized_legacy_proxy_session(&server);
            let notification_sender: crate::NotificationSender = Arc::new(|_| {});
            let request_sender = crate::RequestSender::new(
                Arc::new(crate::PendingRequests::new()),
                Arc::new(|message| {
                    Err(format!("unexpected outbound message in test: {message:?}"))
                }),
            );
            let legacy = fastmcp_core::block_on(server.dispatch_request(
                &Cx::for_testing(),
                &mut legacy_session,
                JsonRpcRequest::new(
                    "completion/complete",
                    Some(serde_json::json!({
                        "ref": {"type": "ref/prompt", "name": "legacy-deploy"},
                        "argument": {"name": "environment", "value": "sta"},
                    })),
                    821_i64,
                ),
                &notification_sender,
                &request_sender,
            ))
            .expect("the public exact-2024 completion path returns a response")
                .result
                .expect("the exact-2024 completion response has a result payload");
            assert_eq!(
                legacy["completion"]["values"],
                serde_json::json!(["legacy-staging"])
            );

            let final_inbound = crate::InboundRequestContext::new(
                Cx::for_testing(),
                822,
                crate::InboundRequestTransport::Memory,
            );
            let final_response = fastmcp_core::block_on(
                server.dispatch_stateless(&final_inbound, &final_completion_request(822)),
            )
            .expect("the public final completion path returns a response")
            .result
            .expect("the final completion response has a result payload");
            assert_eq!(final_response["resultType"], "complete");
            assert_eq!(
                final_response["completion"]["values"],
                serde_json::json!(["final-staging"])
            );
            assert_eq!(
                final_response["completion"]["total"],
                serde_json::json!(92233720368547758081234567890_u128),
                "the proxy retains the final arbitrary-precision completion total"
            );

            let legacy_calls = legacy_calls
                .lock()
                .expect("legacy completion call log is not poisoned");
            assert_eq!(legacy_calls.len(), 1);
            assert!(matches!(
                &legacy_calls[0].reference,
                fastmcp_client::CompletionReference::Prompt { name } if name == "legacy-deploy"
            ));
            assert!(legacy_calls[0].context.is_none());
            let final_calls = final_calls
                .lock()
                .expect("final completion call log is not poisoned");
            assert_eq!(final_calls.len(), 1);
            assert!(matches!(
                &final_calls[0].reference,
                fastmcp_client::CompletionReference::PromptWithTitle { name, title }
                    if name == "final-deploy" && title == "Final Deploy"
            ));
            assert_eq!(
                final_calls[0]
                    .context
                    .as_ref()
                    .and_then(|context| context.arguments.as_ref())
                    .and_then(|arguments| arguments.get("region")),
                Some(&"us-east-1".to_owned()),
                "the proxy forwards final completion context unchanged"
            );
        }

        #[test]
        fn public_proxy_completion_unsupported_upstream_rejects_without_downstream_mutation() {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let server = ServerBuilder::new("completion-proxy", "1.0")
                .proxy_typed(
                    bound_proxy_client(
                        CompletionProxyBackend {
                            supported: false,
                            result: final_completion_proxy_result(),
                            calls: Arc::clone(&calls),
                        },
                        ProtocolEra::Modern2026,
                    ),
                    final_completion_proxy_catalog(),
                )
                .expect("changing only upstream completion support preserves catalog registration")
                .build();
            assert!(
                server.capabilities().completions.is_none(),
                "an unsupported upstream must not advertise initialize completions"
            );
            let discovery = serde_json::to_value(
                server
                    .server_discovery()
                    .expect("the final proxy server remains discoverable"),
            )
            .expect("discovery serializes");
            assert!(
                discovery["capabilities"].get("completions").is_none(),
                "an unsupported upstream must not create a downstream completion claim"
            );

            let before_inbound = crate::InboundRequestContext::new(
                Cx::for_testing(),
                823,
                crate::InboundRequestTransport::Memory,
            );
            let before = fastmcp_core::block_on(server.dispatch_stateless(
                &before_inbound,
                &JsonRpcRequest::new(
                    "prompts/list",
                    Some(serde_json::json!({
                        "_meta": {
                            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                            "io.modelcontextprotocol/clientCapabilities": {},
                        },
                    })),
                    823_i64,
                ),
            ))
            .expect("the proxied final prompt catalog is public before rejection")
            .result
            .expect("the proxied final prompt catalog has a result payload");
            let rejected_inbound = crate::InboundRequestContext::new(
                Cx::for_testing(),
                824,
                crate::InboundRequestTransport::Memory,
            );
            let rejected = fastmcp_core::block_on(
                server.dispatch_stateless(&rejected_inbound, &final_completion_request(824)),
            )
            .expect("the unsupported completion request receives a JSON-RPC error");
            assert_eq!(
                rejected.error.and_then(|error| error.code.as_i32()),
                Some(-32601),
                "the absent local completion handler rejects before an upstream call"
            );
            assert!(rejected.result.is_none());
            let after_inbound = crate::InboundRequestContext::new(
                Cx::for_testing(),
                825,
                crate::InboundRequestTransport::Memory,
            );
            let after = fastmcp_core::block_on(server.dispatch_stateless(
                &after_inbound,
                &JsonRpcRequest::new(
                    "prompts/list",
                    Some(serde_json::json!({
                        "_meta": {
                            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                            "io.modelcontextprotocol/clientCapabilities": {},
                        },
                    })),
                    825_i64,
                ),
            ))
            .expect("the proxied final prompt catalog remains public after rejection")
            .result
            .expect("the proxied final prompt catalog still has a result payload");
            assert_eq!(
                before, after,
                "the rejected request cannot mutate downstream catalog state"
            );
            assert!(
                calls
                    .lock()
                    .expect("completion proxy call log is not poisoned")
                    .is_empty(),
                "the unsupported upstream is never invoked"
            );
        }

        #[test]
        fn public_proxy_completion_duplicate_local_target_keeps_no_upstream_mapping() {
            for duplicate_behavior in [DuplicateBehavior::Warn, DuplicateBehavior::Ignore] {
                let calls = Arc::new(Mutex::new(Vec::new()));
                let server = ServerBuilder::new("completion-proxy", "1.0")
                    .prompt(LocalFinalCompletionPrompt)
                    .on_duplicate(duplicate_behavior)
                    .proxy_typed(
                        bound_proxy_client(
                            CompletionProxyBackend {
                                supported: true,
                                result: final_completion_proxy_result(),
                                calls: Arc::clone(&calls),
                            },
                            ProtocolEra::Modern2026,
                        ),
                        final_completion_proxy_catalog(),
                    )
                    .expect("retaining the local final prompt is a successful duplicate admission")
                    .build();

                let discovery = serde_json::to_value(
                    server
                        .server_discovery()
                        .expect("the retained local prompt server remains discoverable"),
                )
                .expect("discovery serializes");
                assert!(
                    discovery["capabilities"].get("completions").is_none(),
                    "{duplicate_behavior:?} must not advertise an upstream provider for a retained local target"
                );

                let before_inbound = crate::InboundRequestContext::new(
                    Cx::for_testing(),
                    826,
                    crate::InboundRequestTransport::Memory,
                );
                let before = fastmcp_core::block_on(server.dispatch_stateless(
                    &before_inbound,
                    &JsonRpcRequest::new(
                        "prompts/list",
                        Some(serde_json::json!({
                            "_meta": {
                                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                                "io.modelcontextprotocol/clientCapabilities": {},
                            },
                        })),
                        826_i64,
                    ),
                ))
                .expect("the retained local prompt is visible through the public final list")
                .result
                .expect("the retained local prompt list has a result payload");
                assert_eq!(
                    before["prompts"][0]["title"], "Local Final Deploy",
                    "{duplicate_behavior:?} retains the pre-existing local prompt target"
                );

                let rejected_inbound = crate::InboundRequestContext::new(
                    Cx::for_testing(),
                    827,
                    crate::InboundRequestTransport::Memory,
                );
                let rejected = fastmcp_core::block_on(
                    server.dispatch_stateless(&rejected_inbound, &final_completion_request(827)),
                )
                .expect("the absent completion mapping returns a JSON-RPC error");
                assert_eq!(
                    rejected.error.and_then(|error| error.code.as_i32()),
                    Some(-32601),
                    "{duplicate_behavior:?} rejects before invocation because no upstream mapping was installed"
                );
                assert!(rejected.result.is_none());

                let after_inbound = crate::InboundRequestContext::new(
                    Cx::for_testing(),
                    828,
                    crate::InboundRequestTransport::Memory,
                );
                let after = fastmcp_core::block_on(server.dispatch_stateless(
                    &after_inbound,
                    &JsonRpcRequest::new(
                        "prompts/list",
                        Some(serde_json::json!({
                            "_meta": {
                                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                                "io.modelcontextprotocol/clientCapabilities": {},
                            },
                        })),
                        828_i64,
                    ),
                ))
                .expect("the rejected request leaves the retained local target public")
                .result
                .expect("the retained local prompt list still has a result payload");
                assert_eq!(
                    before, after,
                    "{duplicate_behavior:?} completion rejection cannot mutate the retained local target"
                );
                assert!(
                    calls
                        .lock()
                        .expect("completion proxy call log is not poisoned")
                        .is_empty(),
                    "{duplicate_behavior:?} must not invoke the upstream completion backend"
                );
            }
        }

        #[test]
        fn public_proxy_completion_replace_installs_upstream_mapping_for_replaced_target() {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let server = ServerBuilder::new("completion-proxy", "1.0")
                .prompt(LocalFinalCompletionPrompt)
                .on_duplicate(DuplicateBehavior::Replace)
                .proxy_typed(
                    bound_proxy_client(
                        CompletionProxyBackend {
                            supported: true,
                            result: final_completion_proxy_result(),
                            calls: Arc::clone(&calls),
                        },
                        ProtocolEra::Modern2026,
                    ),
                    final_completion_proxy_catalog(),
                )
                .expect("Replace admits the exact upstream final prompt target")
                .build();

            let discovery = serde_json::to_value(
                server
                    .server_discovery()
                    .expect("the replacement proxy server remains discoverable"),
            )
            .expect("discovery serializes");
            assert_eq!(
                discovery["capabilities"]["completions"],
                serde_json::json!({}),
                "the real upstream mapping advertises final completion support"
            );

            let list_inbound = crate::InboundRequestContext::new(
                Cx::for_testing(),
                829,
                crate::InboundRequestTransport::Memory,
            );
            let prompts = fastmcp_core::block_on(server.dispatch_stateless(
                &list_inbound,
                &JsonRpcRequest::new(
                    "prompts/list",
                    Some(serde_json::json!({
                        "_meta": {
                            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                            "io.modelcontextprotocol/clientCapabilities": {},
                        },
                    })),
                    829_i64,
                ),
            ))
            .expect("the replaced prompt is public through the final list")
            .result
            .expect("the replaced prompt list has a result payload");
            assert_eq!(
                prompts["prompts"][0]["title"], "Upstream Final Deploy",
                "Replace exposes the admitted upstream prompt instead of the local target"
            );

            let completion_inbound = crate::InboundRequestContext::new(
                Cx::for_testing(),
                830,
                crate::InboundRequestTransport::Memory,
            );
            let completion = fastmcp_core::block_on(
                server.dispatch_stateless(&completion_inbound, &final_completion_request(830)),
            )
            .expect("the replaced target forwards through the public final completion route")
            .result
            .expect("the replaced target completion has a result payload");
            assert_eq!(
                completion["completion"]["values"],
                serde_json::json!(["final-staging"])
            );
            assert_eq!(
                calls
                    .lock()
                    .expect("completion proxy call log is not poisoned")
                    .len(),
                1,
                "Replace installs exactly one upstream completion invocation path"
            );
        }

        #[test]
        fn public_final_proxy_completion_replacement_evicts_proxy_targets_and_restores_local_providers()
         {
            let prompt_calls = Arc::new(Mutex::new(Vec::new()));
            let template_calls = Arc::new(Mutex::new(Vec::new()));
            let evicted = ServerBuilder::new("completion-proxy", "1.0")
                .proxy_typed(
                    bound_proxy_client(
                        CompletionProxyBackend {
                            supported: true,
                            result: final_completion_proxy_result(),
                            calls: Arc::clone(&prompt_calls),
                        },
                        ProtocolEra::Modern2026,
                    ),
                    final_completion_proxy_catalog(),
                )
                .expect("the final prompt proxy installs")
                .proxy_typed(
                    bound_proxy_client(
                        CompletionProxyBackend {
                            supported: true,
                            result: final_completion_proxy_result(),
                            calls: Arc::clone(&template_calls),
                        },
                        ProtocolEra::Modern2026,
                    ),
                    final_completion_proxy_template_catalog(),
                )
                .expect("the final resource-template proxy installs")
                .on_duplicate(DuplicateBehavior::Replace)
                .prompt(LocalFinalCompletionPrompt)
                .resource_template(local_completion_template())
                .build();

            let discovery = serde_json::to_value(
                evicted
                    .server_discovery()
                    .expect("the locally replaced catalog remains discoverable"),
            )
            .expect("discovery serializes");
            assert!(
                discovery["capabilities"].get("completions").is_none(),
                "replacing every proxied final target removes proxy completion advertisement"
            );
            for (id, request) in [
                (831, final_completion_request(831)),
                (832, final_resource_template_completion_request(832)),
            ] {
                let inbound = crate::InboundRequestContext::new(
                    Cx::for_testing(),
                    id,
                    crate::InboundRequestTransport::Memory,
                );
                let rejected =
                    fastmcp_core::block_on(evicted.dispatch_stateless(&inbound, &request))
                        .expect("the evicted completion mapping returns a JSON-RPC error");
                assert_eq!(
                    rejected.error.and_then(|error| error.code.as_i32()),
                    Some(-32601),
                    "the local replacement has no retained upstream completion provider"
                );
                assert!(rejected.result.is_none());
            }
            assert!(
                prompt_calls
                    .lock()
                    .expect("prompt completion proxy call log is not poisoned")
                    .is_empty()
            );
            assert!(
                template_calls
                    .lock()
                    .expect("template completion proxy call log is not poisoned")
                    .is_empty()
            );

            let prompt_calls = Arc::new(Mutex::new(Vec::new()));
            let template_calls = Arc::new(Mutex::new(Vec::new()));
            let restored = ServerBuilder::new("completion-proxy", "1.0")
                .proxy_typed(
                    bound_proxy_client(
                        CompletionProxyBackend {
                            supported: true,
                            result: final_completion_proxy_result(),
                            calls: Arc::clone(&prompt_calls),
                        },
                        ProtocolEra::Modern2026,
                    ),
                    final_completion_proxy_catalog(),
                )
                .expect("the final prompt proxy installs")
                .proxy_typed(
                    bound_proxy_client(
                        CompletionProxyBackend {
                            supported: true,
                            result: final_completion_proxy_result(),
                            calls: Arc::clone(&template_calls),
                        },
                        ProtocolEra::Modern2026,
                    ),
                    final_completion_proxy_template_catalog(),
                )
                .expect("the final resource-template proxy installs")
                .on_duplicate(DuplicateBehavior::Replace)
                .prompt(LocalFinalCompletionPrompt)
                .resource_template(local_completion_template())
                .prompt_completion_handler("final-deploy", TestCompletion)
                .resource_template_completion_handler("completion://{environment}", TestCompletion)
                .build();
            let discovery = serde_json::to_value(
                restored
                    .server_discovery()
                    .expect("the local completion providers make final discovery available"),
            )
            .expect("discovery serializes");
            assert_eq!(
                discovery["capabilities"]["completions"],
                serde_json::json!({}),
                "only the restored local final providers advertise completion support"
            );
            for (id, request) in [
                (833, final_completion_request(833)),
                (834, final_resource_template_completion_request(834)),
            ] {
                let inbound = crate::InboundRequestContext::new(
                    Cx::for_testing(),
                    id,
                    crate::InboundRequestTransport::Memory,
                );
                let response =
                    fastmcp_core::block_on(restored.dispatch_stateless(&inbound, &request))
                        .expect("the local replacement provider handles the public final request")
                        .result
                        .expect("the local replacement provider returns a result payload");
                assert_eq!(
                    response["completion"]["values"],
                    serde_json::json!(["staging"])
                );
            }
            assert!(
                prompt_calls
                    .lock()
                    .expect("prompt completion proxy call log is not poisoned")
                    .is_empty(),
                "local prompt providers must not invoke the displaced proxy"
            );
            assert!(
                template_calls
                    .lock()
                    .expect("template completion proxy call log is not poisoned")
                    .is_empty(),
                "local template providers must not invoke the displaced proxy"
            );
        }

        #[test]
        fn public_legacy_proxy_completion_replacement_evicts_proxy_targets_and_restores_local_provider()
         {
            let prompt_calls = Arc::new(Mutex::new(Vec::new()));
            let template_calls = Arc::new(Mutex::new(Vec::new()));
            let evicted = ServerBuilder::new("completion-proxy", "1.0")
                .proxy_typed(
                    bound_proxy_client(
                        CompletionProxyBackend {
                            supported: true,
                            result: legacy_completion_proxy_result(),
                            calls: Arc::clone(&prompt_calls),
                        },
                        ProtocolEra::Legacy2024,
                    ),
                    legacy_completion_proxy_catalog(),
                )
                .expect("the legacy prompt proxy installs")
                .proxy_typed(
                    bound_proxy_client(
                        CompletionProxyBackend {
                            supported: true,
                            result: legacy_completion_proxy_result(),
                            calls: Arc::clone(&template_calls),
                        },
                        ProtocolEra::Legacy2024,
                    ),
                    legacy_completion_proxy_template_catalog(),
                )
                .expect("the legacy resource-template proxy installs")
                .on_duplicate(DuplicateBehavior::Replace)
                .legacy_prompt(LocalLegacyCompletionPrompt)
                .legacy_resource_template(local_completion_template())
                .build();
            let mut legacy_session = initialized_legacy_proxy_session(&evicted);
            let notification_sender: crate::NotificationSender = Arc::new(|_| {});
            let request_sender = crate::RequestSender::new(
                Arc::new(crate::PendingRequests::new()),
                Arc::new(|message| {
                    Err(format!("unexpected outbound message in test: {message:?}"))
                }),
            );
            for request in [
                legacy_completion_request(
                    835,
                    serde_json::json!({"type": "ref/prompt", "name": "legacy-deploy"}),
                ),
                legacy_completion_request(
                    836,
                    serde_json::json!({"type": "ref/resource", "uri": "completion://{environment}"}),
                ),
            ] {
                let rejected = fastmcp_core::block_on(evicted.dispatch_request(
                    &Cx::for_testing(),
                    &mut legacy_session,
                    request,
                    &notification_sender,
                    &request_sender,
                ))
                .expect("the public replaced legacy target returns a JSON-RPC error");
                assert_eq!(
                    rejected.error.and_then(|error| error.code.as_i32()),
                    Some(-32601),
                    "the local replacement has no retained upstream completion provider"
                );
                assert!(rejected.result.is_none());
            }
            assert!(
                prompt_calls
                    .lock()
                    .expect("prompt completion proxy call log is not poisoned")
                    .is_empty()
            );
            assert!(
                template_calls
                    .lock()
                    .expect("template completion proxy call log is not poisoned")
                    .is_empty()
            );

            let prompt_calls = Arc::new(Mutex::new(Vec::new()));
            let template_calls = Arc::new(Mutex::new(Vec::new()));
            let restored = ServerBuilder::new("completion-proxy", "1.0")
                .proxy_typed(
                    bound_proxy_client(
                        CompletionProxyBackend {
                            supported: true,
                            result: legacy_completion_proxy_result(),
                            calls: Arc::clone(&prompt_calls),
                        },
                        ProtocolEra::Legacy2024,
                    ),
                    legacy_completion_proxy_catalog(),
                )
                .expect("the legacy prompt proxy installs")
                .proxy_typed(
                    bound_proxy_client(
                        CompletionProxyBackend {
                            supported: true,
                            result: legacy_completion_proxy_result(),
                            calls: Arc::clone(&template_calls),
                        },
                        ProtocolEra::Legacy2024,
                    ),
                    legacy_completion_proxy_template_catalog(),
                )
                .expect("the legacy resource-template proxy installs")
                .on_duplicate(DuplicateBehavior::Replace)
                .legacy_prompt(LocalLegacyCompletionPrompt)
                .legacy_resource_template(local_completion_template())
                .legacy_completion_handler(TestCompletion)
                .build();
            let mut legacy_session = initialized_legacy_proxy_session(&restored);
            let notification_sender: crate::NotificationSender = Arc::new(|_| {});
            let request_sender = crate::RequestSender::new(
                Arc::new(crate::PendingRequests::new()),
                Arc::new(|message| {
                    Err(format!("unexpected outbound message in test: {message:?}"))
                }),
            );
            for request in [
                legacy_completion_request(
                    837,
                    serde_json::json!({"type": "ref/prompt", "name": "legacy-deploy"}),
                ),
                legacy_completion_request(
                    838,
                    serde_json::json!({"type": "ref/resource", "uri": "completion://{environment}"}),
                ),
            ] {
                let response = fastmcp_core::block_on(restored.dispatch_request(
                    &Cx::for_testing(),
                    &mut legacy_session,
                    request,
                    &notification_sender,
                    &request_sender,
                ))
                .expect("the public local legacy provider returns a response")
                .result
                .expect("the local legacy provider returns a result payload");
                assert_eq!(
                    response["completion"]["values"],
                    serde_json::json!(["staging"])
                );
            }
            assert!(
                prompt_calls
                    .lock()
                    .expect("prompt completion proxy call log is not poisoned")
                    .is_empty()
            );
            assert!(
                template_calls
                    .lock()
                    .expect("template completion proxy call log is not poisoned")
                    .is_empty()
            );
        }

        #[test]
        fn builder_proxy_rejects_final_tools_with_one_legacy_resource_vector() {
            let mut catalog = final_proxy_catalog();
            catalog.resources.push(Resource {
                uri: "file:///must-not-project".to_owned(),
                name: "must-not-project".to_owned(),
                description: None,
                mime_type: None,
                icon: None,
                version: None,
                tags: Vec::new(),
            });

            assert_rejected_proxy_catalog_returns_an_error(catalog);
        }

        #[test]
        fn public_legacy_and_stateless_ping_answer_empty_object_without_entering_the_final_request_union()
         {
            let server = ServerBuilder::new("srv", "1.0").build();
            let mut legacy_session =
                crate::Session::new(server.info().clone(), server.capabilities().clone());
            legacy_session.initialize(
                fastmcp_protocol::ClientInfo {
                    name: "exact-2024-ping-client".to_owned(),
                    version: "1.0".to_owned(),
                },
                fastmcp_protocol::ClientCapabilities::default(),
                "2024-11-05".to_owned(),
            );
            let state_before = legacy_session.state().len();
            let legacy_ping = JsonRpcRequest::new("ping", Some(serde_json::json!({})), 714_i64);
            let mut final_ping = legacy_ping.clone();
            final_ping
                .params
                .as_mut()
                .expect("the cloned ping request retains its object parameters")
                .as_object_mut()
                .expect("the cloned ping parameters remain an object")
                .insert(
                    "_meta".to_owned(),
                    serde_json::json!({
                        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                        "io.modelcontextprotocol/clientCapabilities": {},
                    }),
                );
            let mut final_without_metadata = final_ping
                .params
                .clone()
                .expect("the final ping parameters are retained for the negative-control check");
            final_without_metadata
                .as_object_mut()
                .expect("the final ping parameters remain an object")
                .remove("_meta");
            assert_eq!(final_ping.method, legacy_ping.method);
            assert_eq!(final_ping.id, legacy_ping.id);
            assert_eq!(Some(final_without_metadata), legacy_ping.params);

            let notification_sender: crate::NotificationSender = Arc::new(|_| {});
            let request_sender = crate::RequestSender::new(
                Arc::new(crate::PendingRequests::new()),
                Arc::new(|message| {
                    Err(format!("unexpected outbound message in test: {message:?}"))
                }),
            );
            let legacy = fastmcp_core::block_on(server.dispatch_request(
                &Cx::for_testing(),
                &mut legacy_session,
                legacy_ping,
                &notification_sender,
                &request_sender,
            ))
            .expect("the exact-2024 public dispatch path responds to ping");
            assert_eq!(legacy.result, Some(serde_json::json!({})));
            assert!(legacy.error.is_none());

            let inbound = crate::InboundRequestContext::new(
                Cx::for_testing(),
                714,
                crate::InboundRequestTransport::Memory,
            );
            let answered = fastmcp_core::block_on(server.dispatch_stateless(&inbound, &final_ping))
                .expect("stateless modern ping is a connection health-check");
            assert_eq!(answered.result, Some(serde_json::json!({})));
            assert!(answered.error.is_none());
            assert_eq!(
                legacy_session.state().len(),
                state_before,
                "ping cannot mutate session state"
            );

            assert!(
                !fastmcp_protocol::methods::FINAL_2026_07_28_METHODS
                    .iter()
                    .any(|method| method.name == "ping"),
                "ping must remain outside the official 2026 client-request union"
            );
            let decode = fastmcp_protocol::CoreRequest::decode(
                ProtocolEra::Modern2026,
                "ping",
                Some(&serde_json::json!({})),
            )
            .expect_err("FinalCoreRequest must not grow a Ping variant");
            assert!(matches!(
                decode,
                fastmcp_protocol::CoreDispatchError::UnsupportedMethod {
                    era: ProtocolEra::Modern2026,
                    method,
                } if method == "ping"
            ));
        }

        #[test]
        fn typed_proxy_registration_retains_final_resource_template_and_prompt_metadata() {
            let resource = serde_json::json!({
                "uri": "mcp://static-upstream/resource",
                "name": "upstream-resource",
                "size": 4096,
                "_meta": {"com.example/resource": {"retained": true}}
            });
            let template = serde_json::json!({
                "uriTemplate": "mcp://upstream/{name}",
                "name": "upstream-template",
                "_meta": {"com.example/template": {"retained": true}}
            });
            let prompt = serde_json::json!({
                "name": "upstream-prompt",
                "arguments": [{"name": "region", "title": "Region"}],
                "_meta": {"com.example/prompt": {"retained": true}}
            });
            let typed = ProxyTypedCatalog {
                tools: ProxyToolCatalog::Final(ProxyFinalCatalog::new(Vec::new())),
                resources: ProxyResourceCatalog::Final(ProxyFinalCatalog::new(vec![
                    serde_json::from_value(resource.clone())
                        .expect("the final resource fixture is valid"),
                ])),
                resource_templates: ProxyResourceTemplateCatalog::Final(ProxyFinalCatalog::new(
                    vec![
                        serde_json::from_value(template.clone())
                            .expect("the final resource-template fixture is valid"),
                    ],
                )),
                prompts: ProxyPromptCatalog::Final(ProxyFinalCatalog::new(vec![
                    serde_json::from_value(prompt.clone())
                        .expect("the final prompt fixture is valid"),
                ])),
            };
            let server = ServerBuilder::new("srv", "1.0")
                .proxy_typed(
                    bound_proxy_client(DuplicatePolicyProxyBackend, ProtocolEra::Modern2026),
                    typed,
                )
                .expect("coherent typed catalog registers")
                .build();
            for (method, member, expected, id) in [
                ("resources/list", "resources", resource, 711_i64),
                (
                    "resources/templates/list",
                    "resourceTemplates",
                    template,
                    712_i64,
                ),
                ("prompts/list", "prompts", prompt, 713_i64),
            ] {
                let inbound = crate::InboundRequestContext::new(
                    Cx::for_testing(),
                    u64::try_from(id).expect("test request IDs are non-negative"),
                    crate::InboundRequestTransport::Memory,
                );
                let response = fastmcp_core::block_on(server.dispatch_stateless(
                    &inbound,
                    &JsonRpcRequest::new(
                        method,
                        Some(serde_json::json!({
                            "_meta": {
                                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                                "io.modelcontextprotocol/clientCapabilities": {},
                            },
                        })),
                        id,
                    ),
                ))
                .expect("final discovery dispatch succeeds");
                assert_eq!(
                    response.result.expect("final discovery has a result")[member][0],
                    expected,
                    "final proxy registration must retain exact {member} metadata"
                );
            }

            assert!(server.resources().is_empty());
            assert!(server.resource_templates().is_empty());
            assert!(server.prompts().is_empty());
            let router = server.into_router();
            let state = fastmcp_core::SessionState::new();
            let request_ctx =
                fastmcp_core::McpContext::with_state(Cx::for_testing(), 714, state.clone());
            let legacy_resource = fastmcp_core::block_on(router.handle_resources_read(
                &request_ctx,
                &fastmcp_protocol::ReadResourceParams {
                    uri: "mcp://upstream/resource".to_owned(),
                    meta: None,
                },
                state.clone(),
                None,
                None,
            ))
            .expect_err("typed-final resource registration is not legacy-visible");
            assert_eq!(
                legacy_resource.code,
                fastmcp_core::McpErrorCode::ResourceNotFound
            );
            let legacy_prompt = fastmcp_core::block_on(router.handle_prompts_get(
                &request_ctx,
                fastmcp_protocol::GetPromptParams {
                    name: "upstream-prompt".to_owned(),
                    arguments: None,
                    meta: None,
                },
                state,
                None,
                None,
            ))
            .expect_err("typed-final prompt registration is not legacy-visible");
            assert_eq!(
                legacy_prompt.code,
                fastmcp_core::McpErrorCode::PromptNotFound
            );
        }

        #[test]
        fn typed_proxy_registration_rejects_an_unbound_caller_asserted_final_catalog() {
            let error = match ServerBuilder::new("srv", "1.0").proxy_typed(
                ProxyClient::from_backend(DuplicatePolicyProxyBackend),
                final_completion_proxy_catalog(),
            ) {
                Ok(_) => {
                    panic!("a caller-supplied typed catalog cannot bind an unbound proxy route")
                }
                Err(error) => error,
            };

            assert_eq!(error.code, fastmcp_core::McpErrorCode::InvalidRequest);
            assert!(error.message.contains("cannot bind an unbound route"));
        }

        #[test]
        fn typed_proxy_registration_rejects_one_mixed_era_component_vector() {
            let typed = ProxyTypedCatalog {
                tools: ProxyToolCatalog::Legacy(Vec::new()),
                resources: ProxyResourceCatalog::Final(ProxyFinalCatalog::new(Vec::new())),
                resource_templates: ProxyResourceTemplateCatalog::Legacy(Vec::new()),
                prompts: ProxyPromptCatalog::Legacy(Vec::new()),
            };
            let error = match ServerBuilder::new("srv", "1.0").register_raw_typed_proxy_catalog(
                bound_proxy_client(DuplicatePolicyProxyBackend, ProtocolEra::Legacy2024),
                typed,
            ) {
                Err(error) => error,
                Ok(_) => {
                    panic!("changing only resources to final rejects a mixed-era proxy catalog")
                }
            };
            assert_eq!(error.code, fastmcp_core::McpErrorCode::InvalidRequest);
        }

        #[test]
        fn builder_proxy_rejects_the_same_final_catalog_for_a_legacy_binding() {
            let error = match ServerBuilder::new("srv", "1.0").proxy(
                final_catalog_proxy_client(ProtocolEra::Legacy2024),
                final_proxy_catalog(),
            ) {
                Ok(_) => panic!("the catalog era cannot contradict the immutable route binding"),
                Err(error) => error,
            };

            assert_eq!(error.code, fastmcp_core::McpErrorCode::InvalidRequest);
        }

        #[test]
        fn builder_proxy_with_catalog() {
            use crate::proxy::ProxyClient;

            struct DummyBackend;
            impl crate::proxy::ProxyBackend for DummyBackend {
                fn list_tools(&mut self) -> McpResult<Vec<Tool>> {
                    Ok(vec![Tool {
                        name: "proxy-tool".to_owned(),
                        description: None,
                        input_schema: serde_json::json!({}),
                        output_schema: None,
                        icon: None,
                        version: None,
                        tags: Vec::new(),
                        annotations: None,
                    }])
                }
                fn list_resources(&mut self) -> McpResult<Vec<Resource>> {
                    Ok(vec![])
                }
                fn list_resource_templates(&mut self) -> McpResult<Vec<ResourceTemplate>> {
                    Ok(vec![])
                }
                fn list_prompts(&mut self) -> McpResult<Vec<Prompt>> {
                    Ok(vec![])
                }
                fn call_tool(&mut self, _: &str, _: serde_json::Value) -> McpResult<Vec<Content>> {
                    Ok(vec![])
                }
                fn call_tool_with_progress(
                    &mut self,
                    _: &str,
                    _: serde_json::Value,
                    _: crate::proxy::ProgressCallback<'_>,
                ) -> McpResult<Vec<Content>> {
                    Ok(vec![])
                }
                fn read_resource(&mut self, _: &str) -> McpResult<Vec<ResourceContent>> {
                    Ok(vec![])
                }
                fn get_prompt(
                    &mut self,
                    _: &str,
                    _: std::collections::HashMap<String, String>,
                ) -> McpResult<Vec<fastmcp_protocol::PromptMessage>> {
                    Ok(vec![])
                }
            }

            let client = ProxyClient::from_backend(DummyBackend);
            let catalog = client
                .catalog()
                .expect("backend discovery supplies the legacy era evidence");

            let server = ServerBuilder::new("srv", "1.0")
                .proxy(client, catalog)
                .expect("the backend-observed catalog is admitted")
                .build();
            assert!(server.has_tools());
        }

        #[test]
        fn builder_proxy_honors_duplicate_policy_for_every_component_kind() {
            for behavior in [
                DuplicateBehavior::Warn,
                DuplicateBehavior::Ignore,
                DuplicateBehavior::Error,
            ] {
                let (client, catalog) = discovered_duplicate_policy_proxy();
                let server = ServerBuilder::new("srv", "1.0")
                    .on_duplicate(behavior)
                    .tool(TestTool)
                    .resource(TestResource)
                    .prompt(TestPrompt)
                    .proxy(client, catalog)
                    .expect("the backend-observed catalog is admitted")
                    .build();
                let router = server.into_router();

                assert_eq!(
                    router.tools()[0].description.as_deref(),
                    Some("a test tool")
                );
                assert_eq!(router.resources()[0].name, "test_res");
                assert_eq!(router.prompts()[0].description, None);
            }

            let (client, catalog) = discovered_duplicate_policy_proxy();
            let replaced = ServerBuilder::new("srv", "1.0")
                .on_duplicate(DuplicateBehavior::Replace)
                .tool(TestTool)
                .resource(TestResource)
                .prompt(TestPrompt)
                .proxy(client, catalog)
                .expect("the backend-observed catalog is admitted")
                .build()
                .into_router();

            assert_eq!(
                replaced.tools()[0].description.as_deref(),
                Some("proxied tool")
            );
            assert_eq!(replaced.resources()[0].name, "proxied resource");
            assert_eq!(
                replaced.prompts()[0].description.as_deref(),
                Some("proxied prompt")
            );
        }

        #[cfg(feature = "tasks")]
        #[test]
        fn as_proxy_typed_installs_route_bound_final_tasks_relay() {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let updates = Arc::new(Mutex::new(Vec::new()));
            let server = ServerBuilder::new("prefixed-proxy-tasks", "1.0")
                .as_proxy_typed(
                    "ext",
                    ordinary_proxy_tasks_client(calls, updates),
                    final_completion_proxy_catalog(),
                )
                .expect("as_proxy_typed admits a modern catalog with a Tasks-capable route")
                .build();
            assert!(
                server.final_task_runtime().is_none(),
                "as_proxy_typed must install the route-bound Tasks relay instead of the default in-memory store"
            );
            let discovery = serde_json::to_value(
                server
                    .server_discovery()
                    .expect("the prefixed Tasks proxy remains discoverable"),
            )
            .expect("prefixed Tasks proxy discovery serializes");
            assert_eq!(
                discovery.pointer("/capabilities/extensions/io.modelcontextprotocol~1tasks"),
                Some(&serde_json::json!({})),
                "as_proxy_typed must advertise the same Tasks extension as proxy_typed"
            );
        }

        #[test]
        fn as_proxy_raw_propagates_duplicate_registration_errors() {
            let result = ServerBuilder::new("srv", "1.0")
                .on_duplicate(DuplicateBehavior::Error)
                .tool(TestTool)
                .into_proxy_raw_with_proxy_client(ProxyClient::from_backend(
                    DuplicatePolicyProxyBackend,
                ));

            let error = match result {
                Ok(_) => panic!("raw proxy registration unexpectedly accepted a duplicate tool"),
                Err(error) => error,
            };
            assert_eq!(error.code, fastmcp_core::McpErrorCode::InvalidRequest);
            assert!(error.message.starts_with("Tool already exists"));
        }
    }

    // ── DEFAULT_REQUEST_TIMEOUT_SECS constant ──────────────────────

    #[test]
    fn default_request_timeout_constant() {
        assert_eq!(DEFAULT_REQUEST_TIMEOUT_SECS, 30);
    }

    // ── mask_error_details toggling ────────────────────────────────

    #[test]
    fn builder_mask_error_details_toggle() {
        let builder = ServerBuilder::new("srv", "1.0")
            .mask_error_details(true)
            .mask_error_details(false);
        assert!(!builder.is_error_masking_enabled());
    }

    // ── strict_input_validation toggling ────────────────────────────

    #[test]
    fn builder_strict_validation_toggle() {
        let builder = ServerBuilder::new("srv", "1.0")
            .strict_input_validation(true)
            .strict_input_validation(false);
        assert!(!builder.is_strict_input_validation_enabled());
    }

    #[cfg(feature = "tasks")]
    mod task_manager_tests {
        use super::*;

        // ── Task manager ──────────────────────────────────────────────────

        #[test]
        fn default_build_installs_in_memory_official_tasks() {
            let server = ServerBuilder::new("srv", "1.0").build();
            assert!(
                server.final_task_runtime().is_some(),
                "default build must install official in-memory Tasks"
            );
        }

        #[test]
        fn builder_with_task_manager_retains_manager_without_advertising_capability() {
            use crate::tasks::TaskManager;
            let tm = TaskManager::new().into_shared();
            let server = ServerBuilder::new("srv", "1.0")
                .with_task_manager(tm)
                .build();
            assert!(server.task_manager().is_some());
            assert!(server.capabilities().tasks.is_none());
            assert!(
                server.final_task_runtime().is_none(),
                "quarantined task manager must not receive official Tasks"
            );
        }

        #[test]
        fn builder_with_notifying_task_manager_keeps_capability_quarantined() {
            use crate::tasks::TaskManager;
            let tm = TaskManager::with_list_changed_notifications().into_shared();
            let server = ServerBuilder::new("srv", "1.0")
                .with_task_manager(tm)
                .build();
            assert!(server.task_manager().is_some());
            assert!(server.capabilities().tasks.is_none());
        }

        #[test]
        fn builder_with_non_notifying_task_manager_keeps_capability_quarantined() {
            use crate::tasks::TaskManager;
            let tm = TaskManager::new().into_shared();
            let server = ServerBuilder::new("srv", "1.0")
                .with_task_manager(tm)
                .build();
            assert!(server.task_manager().is_some());
            assert!(server.capabilities().tasks.is_none());
        }
    }

    // ── Duplicate behavior for resources and prompts ─────────────────

    struct DupResource(&'static str);
    impl crate::ResourceHandler for DupResource {
        fn definition(&self) -> Resource {
            Resource {
                uri: format!("file:///{}", self.0),
                name: self.0.to_string(),
                description: None,
                mime_type: None,
                icon: None,
                version: None,
                tags: vec![],
            }
        }
        fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
            Ok(vec![])
        }
    }

    struct DupPrompt(&'static str);
    impl crate::PromptHandler for DupPrompt {
        fn definition(&self) -> Prompt {
            Prompt {
                name: self.0.to_string(),
                description: None,
                arguments: vec![],
                icon: None,
                version: None,
                tags: vec![],
            }
        }
        fn get(
            &self,
            _ctx: &McpContext,
            _args: std::collections::HashMap<String, String>,
        ) -> McpResult<Vec<fastmcp_protocol::PromptMessage>> {
            Ok(vec![])
        }
    }

    #[test]
    fn builder_on_duplicate_error_resource_refuses_the_build() {
        let refused = refused_registrations(
            ServerBuilder::new("srv", "1.0")
                .on_duplicate(DuplicateBehavior::Error)
                .resource(DupResource("dup"))
                .resource(DupResource("dup")),
        );
        assert_eq!(refused.len(), 1);
        assert_eq!(
            (refused[0].kind, refused[0].name.as_str()),
            (RegistrationKind::Resource, "file:///dup")
        );
    }

    #[test]
    fn builder_on_duplicate_error_prompt_refuses_the_build() {
        let refused = refused_registrations(
            ServerBuilder::new("srv", "1.0")
                .on_duplicate(DuplicateBehavior::Error)
                .prompt(DupPrompt("dup"))
                .prompt(DupPrompt("dup")),
        );
        assert_eq!(refused.len(), 1);
        assert_eq!(
            (refused[0].kind, refused[0].name.as_str()),
            (RegistrationKind::Prompt, "dup")
        );
    }

    #[cfg(feature = "proxy")]
    #[test]
    fn prefixed_as_proxy_skips_only_duplicate_registration_errors() {
        let duplicate =
            fastmcp_core::McpError::invalid_request("Tool already exists; component_key=tool_key");
        assert!(
            ServerBuilder::absorb_prefixed_proxy_duplicate(duplicate).is_ok(),
            "a colliding catalog member may skip under DuplicateBehavior::Error"
        );
        let panicked =
            fastmcp_core::McpError::internal_error("tool metadata hook panicked during admission");
        assert!(
            ServerBuilder::absorb_prefixed_proxy_duplicate(panicked).is_err(),
            "a non-duplicate admission failure must fail the proxy / as_proxy_typed install"
        );
        let apps = fastmcp_core::McpError::invalid_request(
            "MCP Apps tool metadata requires ServerBuilder::mcp_apps_tool",
        );
        assert!(
            ServerBuilder::absorb_prefixed_proxy_duplicate(apps).is_err(),
            "InvalidRequest that is not a duplicate must not be skipped"
        );
    }

    // ── Proxy with resources and prompts ─────────────────────────────

    #[cfg(feature = "proxy")]
    #[test]
    fn builder_proxy_with_resources_and_prompts() {
        use crate::proxy::ProxyClient;

        struct DummyBackend2;
        impl crate::proxy::ProxyBackend for DummyBackend2 {
            fn list_tools(&mut self) -> McpResult<Vec<Tool>> {
                Ok(vec![])
            }
            fn list_resources(&mut self) -> McpResult<Vec<Resource>> {
                Ok(vec![Resource {
                    uri: "file:///proxy-res".to_owned(),
                    name: "proxy-res".to_owned(),
                    description: None,
                    mime_type: None,
                    icon: None,
                    version: None,
                    tags: Vec::new(),
                }])
            }
            fn list_resource_templates(&mut self) -> McpResult<Vec<ResourceTemplate>> {
                Ok(vec![ResourceTemplate {
                    uri_template: "db://{table}".to_owned(),
                    name: "db".to_owned(),
                    description: None,
                    mime_type: None,
                    icon: None,
                    version: None,
                    tags: Vec::new(),
                }])
            }
            fn list_prompts(&mut self) -> McpResult<Vec<Prompt>> {
                Ok(vec![Prompt {
                    name: "proxy-prompt".to_owned(),
                    description: None,
                    arguments: Vec::new(),
                    icon: None,
                    version: None,
                    tags: Vec::new(),
                }])
            }
            fn call_tool(&mut self, _: &str, _: serde_json::Value) -> McpResult<Vec<Content>> {
                Ok(vec![])
            }
            fn call_tool_with_progress(
                &mut self,
                _: &str,
                _: serde_json::Value,
                _: crate::proxy::ProgressCallback<'_>,
            ) -> McpResult<Vec<Content>> {
                Ok(vec![])
            }
            fn read_resource(&mut self, _: &str) -> McpResult<Vec<ResourceContent>> {
                Ok(vec![])
            }
            fn get_prompt(
                &mut self,
                _: &str,
                _: std::collections::HashMap<String, String>,
            ) -> McpResult<Vec<fastmcp_protocol::PromptMessage>> {
                Ok(vec![])
            }
        }

        let client = ProxyClient::from_backend(DummyBackend2);
        let catalog = client
            .catalog()
            .expect("backend discovery supplies the legacy era evidence");

        let server = ServerBuilder::new("srv", "1.0")
            .proxy(client, catalog)
            .expect("the backend-observed catalog is admitted")
            .build();
        assert!(server.has_resources());
        assert!(server.has_prompts());
        assert!(!server.has_tools());
    }

    // ── Build propagates strict validation to router ─────────────────

    #[test]
    fn build_propagates_strict_validation_to_router() {
        let server = ServerBuilder::new("srv", "1.0")
            .strict_input_validation(true)
            .build();
        let router = server.into_router();
        assert!(router.strict_input_validation());
    }

    #[test]
    fn build_propagates_strict_validation_false_to_router() {
        let server = ServerBuilder::new("srv", "1.0")
            .strict_input_validation(false)
            .build();
        let router = server.into_router();
        assert!(!router.strict_input_validation());
    }

    // ── log_level_filter with Off ────────────────────────────────────

    #[test]
    fn builder_log_level_filter_off() {
        let builder = ServerBuilder::new("srv", "1.0").log_level_filter(LevelFilter::Off);
        assert_eq!(builder.logging.level, LevelFilter::Off);
        assert_eq!(builder.console_config.log_level, LevelFilter::Off);
    }

    // ── mount does not update capabilities when nothing mounted ──────

    #[test]
    fn builder_mount_no_op_leaves_capabilities_unchanged() {
        let source = ServerBuilder::new("sub", "1.0").build();
        let main = ServerBuilder::new("main", "1.0")
            .mount(source, Some("ns"))
            .build();
        assert!(!main.has_tools());
        assert!(!main.has_resources());
        assert!(!main.has_prompts());
    }
}
