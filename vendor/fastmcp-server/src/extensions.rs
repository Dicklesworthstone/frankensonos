//! Typed server handlers for negotiated MCP extensions.
//!
//! This registry deliberately owns neither JSON-RPC decoding nor server/router
//! dispatch. It binds a typed handler to an extension capability and request
//! method, then delegates current-exchange admission to the frozen protocol
//! descriptor registry before invoking that handler. Router integration owns
//! the wire boundary separately.

use std::collections::BTreeMap;
use std::fmt;
use std::marker::PhantomData;

use crate::BoxFuture;

use fastmcp_core::{McpContext, McpError, McpResult};
use fastmcp_protocol::extensions::{
    ExtensionDispatchError, ExtensionRegistryError, MAX_EXTENSION_MEMBER_NAME_BYTES,
    NegotiatedExtensionSet, ServerExtensionDiscovery,
};
#[cfg(feature = "apps")]
use fastmcp_protocol::extensions::{
    official_mcp_apps_empty_server_settings, register_official_mcp_apps_extension,
    validate_official_mcp_apps_server_settings,
};
use fastmcp_protocol::protocol_policy::ProtocolEra;
use fastmcp_protocol::{
    ExtensionDescriptorRegistry, ExtensionDirection, ExtensionId, ExtensionRegistryReceipt,
    ExtensionSettings, JsonRpcRequest,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

/// A typed implementation for one client-to-server extension request.
///
/// The supplied [`McpContext`] is the caller's request context. Implementations
/// must preserve its cancellation and budget ownership rather than creating a
/// detached runtime context.
pub trait ExtensionHandler<Request, Response>: Send + Sync {
    /// Handles one already-admitted extension request.
    fn handle(&self, context: &McpContext, request: Request) -> McpResult<Response>;
}

#[cfg(all(test, feature = "apps"))]
mod apps_only_tests {
    use fastmcp_core::{McpContext, McpResult};
    use fastmcp_protocol::extensions::official_mcp_apps_empty_server_settings;
    use fastmcp_protocol::{
        ExtensionDescriptorRegistry, ExtensionRegistryError, ExtensionSettings,
    };
    use serde_json::json;

    use super::{ExtensionHandlerKey, ExtensionHandlerRegistrationError, ExtensionHandlerRegistry};

    #[test]
    fn official_apps_rejects_unowned_client_to_server_handler_without_mutation() {
        let mut handlers = ExtensionHandlerRegistry::new(ExtensionDescriptorRegistry::new());
        let apps_id = handlers
            .install_official_mcp_apps()
            .expect("the official Apps descriptor and marker install");
        let key = ExtensionHandlerKey::new(
            apps_id.clone(),
            fastmcp_protocol::extensions::MCP_APPS_INITIALIZE_METHOD,
        );

        assert_eq!(
            handlers.register(
                apps_id.clone(),
                fastmcp_protocol::extensions::MCP_APPS_INITIALIZE_METHOD,
                |_context: &McpContext,
                 _params: serde_json::Value|
                 -> McpResult<serde_json::Value> { Ok(json!({})) },
            ),
            Err(ExtensionHandlerRegistrationError::MethodNotOwned(key)),
            "MCP Apps owns no client-to-server extension method on this server registry"
        );
        assert_eq!(
            handlers.len(),
            0,
            "the rejected Apps handler cannot create a dead dispatch entry"
        );
        assert_eq!(
            handlers.server_metadata_len(),
            1,
            "the rejection preserves the installed Apps discovery marker"
        );
        assert_eq!(
            handlers.descriptor_registry().descriptor(&apps_id),
            Some(&fastmcp_protocol::official_mcp_apps_descriptor()),
            "the rejected handler cannot alter the official Apps descriptor"
        );
    }

    #[test]
    fn official_apps_installation_duplicate_is_rejected_without_mutating_metadata() {
        let mut handlers = ExtensionHandlerRegistry::new(ExtensionDescriptorRegistry::new());
        let apps_id = handlers
            .install_official_mcp_apps()
            .expect("baseline official Apps installation succeeds");

        assert_eq!(
            handlers
                .install_official_mcp_apps()
                .expect_err("only the duplicate installation is rejected"),
            ExtensionHandlerRegistrationError::OfficialMcpAppsAlreadyInstalled
        );
        assert_eq!(handlers.server_metadata_len(), 1);
        assert_eq!(
            handlers.descriptor_registry().descriptor(&apps_id),
            Some(&fastmcp_protocol::official_mcp_apps_descriptor())
        );
    }

    #[test]
    fn manually_registered_official_apps_metadata_requires_the_empty_marker() {
        let mut descriptors = ExtensionDescriptorRegistry::new();
        let apps_id = fastmcp_protocol::register_official_mcp_apps_extension(&mut descriptors)
            .expect("the official Apps descriptor registers");
        let mut handlers = ExtensionHandlerRegistry::new(descriptors);
        let rejected = ExtensionSettings::new(json!({ "unexpected": true }))
            .expect("the one-field alternate is generic extension metadata");

        assert_eq!(
            handlers.register_server_metadata(apps_id.clone(), rejected),
            Err(ExtensionHandlerRegistrationError::Registry(
                ExtensionRegistryError::OfficialMcpAppsServerSettingsNotEmpty
            )),
            "only the non-empty official Apps marker is rejected"
        );
        assert_eq!(
            handlers.server_metadata_len(),
            0,
            "the rejected marker cannot be retained for later builder composition"
        );

        handlers
            .register_server_metadata(apps_id, official_mcp_apps_empty_server_settings())
            .expect("the exact empty official Apps marker remains accepted");
        assert_eq!(handlers.server_metadata_len(), 1);
    }
}

impl<Request, Response, Handler> ExtensionHandler<Request, Response> for Handler
where
    Handler: Fn(&McpContext, Request) -> McpResult<Response> + Send + Sync,
{
    fn handle(&self, context: &McpContext, request: Request) -> McpResult<Response> {
        self(context, request)
    }
}

/// Object-safe bridge that retains each registered handler's Rust types.
trait ErasedExtensionHandler: Send + Sync {
    fn invoke(&self, context: &McpContext, parameters: Value) -> McpResult<Value>;

    fn invoke_async<'a>(
        &'a self,
        context: &'a McpContext,
        parameters: Value,
    ) -> BoxFuture<'a, McpResult<Value>> {
        Box::pin(async move { self.invoke(context, parameters) })
    }
}

/// Typed async callback retained without constructing an executor or holding
/// registry mutation state while the callback is suspended.
struct AsyncSerdeExtensionHandler<Request, Response, Handler> {
    handler: Handler,
    marker: PhantomData<fn(Request) -> Response>,
}

impl<Request, Response, Handler> ErasedExtensionHandler
    for AsyncSerdeExtensionHandler<Request, Response, Handler>
where
    Request: DeserializeOwned + Send,
    Response: Serialize,
    Handler:
        for<'a> Fn(&'a McpContext, Request) -> BoxFuture<'a, McpResult<Response>> + Send + Sync,
{
    fn invoke(&self, _context: &McpContext, _parameters: Value) -> McpResult<Value> {
        Err(McpError::invalid_request(
            "this extension handler requires asynchronous request dispatch",
        ))
    }

    fn invoke_async<'a>(
        &'a self,
        context: &'a McpContext,
        parameters: Value,
    ) -> BoxFuture<'a, McpResult<Value>> {
        Box::pin(async move {
            let request = serde_json::from_value(parameters)
                .map_err(|error| McpError::invalid_params(error.to_string()))?;
            let response = (self.handler)(context, request).await?;
            serde_json::to_value(response).map_err(|_| {
                McpError::internal_error("typed extension handler response serialization failed")
            })
        })
    }
}

/// Typed serde adapter retained behind the heterogeneous handler map.
struct SerdeExtensionHandler<Request, Response, Handler> {
    handler: Handler,
    marker: PhantomData<fn(Request) -> Response>,
}

impl<Request, Response, Handler> ErasedExtensionHandler
    for SerdeExtensionHandler<Request, Response, Handler>
where
    Request: DeserializeOwned,
    Response: Serialize,
    Handler: ExtensionHandler<Request, Response>,
{
    fn invoke(&self, context: &McpContext, parameters: Value) -> McpResult<Value> {
        let request = serde_json::from_value(parameters)
            .map_err(|error| McpError::invalid_params(error.to_string()))?;
        let response = self.handler.handle(context, request)?;
        serde_json::to_value(response).map_err(|_| {
            McpError::internal_error("typed extension handler response serialization failed")
        })
    }
}

/// A stable server-side location for one extension request handler.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ExtensionHandlerKey {
    extension_id: ExtensionId,
    method: String,
}

impl ExtensionHandlerKey {
    /// Creates a handler location from its capability and exact request method.
    #[must_use]
    pub fn new(extension_id: ExtensionId, method: impl Into<String>) -> Self {
        Self {
            extension_id,
            method: method.into(),
        }
    }

    /// Returns the owning extension capability.
    #[must_use]
    pub const fn extension_id(&self) -> &ExtensionId {
        &self.extension_id
    }

    /// Returns the exact client-to-server method spelling.
    #[must_use]
    pub fn method(&self) -> &str {
        &self.method
    }
}

/// Failure while registering a typed extension handler.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExtensionHandlerRegistrationError {
    /// Handler mutation was attempted after the registry was frozen.
    Frozen,
    /// The handler's capability is absent from the protocol descriptor registry.
    UnregisteredExtension(String),
    /// The handler method is empty and cannot name an RPC request.
    EmptyMethodName,
    /// The handler method exceeds the protocol's bounded member-name limit.
    MethodNameTooLong(String),
    /// The extension descriptor does not own the requested method.
    MethodNotOwned(ExtensionHandlerKey),
    /// The extension descriptor owns the method only in the server-to-client direction.
    MethodNotClientToServer(ExtensionHandlerKey),
    /// A handler is already registered for this exact extension request location.
    DuplicateHandler(ExtensionHandlerKey),
    /// Server discovery metadata is already registered for this extension.
    DuplicateServerMetadata(ExtensionId),
    /// The official MCP Apps descriptor and server marker are already installed.
    #[cfg(feature = "apps")]
    OfficialMcpAppsAlreadyInstalled,
    /// The protocol registry rejected official MCP Apps descriptor installation.
    Registry(ExtensionRegistryError),
}

impl fmt::Display for ExtensionHandlerRegistrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Frozen => formatter.write_str("extension handler registry is frozen"),
            Self::UnregisteredExtension(id) => {
                write!(formatter, "extension handler has no descriptor: {id}")
            }
            Self::EmptyMethodName => formatter.write_str("extension handler method is empty"),
            Self::MethodNameTooLong(method) => {
                write!(
                    formatter,
                    "extension handler method exceeds its byte limit: {method}"
                )
            }
            Self::MethodNotOwned(key) => write!(
                formatter,
                "extension handler method is not owned by its descriptor: {}/{}",
                key.extension_id(),
                key.method()
            ),
            Self::MethodNotClientToServer(key) => write!(
                formatter,
                "extension handler method is not client-to-server: {}/{}",
                key.extension_id(),
                key.method()
            ),
            Self::DuplicateHandler(key) => write!(
                formatter,
                "extension handler is already registered: {}/{}",
                key.extension_id(),
                key.method()
            ),
            Self::DuplicateServerMetadata(id) => {
                write!(
                    formatter,
                    "extension server metadata is already registered: {id}"
                )
            }
            #[cfg(feature = "apps")]
            Self::OfficialMcpAppsAlreadyInstalled => {
                formatter.write_str("official MCP Apps extension is already installed")
            }
            Self::Registry(error) => {
                write!(formatter, "extension registry rejected install: {error}")
            }
        }
    }
}

impl std::error::Error for ExtensionHandlerRegistrationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Registry(error) => Some(error),
            Self::Frozen
            | Self::UnregisteredExtension(_)
            | Self::EmptyMethodName
            | Self::MethodNameTooLong(_)
            | Self::MethodNotOwned(_)
            | Self::MethodNotClientToServer(_)
            | Self::DuplicateHandler(_)
            | Self::DuplicateServerMetadata(_) => None,
            #[cfg(feature = "apps")]
            Self::OfficialMcpAppsAlreadyInstalled => None,
        }
    }
}

/// Failure while looking up a registered extension handler.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExtensionHandlerLookupError {
    /// Lookup is unavailable until the capability and handler registries are frozen together.
    RegistryNotFrozen,
    /// No typed handler is registered for the admitted capability and request method.
    HandlerNotFound(ExtensionHandlerKey),
}

impl fmt::Display for ExtensionHandlerLookupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RegistryNotFrozen => {
                formatter.write_str("extension handler registry must be frozen before lookup")
            }
            Self::HandlerNotFound(key) => write!(
                formatter,
                "no extension handler is registered: {}/{}",
                key.extension_id(),
                key.method()
            ),
        }
    }
}

impl std::error::Error for ExtensionHandlerLookupError {}

/// Failure while admitting or invoking a typed extension handler.
#[derive(Debug)]
pub enum ExtensionHandlerInvocationError {
    /// Invocation is unavailable until the capability and handler registries are frozen together.
    RegistryNotFrozen,
    /// The current exchange did not admit this capability and client-to-server request method.
    Protocol(ExtensionDispatchError),
    /// No handler was registered for an otherwise admitted extension request method.
    HandlerNotFound(ExtensionHandlerKey),
    /// A descriptor-owned extension request was sent as an id-less notification.
    RequestEnvelopeRequired(ExtensionHandlerKey),
    /// The admitted typed handler returned an MCP error.
    Handler(McpError),
}

impl fmt::Display for ExtensionHandlerInvocationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RegistryNotFrozen => {
                formatter.write_str("extension handler registry must be frozen before invocation")
            }
            Self::Protocol(error) => {
                write!(formatter, "extension request was not admitted: {error}")
            }
            Self::HandlerNotFound(key) => write!(
                formatter,
                "no extension handler is registered: {}/{}",
                key.extension_id(),
                key.method()
            ),
            Self::RequestEnvelopeRequired(key) => write!(
                formatter,
                "extension request requires a JSON-RPC request id: {}/{}",
                key.extension_id(),
                key.method()
            ),
            Self::Handler(error) => write!(formatter, "extension handler failed: {error}"),
        }
    }
}

impl std::error::Error for ExtensionHandlerInvocationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Protocol(error) => Some(error),
            Self::Handler(error) => Some(error),
            Self::RegistryNotFrozen
            | Self::HandlerNotFound(_)
            | Self::RequestEnvelopeRequired(_) => None,
        }
    }
}

/// Frozen, typed server handlers bound to one protocol descriptor registry.
///
/// The map is erased only internally. Each [`Self::register`] call fixes that
/// method's own decoded request and encoded response types in a serde adapter,
/// so one registry can hold heterogeneous extension methods without weakening
/// a handler's typed Rust boundary. Method ownership, direction, negotiated
/// capability activation, and descriptor receipt are still enforced by the
/// protocol registry at every invocation.
pub struct ExtensionHandlerRegistry {
    descriptor_registry: ExtensionDescriptorRegistry,
    handlers: BTreeMap<ExtensionHandlerKey, Box<dyn ErasedExtensionHandler>>,
    server_metadata: BTreeMap<ExtensionId, ExtensionSettings>,
    frozen: bool,
}

impl ExtensionHandlerRegistry {
    /// Starts a handler registry around the supplied protocol descriptor registry.
    #[must_use]
    pub fn new(descriptor_registry: ExtensionDescriptorRegistry) -> Self {
        Self {
            descriptor_registry,
            handlers: BTreeMap::new(),
            server_metadata: BTreeMap::new(),
            frozen: false,
        }
    }

    /// Returns the protocol descriptor registry that governs handler admission.
    #[must_use]
    pub const fn descriptor_registry(&self) -> &ExtensionDescriptorRegistry {
        &self.descriptor_registry
    }

    /// Returns the mutable descriptor registry before handler freeze.
    ///
    /// Builder-only composition must add every descriptor before typed handlers
    /// are frozen under their shared receipt. Once frozen, neither descriptor
    /// nor handler mutation is admitted.
    pub(crate) fn descriptor_registry_mut(
        &mut self,
    ) -> Result<&mut ExtensionDescriptorRegistry, ExtensionHandlerRegistrationError> {
        if self.frozen {
            return Err(ExtensionHandlerRegistrationError::Frozen);
        }
        Ok(&mut self.descriptor_registry)
    }

    /// Returns whether handler registration has been frozen.
    #[must_use]
    pub const fn is_frozen(&self) -> bool {
        self.frozen
    }

    /// Returns the number of registered typed handlers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.handlers.len()
    }

    /// Returns whether no typed handlers have been registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.handlers.is_empty()
    }

    /// Returns the number of descriptor-bound server metadata entries.
    #[must_use]
    pub fn server_metadata_len(&self) -> usize {
        self.server_metadata.len()
    }

    /// Returns server metadata registered alongside descriptors before freeze.
    ///
    /// The live server runtime merges this snapshot with caller-supplied
    /// discovery before it derives local extension enablement. The public
    /// [`Self::server_discovery`] surface remains frozen-only.
    pub(crate) fn configured_server_discovery(&self) -> ServerExtensionDiscovery {
        ServerExtensionDiscovery {
            extensions: self.server_metadata.clone(),
        }
    }

    /// Registers the exact server discovery settings for one extension.
    ///
    /// This is intentionally separate from request-handler registration: an
    /// extension such as MCP Apps can need a server discovery marker before it
    /// owns a client-to-server extension method. The marker is accepted only
    /// for a descriptor already linked into this registry, and is immutable
    /// once the descriptor receipt is frozen.
    pub fn register_server_metadata(
        &mut self,
        extension_id: ExtensionId,
        settings: ExtensionSettings,
    ) -> Result<(), ExtensionHandlerRegistrationError> {
        if self.frozen {
            return Err(ExtensionHandlerRegistrationError::Frozen);
        }
        if self.descriptor_registry.descriptor(&extension_id).is_none() {
            return Err(ExtensionHandlerRegistrationError::UnregisteredExtension(
                extension_id.to_string(),
            ));
        }
        #[cfg(feature = "apps")]
        if extension_id == fastmcp_protocol::official_mcp_apps_extension_id() {
            validate_official_mcp_apps_server_settings(&settings)
                .map_err(ExtensionHandlerRegistrationError::Registry)?;
        }
        if self.server_metadata.contains_key(&extension_id) {
            return Err(ExtensionHandlerRegistrationError::DuplicateServerMetadata(
                extension_id,
            ));
        }

        self.server_metadata.insert(extension_id, settings);
        Ok(())
    }

    /// Installs the official MCP Apps descriptor and its exact empty server marker.
    ///
    /// MCP Apps has no client-to-server extension method, so its server-side
    /// installation consists solely of the protocol descriptor and discovery
    /// metadata. The resolver remains caller-owned and must preserve its
    /// `ExtensionSettingsCompatibilityResolver::resolve_with_disposition`
    /// result when this registry is installed into a live server runtime.
    #[cfg(feature = "apps")]
    pub fn install_official_mcp_apps(
        &mut self,
    ) -> Result<ExtensionId, ExtensionHandlerRegistrationError> {
        if self.frozen {
            return Err(ExtensionHandlerRegistrationError::Frozen);
        }
        let id = fastmcp_protocol::official_mcp_apps_extension_id();
        if self.descriptor_registry.descriptor(&id).is_some()
            || self.server_metadata.contains_key(&id)
        {
            return Err(ExtensionHandlerRegistrationError::OfficialMcpAppsAlreadyInstalled);
        }
        register_official_mcp_apps_extension(&mut self.descriptor_registry)
            .map_err(ExtensionHandlerRegistrationError::Registry)?;
        self.register_server_metadata(id.clone(), official_mcp_apps_empty_server_settings())?;
        Ok(id)
    }

    /// Registers one typed handler for an extension request method.
    ///
    /// Descriptor existence, exact method ownership, and client-to-server
    /// direction are checked immediately. [`Self::invoke`] still checks
    /// request-specific admission against the current negotiated extension set
    /// before the handler can run.
    pub fn register<Request, Response, Handler>(
        &mut self,
        extension_id: ExtensionId,
        method: impl Into<String>,
        handler: Handler,
    ) -> Result<(), ExtensionHandlerRegistrationError>
    where
        Request: DeserializeOwned + 'static,
        Response: Serialize + 'static,
        Handler: ExtensionHandler<Request, Response> + 'static,
    {
        self.register_handler(
            extension_id,
            method.into(),
            Box::new(SerdeExtensionHandler::<Request, Response, Handler> {
                handler,
                marker: PhantomData,
            }),
        )
    }

    /// Registers a typed asynchronous handler polled on the caller's context.
    ///
    /// Invoke it through [`Self::invoke_async`] or an owned server transport.
    /// The synchronous invocation API rejects it before executing the callback.
    pub fn register_async<Request, Response, Handler>(
        &mut self,
        extension_id: ExtensionId,
        method: impl Into<String>,
        handler: Handler,
    ) -> Result<(), ExtensionHandlerRegistrationError>
    where
        Request: DeserializeOwned + Send + 'static,
        Response: Serialize + 'static,
        Handler: for<'a> Fn(&'a McpContext, Request) -> BoxFuture<'a, McpResult<Response>>
            + Send
            + Sync
            + 'static,
    {
        self.register_handler(
            extension_id,
            method.into(),
            Box::new(AsyncSerdeExtensionHandler::<Request, Response, Handler> {
                handler,
                marker: PhantomData,
            }),
        )
    }

    fn register_handler(
        &mut self,
        extension_id: ExtensionId,
        method: String,
        handler: Box<dyn ErasedExtensionHandler>,
    ) -> Result<(), ExtensionHandlerRegistrationError> {
        if self.frozen {
            return Err(ExtensionHandlerRegistrationError::Frozen);
        }
        if self.descriptor_registry.descriptor(&extension_id).is_none() {
            return Err(ExtensionHandlerRegistrationError::UnregisteredExtension(
                extension_id.to_string(),
            ));
        }

        let key = ExtensionHandlerKey::new(extension_id, method);
        if key.method().is_empty() {
            return Err(ExtensionHandlerRegistrationError::EmptyMethodName);
        }
        if key.method().len() > MAX_EXTENSION_MEMBER_NAME_BYTES {
            return Err(ExtensionHandlerRegistrationError::MethodNameTooLong(
                key.method().to_owned(),
            ));
        }
        let Some(method_descriptor) = self
            .descriptor_registry
            .method_descriptor(key.extension_id(), key.method())
        else {
            return Err(ExtensionHandlerRegistrationError::MethodNotOwned(key));
        };
        if method_descriptor.direction != ExtensionDirection::ClientToServer {
            return Err(ExtensionHandlerRegistrationError::MethodNotClientToServer(
                key,
            ));
        }
        if self.handlers.contains_key(&key) {
            return Err(ExtensionHandlerRegistrationError::DuplicateHandler(key));
        }

        self.handlers.insert(key, handler);
        Ok(())
    }

    /// Freezes protocol descriptors and typed handlers under one immutable receipt.
    pub fn freeze(&mut self) -> Result<ExtensionRegistryReceipt, ExtensionRegistryError> {
        let receipt = self.descriptor_registry.freeze()?;
        self.frozen = true;
        Ok(receipt)
    }

    /// Freezes this registry and returns its matching discovery metadata.
    ///
    /// Use this after extension composition is complete to inspect one frozen
    /// descriptor receipt with its matching metadata. During builder
    /// composition, [`ServerBuilder::extension_registry`](crate::ServerBuilder::extension_registry)
    /// derives registered metadata directly so later official Tasks installation
    /// remains mutable.
    pub fn freeze_with_server_discovery(
        &mut self,
    ) -> Result<(ExtensionRegistryReceipt, ServerExtensionDiscovery), ExtensionRegistryError> {
        let receipt = self.freeze()?;
        let discovery = self
            .server_discovery()
            .expect("registry is frozen immediately after a successful freeze");
        Ok((receipt, discovery))
    }

    /// Returns the server discovery data bound to this frozen registry.
    ///
    /// This frozen diagnostic/export surface is separate from builder-time
    /// composition, which snapshots registered metadata before freeze.
    pub fn server_discovery(
        &self,
    ) -> Result<ServerExtensionDiscovery, ExtensionHandlerLookupError> {
        if !self.frozen {
            return Err(ExtensionHandlerLookupError::RegistryNotFrozen);
        }
        Ok(ServerExtensionDiscovery {
            extensions: self.server_metadata.clone(),
        })
    }

    /// Looks up one handler after freezing has made the registry immutable.
    pub fn lookup(
        &self,
        extension_id: &ExtensionId,
        method: &str,
    ) -> Result<&ExtensionHandlerKey, ExtensionHandlerLookupError> {
        if !self.frozen {
            return Err(ExtensionHandlerLookupError::RegistryNotFrozen);
        }
        let key = ExtensionHandlerKey::new(extension_id.clone(), method);
        self.handlers
            .get_key_value(&key)
            .map(|(registered_key, _)| registered_key)
            .ok_or(ExtensionHandlerLookupError::HandlerNotFound(key))
    }

    /// Admits and invokes one typed client-to-server extension request.
    ///
    /// The protocol admission call binds this invocation to the exact frozen
    /// descriptor receipt, current negotiated extension set, modern era, and
    /// client-to-server ownership and request envelope before the handler is
    /// looked up or executed. A handler can only be registered for a
    /// descriptor's request method, so an id-less notification is rejected
    /// generically before its parameters can reach stateful extension code.
    pub fn invoke(
        &self,
        negotiated: &NegotiatedExtensionSet,
        protocol_era: ProtocolEra,
        extension_id: &ExtensionId,
        request: &JsonRpcRequest,
        context: &McpContext,
    ) -> Result<Value, ExtensionHandlerInvocationError> {
        let handler = self.admitted_handler(negotiated, protocol_era, extension_id, request)?;
        handler
            .invoke(
                context,
                request
                    .params
                    .clone()
                    .unwrap_or_else(|| serde_json::json!({})),
            )
            .map_err(ExtensionHandlerInvocationError::Handler)
    }

    /// Admits and awaits a typed handler without entering another runtime.
    /// Synchronous and asynchronous handlers use the same frozen admission.
    pub async fn invoke_async(
        &self,
        context: &McpContext,
        negotiated: &NegotiatedExtensionSet,
        protocol_era: ProtocolEra,
        extension_id: &ExtensionId,
        request: &JsonRpcRequest,
    ) -> Result<Value, ExtensionHandlerInvocationError> {
        let handler = self.admitted_handler(negotiated, protocol_era, extension_id, request)?;
        context
            .ensure_live()
            .map_err(|_| ExtensionHandlerInvocationError::Handler(McpError::request_cancelled()))?;
        let response = handler
            .invoke_async(
                context,
                request
                    .params
                    .clone()
                    .unwrap_or_else(|| serde_json::json!({})),
            )
            .await
            .map_err(ExtensionHandlerInvocationError::Handler)?;
        context
            .ensure_live()
            .map_err(|_| ExtensionHandlerInvocationError::Handler(McpError::request_cancelled()))?;
        Ok(response)
    }

    fn admitted_handler(
        &self,
        negotiated: &NegotiatedExtensionSet,
        protocol_era: ProtocolEra,
        extension_id: &ExtensionId,
        request: &JsonRpcRequest,
    ) -> Result<&dyn ErasedExtensionHandler, ExtensionHandlerInvocationError> {
        if !self.frozen {
            return Err(ExtensionHandlerInvocationError::RegistryNotFrozen);
        }

        negotiated
            .admit_method(
                &self.descriptor_registry,
                protocol_era,
                extension_id,
                &request.method,
                ExtensionDirection::ClientToServer,
            )
            .map_err(ExtensionHandlerInvocationError::Protocol)?;
        let key = ExtensionHandlerKey::new(extension_id.clone(), &request.method);
        if request.is_notification() {
            return Err(ExtensionHandlerInvocationError::RequestEnvelopeRequired(
                key,
            ));
        }
        let key = self
            .lookup(extension_id, &request.method)
            .map_err(|error| match error {
                ExtensionHandlerLookupError::RegistryNotFrozen => {
                    ExtensionHandlerInvocationError::RegistryNotFrozen
                }
                ExtensionHandlerLookupError::HandlerNotFound(key) => {
                    ExtensionHandlerInvocationError::HandlerNotFound(key)
                }
            })?;
        let handler = self
            .handlers
            .get(key)
            .ok_or_else(|| ExtensionHandlerInvocationError::HandlerNotFound(key.clone()))?;
        Ok(handler.as_ref())
    }
}

#[cfg(all(test, feature = "tasks"))]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use asupersync::Cx;
    use fastmcp_core::{McpContext, McpErrorCode, McpResult};
    use fastmcp_protocol::extensions::{
        ClientExtensionDiscovery, ExtensionLocalEnablement, ExtensionSettings,
        ServerExtensionDiscovery, official_tasks_descriptor, official_tasks_empty_settings,
        register_official_tasks_extension,
    };
    use fastmcp_protocol::protocol_policy::ProtocolEra;
    use fastmcp_protocol::{
        ExtensionDescriptorRegistry, ExtensionDirection, ExtensionId, JsonRpcRequest,
    };
    use serde::{Deserialize, Serialize};
    use serde_json::json;

    use super::{
        ExtensionHandlerInvocationError, ExtensionHandlerKey, ExtensionHandlerRegistrationError,
        ExtensionHandlerRegistry,
    };

    fn tasks_descriptors() -> (ExtensionDescriptorRegistry, ExtensionId) {
        let mut descriptors = ExtensionDescriptorRegistry::new();
        let id = register_official_tasks_extension(&mut descriptors)
            .expect("official Tasks descriptor registers");
        (descriptors, id)
    }

    fn async_extension_dispatch_probe(notification: bool, cancel: bool) {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("caller owns the runtime");
        let context = McpContext::new(
            runtime.request_cx_with_budget(asupersync::Budget::default()),
            71,
        );
        let (descriptors, id) = tasks_descriptors();
        let mut handlers = ExtensionHandlerRegistry::new(descriptors);
        let calls = Arc::new(AtomicUsize::new(0));
        let callback_calls = Arc::clone(&calls);
        handlers
            .register_async(
                id.clone(),
                "tasks/get",
                move |context: &McpContext, request: GetTaskRequest| {
                    let calls = Arc::clone(&callback_calls);
                    Box::pin(async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        asupersync::runtime::yield_now().await;
                        if cancel {
                            assert!(context.request_cancellation().cancel());
                        }
                        get_task(context, request)
                    })
                },
            )
            .expect("async typed handler registers");
        handlers.freeze().expect("freeze descriptors and callbacks");
        let negotiated = negotiated_tasks(handlers.descriptor_registry(), &id);
        let mut request = JsonRpcRequest::new("tasks/get", Some(json!({"value": 41})), 1_i64);
        if notification {
            request.id = None;
        }
        let result = runtime.block_on(handlers.invoke_async(
            &context,
            &negotiated,
            ProtocolEra::Modern2026,
            &id,
            &request,
        ));
        if cancel {
            assert!(
                matches!(result, Err(ExtensionHandlerInvocationError::Handler(error))
                if error.code == McpErrorCode::RequestCancelled)
            );
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "callback ran and returned a value, but cancellation must reject its result"
            );
            return;
        }
        if notification {
            assert!(matches!(
                result,
                Err(ExtensionHandlerInvocationError::RequestEnvelopeRequired(_))
            ));
            assert_eq!(
                calls.load(Ordering::SeqCst),
                0,
                "rejected envelope must not poll the handler"
            );
        } else {
            assert_eq!(
                result.expect("async handler completes"),
                json!({"next": 42})
            );
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }
        assert_eq!(handlers.len(), 1);
        request.id = Some(1_i64.into());
        let before = calls.load(Ordering::SeqCst);
        assert!(matches!(handlers.invoke(
            &negotiated, ProtocolEra::Modern2026, &id, &request, &context,
        ), Err(ExtensionHandlerInvocationError::Handler(error))
            if error.code == McpErrorCode::InvalidRequest));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            before,
            "sync entry must not start a hidden runtime"
        );
        assert_eq!(
            runtime
                .block_on(handlers.invoke_async(
                    &context,
                    &negotiated,
                    ProtocolEra::Modern2026,
                    &id,
                    &request,
                ))
                .expect("same admitted handler remains callable"),
            json!({"next": 42})
        );
        assert_eq!(calls.load(Ordering::SeqCst), before + 1);

        let plain_server = crate::ServerBuilder::new("plain-extension-host", "1.0.0").build();
        let unknown = JsonRpcRequest::new(
            "example/unknown",
            Some(json!({
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {}
                }
            })),
            71_i64,
        );
        let error = runtime
            .block_on(plain_server.dispatch_extension_fallback_async(&context, &unknown))
            .expect_err("unregistered modern methods retain method-not-found semantics");
        assert_eq!(error.code, McpErrorCode::MethodNotFound);
    }

    #[test]
    fn async_extension_dispatch_positive() {
        async_extension_dispatch_probe(false, false);
    }

    #[test]
    fn async_extension_dispatch_planted_negative() {
        async_extension_dispatch_probe(true, false);
    }

    #[test]
    fn async_extension_dispatch_rejects_callback_cancelled_result() {
        async_extension_dispatch_probe(false, true);
    }

    fn primary_tasks_descriptor_with_direction(
        direction: ExtensionDirection,
    ) -> (ExtensionDescriptorRegistry, ExtensionId) {
        let id = fastmcp_protocol::official_tasks_extension_id();
        let mut descriptor = official_tasks_descriptor();
        let method = descriptor
            .method
            .as_mut()
            .expect("official Tasks descriptor owns a primary request method");
        method.direction = direction;
        if direction == ExtensionDirection::ServerToClient {
            method.http_era_disposition = None;
        }
        let mut descriptors = ExtensionDescriptorRegistry::new();
        descriptors
            .register(descriptor)
            .expect("direction-adjusted Tasks descriptor remains structurally valid");
        (descriptors, id)
    }

    #[derive(Deserialize)]
    struct GetTaskRequest {
        value: u32,
    }

    #[derive(Debug, PartialEq, Serialize)]
    struct GetTaskResponse {
        next: u32,
    }

    #[derive(Deserialize)]
    struct UpdateTaskRequest {
        title: String,
    }

    #[derive(Debug, PartialEq, Serialize)]
    struct UpdateTaskResponse {
        updated_title: String,
    }

    #[allow(
        clippy::unnecessary_wraps,
        reason = "the ExtensionHandler callback contract requires McpResult"
    )]
    fn get_task(context: &McpContext, request: GetTaskRequest) -> McpResult<GetTaskResponse> {
        assert_eq!(context.request_id(), 71);
        Ok(GetTaskResponse {
            next: request.value + 1,
        })
    }

    #[allow(
        clippy::unnecessary_wraps,
        reason = "the ExtensionHandler callback contract requires McpResult"
    )]
    fn alternate_get_task(
        context: &McpContext,
        request: GetTaskRequest,
    ) -> McpResult<GetTaskResponse> {
        assert_eq!(context.request_id(), 71);
        Ok(GetTaskResponse {
            next: request.value + 2,
        })
    }

    #[allow(
        clippy::unnecessary_wraps,
        reason = "the ExtensionHandler callback contract requires McpResult"
    )]
    fn update_task(
        context: &McpContext,
        request: UpdateTaskRequest,
    ) -> McpResult<UpdateTaskResponse> {
        assert_eq!(context.request_id(), 71);
        Ok(UpdateTaskResponse {
            updated_title: request.title.to_uppercase(),
        })
    }

    fn negotiated_tasks(
        descriptors: &ExtensionDescriptorRegistry,
        id: &ExtensionId,
    ) -> fastmcp_protocol::extensions::NegotiatedExtensionSet {
        let client = ClientExtensionDiscovery {
            extensions: BTreeMap::from([(id.clone(), official_tasks_empty_settings())]),
        };
        let server = ServerExtensionDiscovery {
            extensions: BTreeMap::from([(id.clone(), official_tasks_empty_settings())]),
        };
        let mut local = ExtensionLocalEnablement::default();
        local.enable(id.clone());
        let mut resolver =
            |_descriptor: &fastmcp_protocol::ExtensionDescriptor,
             _client: &ExtensionSettings,
             _server: &ExtensionSettings| { Ok(official_tasks_empty_settings()) };

        descriptors
            .negotiate(
                ProtocolEra::Modern2026,
                &local,
                &client,
                &server,
                &mut resolver,
            )
            .expect("bilaterally advertised official Tasks negotiates")
    }

    #[test]
    fn extension_handler_registry_freezes_and_looks_up_registered_handler() {
        let (descriptors, id) = tasks_descriptors();
        let mut handlers = ExtensionHandlerRegistry::new(descriptors);
        handlers
            .register(id.clone(), "tasks/get", get_task)
            .expect("typed Tasks handler registers");

        let receipt = handlers.freeze().expect("handler registry freezes");

        assert!(handlers.is_frozen());
        assert_eq!(handlers.len(), 1);
        assert_eq!(handlers.descriptor_registry().receipt(), Some(&receipt));
        let registered = handlers
            .lookup(&id, "tasks/get")
            .expect("frozen registry finds its handler registration");
        assert_eq!(registered.extension_id(), &id);
        assert_eq!(registered.method(), "tasks/get");
    }

    #[test]
    fn extension_handler_registry_requires_client_to_server_ownership_without_mutation() {
        let (valid_descriptors, valid_id) =
            primary_tasks_descriptor_with_direction(ExtensionDirection::ClientToServer);
        let mut valid_handlers = ExtensionHandlerRegistry::new(valid_descriptors);
        valid_handlers
            .register(valid_id, "tasks/get", get_task)
            .expect("a descriptor-owned client-to-server method registers");
        assert_eq!(valid_handlers.len(), 1);

        let (rejected_descriptors, rejected_id) =
            primary_tasks_descriptor_with_direction(ExtensionDirection::ServerToClient);
        let mut rejected_handlers = ExtensionHandlerRegistry::new(rejected_descriptors);
        let key = ExtensionHandlerKey::new(rejected_id.clone(), "tasks/get");

        assert_eq!(
            rejected_handlers.register(rejected_id.clone(), "tasks/get", get_task),
            Err(ExtensionHandlerRegistrationError::MethodNotClientToServer(
                key
            )),
            "the structurally valid server-to-client direction must reject before insertion"
        );
        assert_eq!(
            rejected_handlers.len(),
            0,
            "a server-to-client method cannot create a dead server handler entry"
        );
        assert_eq!(rejected_handlers.server_metadata_len(), 0);
        assert_eq!(
            rejected_handlers
                .descriptor_registry()
                .method_descriptor(&rejected_id, "tasks/get")
                .map(|method| method.direction),
            Some(ExtensionDirection::ServerToClient),
            "rejection must not alter the owned descriptor"
        );
    }

    #[test]
    #[cfg(feature = "apps")]
    fn official_apps_server_metadata_is_emitted_from_the_frozen_registry() {
        let (descriptors, tasks_id) = tasks_descriptors();
        let mut handlers = ExtensionHandlerRegistry::new(descriptors);
        let apps_id = handlers
            .install_official_mcp_apps()
            .expect("the official Apps descriptor and marker install alongside Tasks");
        let (receipt, discovery) = handlers
            .freeze_with_server_discovery()
            .expect("Apps metadata registry freezes with matching discovery");

        assert_eq!(handlers.descriptor_registry().receipt(), Some(&receipt));
        assert_eq!(handlers.server_metadata_len(), 1);
        assert!(
            handlers
                .descriptor_registry()
                .descriptor(&tasks_id)
                .is_some(),
            "Apps installation preserves the already-registered Tasks descriptor"
        );
        assert_eq!(
            handlers.descriptor_registry().descriptor(&apps_id),
            Some(&fastmcp_protocol::official_mcp_apps_descriptor()),
            "server discovery must use the protocol's official Apps descriptor"
        );
        assert_eq!(
            discovery
                .extensions
                .get(&apps_id)
                .map(|settings| serde_json::Value::Object(settings.as_object().clone())),
            Some(json!({})),
            "the official Apps server marker is emitted only through the frozen descriptor registry"
        );
    }

    #[test]
    fn unnegotiated_extension_metadata_is_rejected_without_mutating_the_registry() {
        let (descriptors, _) = tasks_descriptors();
        let mut handlers = ExtensionHandlerRegistry::new(descriptors);
        let unnegotiated = fastmcp_protocol::official_mcp_apps_extension_id();
        let marker = ExtensionSettings::new(json!({}))
            .expect("the unchanged official Apps server marker is an object");

        assert_eq!(
            handlers.register_server_metadata(unnegotiated.clone(), marker),
            Err(ExtensionHandlerRegistrationError::UnregisteredExtension(
                unnegotiated.to_string()
            )),
            "only the extension identifier differs from the registered Apps metadata path"
        );
        assert_eq!(handlers.server_metadata_len(), 0);
        assert!(
            handlers
                .descriptor_registry()
                .descriptor(&unnegotiated)
                .is_none()
        );

        handlers.freeze().expect("unchanged registry still freezes");
        assert!(
            handlers
                .server_discovery()
                .expect("unchanged frozen registry still exports discovery")
                .extensions
                .is_empty(),
            "the rejected unnegotiated extension cannot alter advertised metadata"
        );
    }

    #[test]
    fn extension_handler_registry_rejects_duplicate_key_one_variable_negative() {
        let (descriptors, id) = tasks_descriptors();
        let mut handlers = ExtensionHandlerRegistry::new(descriptors);
        handlers
            .register(id.clone(), "tasks/get", get_task)
            .expect("baseline handler registers");

        assert_eq!(
            handlers.register(id.clone(), "tasks/get", alternate_get_task),
            Err(ExtensionHandlerRegistrationError::DuplicateHandler(
                ExtensionHandlerKey::new(id, "tasks/get")
            )),
            "only the handler implementation changes from the registered request location"
        );
    }

    #[test]
    fn extension_handler_registry_erases_heterogeneous_types_and_rejects_malformed_input() {
        let (descriptors, id) = tasks_descriptors();
        let mut handlers = ExtensionHandlerRegistry::new(descriptors);
        let get_calls = Arc::new(AtomicUsize::new(0));
        let counted_get_calls = Arc::clone(&get_calls);
        handlers
            .register(
                id.clone(),
                "tasks/get",
                move |context: &McpContext, request: GetTaskRequest| {
                    counted_get_calls.fetch_add(1, Ordering::Relaxed);
                    get_task(context, request)
                },
            )
            .expect("typed Tasks get handler registers");
        handlers
            .register(id.clone(), "tasks/update", update_task)
            .expect("differently typed Tasks update handler registers");
        handlers.freeze().expect("handler registry freezes");
        let negotiated = negotiated_tasks(handlers.descriptor_registry(), &id);
        let context = McpContext::new(Cx::for_testing(), 71);

        assert_eq!(
            handlers
                .invoke(
                    &negotiated,
                    ProtocolEra::Modern2026,
                    &id,
                    &JsonRpcRequest::new("tasks/get", Some(json!({"value": 41})), 1_i64),
                    &context,
                )
                .expect("negotiated protocol admission invokes the typed get handler"),
            json!({"next": 42})
        );
        assert_eq!(
            handlers
                .invoke(
                    &negotiated,
                    ProtocolEra::Modern2026,
                    &id,
                    &JsonRpcRequest::new("tasks/update", Some(json!({"title": "review"})), 2_i64),
                    &context,
                )
                .expect("the same registry invokes the differently typed update handler"),
            json!({"updated_title": "REVIEW"})
        );
        assert_eq!(get_calls.load(Ordering::Relaxed), 1);

        let error = handlers
            .invoke(
                &negotiated,
                ProtocolEra::Modern2026,
                &id,
                &JsonRpcRequest::new("tasks/get", Some(json!({"unexpected": true})), 3_i64),
                &context,
            )
            .expect_err("one malformed request field shape must reject before the handler runs");
        let ExtensionHandlerInvocationError::Handler(error) = error else {
            panic!("malformed parameters must reach the typed serde admission boundary");
        };
        assert_eq!(error.code, McpErrorCode::InvalidParams);
        assert_eq!(
            get_calls.load(Ordering::Relaxed),
            1,
            "failed typed decoding cannot invoke the registered handler"
        );
    }

    #[test]
    fn extension_request_descriptor_rejects_an_idless_near_match_before_handler_invocation() {
        let (descriptors, id) = tasks_descriptors();
        let mut handlers = ExtensionHandlerRegistry::new(descriptors);
        let calls = Arc::new(AtomicUsize::new(0));
        let counted_calls = Arc::clone(&calls);
        handlers
            .register(
                id.clone(),
                "tasks/update",
                move |_context: &McpContext, _request: UpdateTaskRequest| {
                    counted_calls.fetch_add(1, Ordering::Relaxed);
                    Ok(serde_json::json!({"resultType": "complete"}))
                },
            )
            .expect("typed Tasks update handler registers");
        handlers.freeze().expect("handler registry freezes");
        let negotiated = negotiated_tasks(handlers.descriptor_registry(), &id);
        let context = McpContext::new(Cx::for_testing(), 72);
        let parameters = json!({"title": "review"});

        assert_eq!(
            handlers
                .invoke(
                    &negotiated,
                    ProtocolEra::Modern2026,
                    &id,
                    &JsonRpcRequest::new("tasks/update", Some(parameters.clone()), 4_i64),
                    &context,
                )
                .expect("the request-shaped extension frame is admitted"),
            json!({"resultType": "complete"})
        );
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        let error = handlers
            .invoke(
                &negotiated,
                ProtocolEra::Modern2026,
                &id,
                &JsonRpcRequest::notification("tasks/update", Some(parameters)),
                &context,
            )
            .expect_err("removing only the request id must reject before handler invocation");
        assert!(matches!(
            error,
            ExtensionHandlerInvocationError::RequestEnvelopeRequired(_)
        ));
        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "the rejected id-less near-match must leave handler-owned state unchanged"
        );
    }
}
