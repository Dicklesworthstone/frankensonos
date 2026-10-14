//! Filesystem resource provider.
//!
//! Exposes files from a directory as MCP resources with configurable
//! patterns, security controls, and MIME type detection.
//!
//! # Security
//!
//! The implementation acquires one root directory capability, traverses each
//! requested component relative to retained directory handles, refuses
//! symlinks, and reads bytes only from the already-opened file handle.
//! Those handle-relative no-follow semantics are qualified on Linux and
//! macOS. Public [`FilesystemProvider::build`] constructs a handler there
//! and routes listing/read I/O through the caller-owned asupersync blocking
//! pool when one is installed. Other targets remain fail-closed.
//!
//! # Example
//!
//! ```ignore
//! use fastmcp_server::providers::{FilesystemProvider, FilesystemProviderError};
//!
//! let result = FilesystemProvider::new("/data/docs")
//!     .with_prefix("docs")
//!     .with_patterns(&["**/*.md", "**/*.txt"])
//!     .with_exclude(&["**/secret/**", "**/.*"])
//!     .with_recursive(true)
//!     .with_max_size(10 * 1024 * 1024)
//!     .build(); // succeeds on Linux/macOS; FeatureUnavailable elsewhere
//! ```

use std::{
    ffi::OsString,
    io::Read,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use cap_fs_ext::{DirExt, FollowSymlinks, MetadataExt, OpenOptionsFollowExt, OpenOptionsSyncExt};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use cap_std::ambient_authority;
use cap_std::fs::{Dir, File, OpenOptions};

use asupersync::Cx;
use fastmcp_core::{McpContext, McpError, McpOutcome, McpResult, Outcome};
use fastmcp_protocol::{
    FinalReadResourceResult, FinalResourceTemplate, Resource, ResourceContent, ResourceTemplate,
};

use crate::handler::{BoxFuture, FinalMethodOutcome, ResourceHandler, UriParams};

/// Default maximum file size (10 MB).
const DEFAULT_MAX_SIZE: usize = 10 * 1024 * 1024;
/// Default maximum number of directory entries inspected per listing.
const DEFAULT_MAX_ENTRIES: usize = 10_000;
/// Default maximum recursive directory depth beneath the provider root.
const DEFAULT_MAX_DEPTH: usize = 64;
/// Default maximum encoded text size of a generated directory listing (1 MB).
const DEFAULT_MAX_LISTING_BYTES: usize = 1024 * 1024;
/// Hard ceiling for one decoded file payload retained by the provider.
const MAX_CONFIGURED_FILE_SIZE: usize = 10 * 1024 * 1024;
/// Hard ceiling for one encoded binary resource payload.
const MAX_ENCODED_BINARY_BYTES: usize = ((MAX_CONFIGURED_FILE_SIZE + 2) / 3) * 4;
/// Maximum admitted relative-path bytes.
const MAX_RELATIVE_PATH_BYTES: usize = 4096;
/// Percent-encoded UTF-8 can expand every admitted byte to `%HH`.
const MAX_ENCODED_RELATIVE_PATH_BYTES: usize = MAX_RELATIVE_PATH_BYTES * 3;
/// Maximum admitted URI-prefix bytes.
const MAX_URI_PREFIX_BYTES: usize = 256;
/// Hard bounds for user-supplied glob policy and resource descriptions.
const MAX_GLOB_PATTERNS: usize = 16;
const MAX_GLOB_PATTERN_BYTES: usize = 128;
const MAX_TOTAL_GLOB_PATTERN_BYTES: usize = 512;
const MAX_GLOB_WILDCARDS_PER_PATTERN: usize = 32;
const MAX_DESCRIPTION_BYTES: usize = 4096;
/// Hard configuration ceilings for directory traversal and listing output.
const MAX_CONFIGURED_ENTRIES: usize = 100_000;
const MAX_CONFIGURED_DEPTH: usize = 256;
const MAX_CONFIGURED_LISTING_BYTES: usize = 10 * 1024 * 1024;
const REDACTED_RESOURCE_PATH: &str = "<resource-path>";
/// Stable label returned by the public production-promotion gate.
const FILESYSTEM_PROVIDER_PROMOTION_GATE: &str =
    "non-unix targets (handle-relative no-follow filesystem I/O is unqualified)";
const LISTING_ENTRY_PREFIX: &str = "{\"uri\":\"";
const LISTING_ENTRY_MIME: &str = "\",\"mimeType\":\"";
const LISTING_ENTRY_SUFFIX: &str = "\"}";

const fn is_directional_format_character(character: char) -> bool {
    matches!(
        character,
        '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
}

fn has_unsafe_display_characters(value: &str) -> bool {
    value
        .chars()
        .any(|character| character.is_control() || is_directional_format_character(character))
}

fn path_traversal_error() -> FilesystemProviderError {
    FilesystemProviderError::PathTraversal {
        requested: REDACTED_RESOURCE_PATH.to_string(),
    }
}

fn from_uri_hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn decode_resource_path(encoded: &str) -> Result<String, FilesystemProviderError> {
    let bytes = encoded.as_bytes();
    let mut decoded = Vec::new();
    decoded
        .try_reserve_exact(bytes.len().min(MAX_RELATIVE_PATH_BYTES))
        .map_err(|error| FilesystemProviderError::Io {
            message: format!("Cannot allocate decoded resource path: {error}"),
        })?;
    let mut index = 0_usize;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index.saturating_add(2) >= bytes.len() {
                return Err(path_traversal_error());
            }
            let high = from_uri_hex(bytes[index + 1]).ok_or_else(path_traversal_error)?;
            let low = from_uri_hex(bytes[index + 2]).ok_or_else(path_traversal_error)?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
        if decoded.len() > MAX_RELATIVE_PATH_BYTES {
            return Err(path_traversal_error());
        }
    }
    String::from_utf8(decoded).map_err(|_| path_traversal_error())
}

const fn resource_path_byte_may_remain_literal(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'-' | b'.'
                | b'_'
                | b'~'
                | b':'
                | b'/'
                | b'@'
                | b'!'
                | b'$'
                | b'&'
                | b'\''
                | b'('
                | b')'
                | b'*'
                | b'+'
                | b','
                | b';'
                | b'='
        )
}

fn encode_resource_path(path: &str) -> Result<String, FilesystemProviderError> {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let capacity = path
        .len()
        .checked_mul(3)
        .filter(|capacity| *capacity <= MAX_ENCODED_RELATIVE_PATH_BYTES)
        .ok_or_else(path_traversal_error)?;
    let mut encoded = String::new();
    encoded
        .try_reserve_exact(capacity)
        .map_err(|error| FilesystemProviderError::Io {
            message: format!("Cannot allocate encoded resource path: {error}"),
        })?;
    for byte in path.bytes() {
        if resource_path_byte_may_remain_literal(byte) {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    Ok(encoded)
}

/// Errors that can occur when using the filesystem provider.
#[derive(Debug, Clone)]
pub enum FilesystemProviderError {
    /// The requested path would escape the root directory.
    PathTraversal { requested: String },
    /// The file exceeds the maximum allowed size.
    TooLarge { path: String, size: u64, max: usize },
    /// Symlink access was denied.
    SymlinkDenied { path: String },
    /// Multi-link files are denied because containment cannot be proven.
    HardLinkDenied { path: String, links: u64 },
    /// The target or production-promotion gate has not qualified the required semantics.
    FeatureUnavailable { platform: String },
    /// IO error occurred.
    Io { message: String },
    /// File not found.
    NotFound { path: String },
    /// A directory listing inspected more entries than configured.
    TooManyEntries { count: usize, max: usize },
    /// Recursive traversal exceeded the configured depth.
    TooDeep {
        path: String,
        depth: usize,
        max: usize,
    },
    /// A generated listing would exceed its configured byte ceiling.
    ListingTooLarge { size: usize, max: usize },
    /// Provider configuration is invalid or exceeds a hard safety ceiling.
    InvalidConfiguration { field: &'static str },
    /// The request was cancelled or its budget expired.
    Cancelled,
}

impl std::fmt::Display for FilesystemProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PathTraversal { requested } => {
                write!(f, "Path traversal attempt blocked: {requested}")
            }
            Self::TooLarge { path, size, max } => {
                write!(f, "File too large: {path} ({size} bytes, max {max} bytes)")
            }
            Self::SymlinkDenied { path } => {
                write!(f, "Symlink access denied: {path}")
            }
            Self::HardLinkDenied { path, links } => {
                write!(f, "Hard-linked file access denied: {path} ({links} links)")
            }
            Self::FeatureUnavailable { platform } => {
                write!(f, "Filesystem provider unavailable on {platform}")
            }
            Self::Io { message } => write!(f, "IO error: {message}"),
            Self::NotFound { path } => write!(f, "File not found: {path}"),
            Self::TooManyEntries { count, max } => {
                write!(
                    f,
                    "Directory listing inspected too many entries: {count} > {max}"
                )
            }
            Self::TooDeep { path, depth, max } => {
                write!(
                    f,
                    "Directory listing exceeded depth at {path}: {depth} > {max}"
                )
            }
            Self::ListingTooLarge { size, max } => {
                write!(f, "Directory listing too large: {size} > {max} bytes")
            }
            Self::InvalidConfiguration { field } => {
                write!(f, "Invalid filesystem provider configuration: {field}")
            }
            Self::Cancelled => write!(f, "Filesystem request cancelled"),
        }
    }
}

impl std::error::Error for FilesystemProviderError {}

impl From<FilesystemProviderError> for McpError {
    fn from(err: FilesystemProviderError) -> Self {
        match err {
            FilesystemProviderError::PathTraversal { .. } => {
                McpError::invalid_request("Filesystem resource path was rejected")
            }
            FilesystemProviderError::TooLarge { size, max, .. } => McpError::invalid_request(
                format!("Filesystem resource exceeds the size limit: {size} > {max} bytes"),
            ),
            FilesystemProviderError::SymlinkDenied { .. }
            | FilesystemProviderError::HardLinkDenied { .. } => {
                McpError::invalid_request("Filesystem resource link access was rejected")
            }
            FilesystemProviderError::FeatureUnavailable { .. } => {
                McpError::internal_error(err.to_string())
            }
            FilesystemProviderError::Io { .. } => {
                McpError::internal_error("Filesystem resource operation failed")
            }
            FilesystemProviderError::NotFound { .. } => {
                McpError::resource_not_found(REDACTED_RESOURCE_PATH)
            }
            FilesystemProviderError::TooManyEntries { .. }
            | FilesystemProviderError::ListingTooLarge { .. }
            | FilesystemProviderError::InvalidConfiguration { .. } => {
                McpError::invalid_request(err.to_string())
            }
            FilesystemProviderError::TooDeep { depth, max, .. } => McpError::invalid_request(
                format!("Filesystem traversal exceeds the depth limit: {depth} > {max}"),
            ),
            FilesystemProviderError::Cancelled => McpError::request_cancelled(),
        }
    }
}

/// A resource provider that exposes filesystem directories.
///
/// Files under the configured root directory are exposed as MCP resources
/// with local file URIs like `file:///{prefix}/{relative_path}`.
///
/// # Security
///
/// - Path traversal attempts (e.g., `../../../etc/passwd`) are blocked
/// - Final and intermediate symlinks are always blocked
/// - Multi-link files are blocked because another name may exist outside the root
/// - Maximum file size limits prevent memory exhaustion
/// - Hidden files (starting with `.`) can be excluded
///
/// # Example
///
/// ```ignore
/// use fastmcp_server::providers::FilesystemProvider;
///
/// let provider = FilesystemProvider::new("/app/data")
///     .with_prefix("data")
///     .with_patterns(&["*.json", "*.yaml"])
///     .with_recursive(true);
/// ```
#[derive(Clone)]
pub struct FilesystemProvider {
    /// Root path retained only for diagnostics after initial acquisition.
    root: PathBuf,
    /// Retained directory capability used for every enumeration and read.
    root_directory: Result<Arc<Dir>, Arc<str>>,
    /// URI path prefix (e.g., "docs" -> "file:///docs/...").
    prefix: Option<String>,
    /// Glob patterns to include (empty = all files).
    include_patterns: Vec<String>,
    /// Whether the include-pattern setter admitted its complete input.
    include_patterns_valid: bool,
    /// Glob patterns to exclude.
    exclude_patterns: Vec<String>,
    /// Whether the exclude-pattern setter admitted its complete input.
    exclude_patterns_valid: bool,
    /// Whether to traverse subdirectories.
    recursive: bool,
    /// Maximum file size in bytes.
    max_file_size: usize,
    /// Maximum directory entries inspected by one live listing.
    max_entries: usize,
    /// Maximum recursive depth beneath the root.
    max_depth: usize,
    /// Maximum UTF-8 bytes emitted by a directory listing.
    max_listing_bytes: usize,
    /// Description for the resource template.
    description: Option<String>,
}

impl std::fmt::Debug for FilesystemProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FilesystemProvider")
            .field("root_capability_acquired", &self.root_directory.is_ok())
            .field("prefix", &self.prefix)
            .field("include_pattern_count", &self.include_patterns.len())
            .field("exclude_pattern_count", &self.exclude_patterns.len())
            .field("recursive", &self.recursive)
            .field("max_file_size", &self.max_file_size)
            .field("max_entries", &self.max_entries)
            .field("max_depth", &self.max_depth)
            .field("max_listing_bytes", &self.max_listing_bytes)
            .field("description_configured", &self.description.is_some())
            .finish()
    }
}

impl FilesystemProvider {
    /// Creates a new filesystem provider for the given root directory.
    ///
    /// # Arguments
    ///
    /// * `root` - The root directory to expose
    ///
    /// # Example
    ///
    /// ```ignore
    /// let provider = FilesystemProvider::new("/data/docs");
    /// ```
    #[must_use]
    pub fn new(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref().to_path_buf();
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let root_directory = Dir::open_ambient_dir(&root, ambient_authority())
            .map(Arc::new)
            .map_err(|error| Arc::<str>::from(error.to_string()));
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let root_directory = Err(Arc::<str>::from(
            "filesystem capability acquisition is unqualified on this target",
        ));

        Self {
            root,
            root_directory,
            prefix: None,
            include_patterns: Vec::new(),
            include_patterns_valid: true,
            // Exclude hidden entries at the root and at every recursive level.
            // Directory exclusions are checked before descent, so a hidden
            // directory cannot expose non-hidden descendants.
            exclude_patterns: vec![".*".to_string(), "**/.*".to_string()],
            exclude_patterns_valid: true,
            recursive: false,
            max_file_size: DEFAULT_MAX_SIZE,
            max_entries: DEFAULT_MAX_ENTRIES,
            max_depth: DEFAULT_MAX_DEPTH,
            max_listing_bytes: DEFAULT_MAX_LISTING_BYTES,
            description: None,
        }
    }

    /// Sets the URI prefix for resources.
    ///
    /// Files will have URIs like `file:///{prefix}/{path}`.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let provider = FilesystemProvider::new("/data")
    ///     .with_prefix("mydata");
    /// // Results in URIs like file:///mydata/readme.md
    /// ```
    #[must_use]
    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = Some(prefix.into());
        self
    }

    /// Sets glob patterns to include.
    ///
    /// Only files matching at least one of these patterns will be exposed.
    /// Empty patterns means all files are included.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let provider = FilesystemProvider::new("/data")
    ///     .with_patterns(&["*.md", "*.txt", "**/*.json"]);
    /// ```
    #[must_use]
    pub fn with_patterns(mut self, patterns: &[&str]) -> Self {
        match admit_glob_patterns(patterns) {
            Some(patterns) => {
                self.include_patterns = patterns;
                self.include_patterns_valid = true;
            }
            None => {
                self.include_patterns.clear();
                self.include_patterns_valid = false;
            }
        }
        self
    }

    /// Sets glob patterns to exclude.
    ///
    /// Files matching any of these patterns will be excluded.
    /// By default, hidden files (starting with `.`) are excluded.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let provider = FilesystemProvider::new("/data")
    ///     .with_exclude(&["**/secret/**", "*.bak"]);
    /// ```
    #[must_use]
    pub fn with_exclude(mut self, patterns: &[&str]) -> Self {
        match admit_glob_patterns(patterns) {
            Some(patterns) => {
                self.exclude_patterns = patterns;
                self.exclude_patterns_valid = true;
            }
            None => {
                self.exclude_patterns.clear();
                self.exclude_patterns_valid = false;
            }
        }
        self
    }

    /// Enables or disables recursive directory traversal.
    ///
    /// When enabled, files in subdirectories are also exposed.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let provider = FilesystemProvider::new("/data")
    ///     .with_recursive(true);
    /// ```
    #[must_use]
    pub fn with_recursive(mut self, enabled: bool) -> Self {
        self.recursive = enabled;
        self
    }

    /// Sets the maximum file size in bytes.
    ///
    /// Files larger than this limit will return an error when read.
    /// Default is 10 MB.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let provider = FilesystemProvider::new("/data")
    ///     .with_max_size(5 * 1024 * 1024); // 5 MB
    /// ```
    #[must_use]
    pub fn with_max_size(mut self, bytes: usize) -> Self {
        self.max_file_size = bytes;
        self
    }

    /// Sets the maximum number of directory entries inspected by one listing.
    #[must_use]
    pub fn with_max_entries(mut self, entries: usize) -> Self {
        self.max_entries = entries;
        self
    }

    /// Sets the maximum recursive directory depth beneath the provider root.
    /// The root itself has depth zero.
    #[must_use]
    pub fn with_max_depth(mut self, depth: usize) -> Self {
        self.max_depth = depth;
        self
    }

    /// Sets the maximum UTF-8 byte size of a generated directory listing.
    #[must_use]
    pub fn with_max_listing_bytes(mut self, bytes: usize) -> Self {
        self.max_listing_bytes = bytes;
        self
    }

    /// Sets the description for the resource template.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let provider = FilesystemProvider::new("/data")
    ///     .with_description("Documentation files");
    /// ```
    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Validates this provider and constructs a production handler.
    ///
    /// Linux and macOS acquire a handle-relative directory capability and
    /// return a handler. Listing and reads run on the caller-owned asupersync
    /// blocking pool when one is installed. Other targets remain fail-closed.
    ///
    /// # Errors
    ///
    /// Returns configuration errors, root-acquisition failures, or
    /// [`FilesystemProviderError::FeatureUnavailable`] on unqualified targets.
    pub fn build(self) -> Result<FilesystemResourceHandler, FilesystemProviderError> {
        self.validate_configuration()?;
        self.root_directory()?;
        Ok(FilesystemResourceHandler { provider: self })
    }

    /// Constructs a handler only for inline unit testing of the quarantined
    /// implementation. This is deliberately private and absent from
    /// production builds, so it cannot bypass the public promotion gate.
    #[cfg(test)]
    fn build_for_test(self) -> Result<FilesystemResourceHandler, FilesystemProviderError> {
        self.root_directory()?;
        self.validate_configuration()?;
        Ok(FilesystemResourceHandler::new(self))
    }

    fn validate_configuration(&self) -> Result<(), FilesystemProviderError> {
        if !self.include_patterns_valid || !self.exclude_patterns_valid {
            return Err(FilesystemProviderError::InvalidConfiguration {
                field: "glob_patterns",
            });
        }
        if self.max_file_size > MAX_CONFIGURED_FILE_SIZE {
            return Err(FilesystemProviderError::InvalidConfiguration {
                field: "max_file_size",
            });
        }
        if self.max_entries == 0 || self.max_entries > MAX_CONFIGURED_ENTRIES {
            return Err(FilesystemProviderError::InvalidConfiguration {
                field: "max_entries",
            });
        }
        if self.max_depth > MAX_CONFIGURED_DEPTH {
            return Err(FilesystemProviderError::InvalidConfiguration { field: "max_depth" });
        }
        if self.max_listing_bytes < 2 || self.max_listing_bytes > MAX_CONFIGURED_LISTING_BYTES {
            return Err(FilesystemProviderError::InvalidConfiguration {
                field: "max_listing_bytes",
            });
        }
        if let Some(prefix) = self.prefix.as_deref()
            && (prefix.is_empty()
                || prefix.len() > MAX_URI_PREFIX_BYTES
                || prefix.bytes().any(|byte| {
                    !byte.is_ascii_alphanumeric() && !matches!(byte, b'.' | b'_' | b'-')
                }))
        {
            return Err(FilesystemProviderError::InvalidConfiguration { field: "prefix" });
        }
        if self.description.as_ref().is_some_and(|description| {
            description.len() > MAX_DESCRIPTION_BYTES || has_unsafe_display_characters(description)
        }) {
            return Err(FilesystemProviderError::InvalidConfiguration {
                field: "description",
            });
        }
        Ok(())
    }

    /// Returns the directory capability acquired when the provider was built.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn root_directory(&self) -> Result<&Dir, FilesystemProviderError> {
        match &self.root_directory {
            Ok(directory) => Ok(directory),
            Err(message) => Err(FilesystemProviderError::Io {
                message: format!(
                    "Cannot open filesystem provider root {}: {message}",
                    self.root.display()
                ),
            }),
        }
    }

    /// Fails closed where handle-relative no-follow semantics are unqualified.
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn root_directory(&self) -> Result<&Dir, FilesystemProviderError> {
        let _ = (&self.root, &self.root_directory);
        Err(FilesystemProviderError::FeatureUnavailable {
            platform: std::env::consts::OS.to_string(),
        })
    }

    /// Validates that a request consists only of normal relative components.
    fn validate_path(&self, requested: &str) -> Result<Vec<OsString>, FilesystemProviderError> {
        if requested.len() > MAX_RELATIVE_PATH_BYTES
            || requested
                .chars()
                .any(|character| character.is_control() || matches!(character, '?' | '#'))
        {
            return Err(path_traversal_error());
        }
        let requested_path = Path::new(requested);
        if requested_path.is_absolute() {
            return Err(path_traversal_error());
        }

        let mut components = Vec::new();
        for component in requested_path.components() {
            match component {
                Component::Normal(name) => components.push(name.to_os_string()),
                Component::Prefix(_)
                | Component::RootDir
                | Component::CurDir
                | Component::ParentDir => {
                    return Err(path_traversal_error());
                }
            }
        }

        if components.is_empty() {
            return Err(path_traversal_error());
        }

        // Policy checks and handle-relative traversal must see the same path
        // spelling. `Path::components` normalizes repeated and trailing `/`
        // separators; accepting that alias while glob exclusions inspect the
        // raw request could let one file acquire two different policy names.
        let canonical = components
            .iter()
            .map(|component| component.to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        if canonical != requested {
            return Err(path_traversal_error());
        }
        Ok(components)
    }

    /// Returns true only for one ordinary path component.
    fn is_normal_component(path: &Path) -> bool {
        let mut components = path.components();
        matches!(
            (components.next(), components.next()),
            (Some(Component::Normal(_)), None)
        )
    }

    /// Maps a failed handle-relative open without using a path for access.
    fn map_component_open_error(
        parent: &Dir,
        component: &Path,
        requested: &str,
        error: std::io::Error,
    ) -> FilesystemProviderError {
        if parent
            .symlink_metadata(component)
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            return FilesystemProviderError::SymlinkDenied {
                path: requested.to_string(),
            };
        }

        if error.kind() == std::io::ErrorKind::NotFound {
            FilesystemProviderError::NotFound {
                path: requested.to_string(),
            }
        } else {
            FilesystemProviderError::Io {
                message: format!(
                    "Cannot open {requested} relative to retained directory capability: {error}"
                ),
            }
        }
    }

    /// Opens one file beneath the retained root, refusing links at every hop.
    fn open_file_nofollow(&self, requested: &str) -> Result<File, FilesystemProviderError> {
        let components = self.validate_path(requested)?;
        let Some((final_component, parent_components)) = components.split_last() else {
            return Err(path_traversal_error());
        };
        let mut current =
            self.root_directory()?
                .try_clone()
                .map_err(|error| FilesystemProviderError::Io {
                    message: format!("Cannot clone retained root directory capability: {error}"),
                })?;

        for component in parent_components {
            let component_path = Path::new(component);
            current = match current.open_dir_nofollow(component_path) {
                Ok(next) => next,
                Err(error) => {
                    return Err(Self::map_component_open_error(
                        &current,
                        component_path,
                        requested,
                        error,
                    ));
                }
            };
        }

        let final_component = Path::new(final_component);
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No).nonblock(true);
        current
            .open_with(final_component, &options)
            .map_err(|error| {
                Self::map_component_open_error(&current, final_component, requested, error)
            })
    }

    /// Checks if a filename matches the include/exclude patterns.
    fn matches_patterns(&self, relative_path: &str) -> bool {
        if self.is_excluded(relative_path) {
            return false;
        }

        // If no include patterns, include everything
        if self.include_patterns.is_empty() {
            return true;
        }

        // Check include patterns
        for pattern in &self.include_patterns {
            if glob_match(pattern, relative_path) {
                return true;
            }
        }

        false
    }

    /// Returns true when an exclusion pattern matches this relative path.
    fn is_excluded(&self, relative_path: &str) -> bool {
        self.exclude_patterns
            .iter()
            .any(|pattern| glob_match(pattern, relative_path))
    }

    /// Returns true when this path or any directory prefix is excluded.
    ///
    /// Directory walking checks exclusions before descent. Direct reads must
    /// apply the same rule to every prefix or a caller could name a visible
    /// file beneath an excluded directory without ever listing it.
    fn has_excluded_ancestor(&self, relative_path: &str) -> bool {
        let mut prefix = String::new();
        for component in relative_path.split('/') {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(component);
            if self.is_excluded(&prefix) {
                return true;
            }
        }
        false
    }

    /// Lists files in the directory that match patterns.
    fn list_files(&self, ctx: &McpContext) -> Result<Vec<FileEntry>, FilesystemProviderError> {
        ctx.checkpoint()
            .map_err(|_| FilesystemProviderError::Cancelled)?;
        let mut entries = Vec::new();
        let mut inspected_entries = 0_usize;
        // Every listing is a JSON array, including the empty `[]` case.
        let mut listing_bytes = 2_usize;
        self.walk_directory(
            ctx,
            self.root_directory()?,
            "",
            0,
            &mut inspected_entries,
            &mut listing_bytes,
            &mut entries,
        )?;
        entries.sort_unstable_by(|left, right| {
            left.relative_path
                .as_bytes()
                .cmp(right.relative_path.as_bytes())
        });
        Ok(entries)
    }

    /// Recursively walks a directory collecting file entries.
    fn walk_directory(
        &self,
        ctx: &McpContext,
        current: &Dir,
        relative_parent: &str,
        depth: usize,
        inspected_entries: &mut usize,
        listing_bytes: &mut usize,
        entries: &mut Vec<FileEntry>,
    ) -> Result<(), FilesystemProviderError> {
        ctx.checkpoint()
            .map_err(|_| FilesystemProviderError::Cancelled)?;
        let read_dir = current
            .entries()
            .map_err(|error| FilesystemProviderError::Io {
                message: format!("Cannot enumerate retained directory capability: {error}"),
            })?;

        for entry_result in read_dir {
            ctx.checkpoint()
                .map_err(|_| FilesystemProviderError::Cancelled)?;
            *inspected_entries = (*inspected_entries).saturating_add(1);
            if *inspected_entries > self.max_entries {
                return Err(FilesystemProviderError::TooManyEntries {
                    count: *inspected_entries,
                    max: self.max_entries,
                });
            }
            let entry = entry_result.map_err(|error| FilesystemProviderError::Io {
                message: format!("Cannot enumerate directory entry: {error}"),
            })?;

            let Ok(file_name) = entry.file_name().into_string() else {
                // Resource URIs are UTF-8. Do not create a lossy alias for an
                // unrepresentable directory entry.
                continue;
            };
            if file_name.chars().any(char::is_control) {
                continue;
            }
            let component = Path::new(&file_name);
            if !Self::is_normal_component(component) {
                continue;
            }

            let relative_path = if relative_parent.is_empty() {
                file_name.clone()
            } else {
                format!("{relative_parent}/{file_name}")
            };
            if self.validate_path(&relative_path).is_err() {
                // Skip entries that cannot be represented by the provider's
                // canonical, bounded resource-URI policy. In particular this
                // prevents advertising overlong descendants.
                continue;
            }
            let metadata = current.symlink_metadata(component).map_err(|error| {
                Self::map_component_open_error(current, component, &relative_path, error)
            })?;
            if metadata.file_type().is_symlink() {
                continue;
            }
            if self.is_excluded(&relative_path) {
                continue;
            }

            if metadata.is_dir() {
                if self.recursive {
                    let child_depth = depth.saturating_add(1);
                    if child_depth > self.max_depth {
                        return Err(FilesystemProviderError::TooDeep {
                            path: relative_path,
                            depth: child_depth,
                            max: self.max_depth,
                        });
                    }
                    let child = current.open_dir_nofollow(component).map_err(|error| {
                        Self::map_component_open_error(current, component, &relative_path, error)
                    })?;
                    self.walk_directory(
                        ctx,
                        &child,
                        &relative_path,
                        child_depth,
                        inspected_entries,
                        listing_bytes,
                        entries,
                    )?;
                }
            } else if metadata.is_file() && self.matches_patterns(&relative_path) {
                let mut options = OpenOptions::new();
                options.read(true).follow(FollowSymlinks::No).nonblock(true);
                let file = current.open_with(component, &options).map_err(|error| {
                    Self::map_component_open_error(current, component, &relative_path, error)
                })?;
                let opened_metadata =
                    file.metadata()
                        .map_err(|error| FilesystemProviderError::Io {
                            message: format!(
                                "Cannot inspect opened resource {relative_path}: {error}"
                            ),
                        })?;
                if opened_metadata.is_file() && opened_metadata.nlink() == 1 {
                    let mime_type = detect_mime_type(Path::new(&relative_path));
                    let uri = self.file_uri(&relative_path)?;
                    let separator_bytes = usize::from(!entries.is_empty());
                    let line_bytes = uri
                        .len()
                        .saturating_add(LISTING_ENTRY_PREFIX.len())
                        .saturating_add(LISTING_ENTRY_MIME.len())
                        .saturating_add(mime_type.len())
                        .saturating_add(LISTING_ENTRY_SUFFIX.len())
                        .saturating_add(separator_bytes);
                    let projected_listing_bytes = (*listing_bytes).saturating_add(line_bytes);
                    if projected_listing_bytes > self.max_listing_bytes {
                        return Err(FilesystemProviderError::ListingTooLarge {
                            size: projected_listing_bytes,
                            max: self.max_listing_bytes,
                        });
                    }
                    *listing_bytes = projected_listing_bytes;
                    entries
                        .try_reserve(1)
                        .map_err(|error| FilesystemProviderError::Io {
                            message: format!("Cannot allocate filesystem entry: {error}"),
                        })?;
                    entries.push(FileEntry {
                        relative_path: relative_path.clone(),
                        uri,
                        size: Some(opened_metadata.len()),
                        mime_type,
                    });
                }
            }
        }

        Ok(())
    }

    /// Returns the canonical RFC 6570 reserved-expansion URI for a file.
    fn file_uri(&self, relative_path: &str) -> Result<String, FilesystemProviderError> {
        self.validate_path(relative_path)?;
        let encoded_path = encode_resource_path(relative_path)?;
        let base_bytes = self.prefix.as_ref().map_or(8, |prefix| {
            "file:///"
                .len()
                .saturating_add(prefix.len())
                .saturating_add(1)
        });
        let capacity = base_bytes
            .checked_add(encoded_path.len())
            .ok_or_else(path_traversal_error)?;
        let mut uri = String::new();
        uri.try_reserve_exact(capacity)
            .map_err(|error| FilesystemProviderError::Io {
                message: format!("Cannot allocate filesystem resource URI: {error}"),
            })?;
        uri.push_str("file:///");
        if let Some(prefix) = &self.prefix {
            uri.push_str(prefix);
            uri.push('/');
        }
        uri.push_str(&encoded_path);
        Ok(uri)
    }

    /// Returns the URI template for this provider.
    fn uri_template(&self) -> String {
        match &self.prefix {
            Some(prefix) => format!("file:///{prefix}/{{+path}}"),
            None => "file:///{+path}".to_string(),
        }
    }

    /// Extracts the relative path from a URI.
    fn path_from_uri(&self, uri: &str) -> Result<String, FilesystemProviderError> {
        let expected_prefix = match &self.prefix {
            Some(p) => format!("file:///{p}/"),
            None => "file:///".to_string(),
        };
        if uri.len()
            > expected_prefix
                .len()
                .saturating_add(MAX_ENCODED_RELATIVE_PATH_BYTES)
            || uri
                .chars()
                .any(|character| character.is_control() || matches!(character, '?' | '#'))
        {
            return Err(path_traversal_error());
        }

        let encoded_path = uri
            .strip_prefix(&expected_prefix)
            .ok_or_else(path_traversal_error)?;
        let path = decode_resource_path(encoded_path)?;
        self.validate_path(&path)?;
        if encode_resource_path(&path)? != encoded_path {
            return Err(path_traversal_error());
        }
        Ok(path)
    }

    /// Reads a file and returns its content.
    fn read_file(
        &self,
        ctx: &McpContext,
        relative_path: &str,
    ) -> Result<FileContent, FilesystemProviderError> {
        ctx.checkpoint()
            .map_err(|_| FilesystemProviderError::Cancelled)?;
        let requested_components = self.validate_path(relative_path)?;
        let parent_depth = requested_components.len().saturating_sub(1);
        if !self.recursive && parent_depth > 0 {
            return Err(FilesystemProviderError::NotFound {
                path: relative_path.to_string(),
            });
        }
        if self.recursive && parent_depth > self.max_depth {
            return Err(FilesystemProviderError::TooDeep {
                path: relative_path.to_string(),
                depth: parent_depth,
                max: self.max_depth,
            });
        }
        if self.has_excluded_ancestor(relative_path) || !self.matches_patterns(relative_path) {
            // Treat policy-hidden resources as absent so the read surface does
            // not disclose whether an excluded path exists.
            return Err(FilesystemProviderError::NotFound {
                path: relative_path.to_string(),
            });
        }
        let file = self.open_file_nofollow(relative_path)?;
        self.read_open_file(ctx, file, relative_path)
    }

    /// Reads only from an already-opened capability file handle.
    fn read_open_file(
        &self,
        ctx: &McpContext,
        mut file: File,
        relative_path: &str,
    ) -> Result<FileContent, FilesystemProviderError> {
        let metadata = file
            .metadata()
            .map_err(|error| FilesystemProviderError::Io {
                message: format!("Cannot inspect opened resource {relative_path}: {error}"),
            })?;

        if !metadata.is_file() {
            return Err(FilesystemProviderError::Io {
                message: format!("Resource is not a regular file: {relative_path}"),
            });
        }

        let links = metadata.nlink();
        if links != 1 {
            return Err(FilesystemProviderError::HardLinkDenied {
                path: relative_path.to_string(),
                links,
            });
        }

        if metadata.len() > self.max_file_size as u64 {
            return Err(FilesystemProviderError::TooLarge {
                path: relative_path.to_string(),
                size: metadata.len(),
                max: self.max_file_size,
            });
        }

        let mut bytes = Vec::new();
        bytes
            .try_reserve(self.max_file_size.min(64 * 1024))
            .map_err(|error| FilesystemProviderError::Io {
                message: format!("Cannot allocate buffer for resource {relative_path}: {error}"),
            })?;
        let read_limit = self.max_file_size.saturating_add(1);
        let mut chunk = Vec::new();
        chunk
            .try_reserve_exact(64 * 1024)
            .map_err(|error| FilesystemProviderError::Io {
                message: format!(
                    "Cannot allocate read buffer for resource {relative_path}: {error}"
                ),
            })?;
        chunk.resize(64 * 1024, 0);
        while bytes.len() < read_limit {
            ctx.checkpoint()
                .map_err(|_| FilesystemProviderError::Cancelled)?;
            let remaining = read_limit.saturating_sub(bytes.len());
            let chunk_len = remaining.min(chunk.len());
            let read = file.read(&mut chunk[..chunk_len]).map_err(|error| {
                FilesystemProviderError::Io {
                    message: format!("Cannot read opened resource {relative_path}: {error}"),
                }
            })?;
            if read == 0 {
                break;
            }
            bytes
                .try_reserve(read)
                .map_err(|error| FilesystemProviderError::Io {
                    message: format!(
                        "Cannot grow buffer for opened resource {relative_path}: {error}"
                    ),
                })?;
            bytes.extend_from_slice(&chunk[..read]);
        }
        if bytes.len() > self.max_file_size {
            return Err(FilesystemProviderError::TooLarge {
                path: relative_path.to_string(),
                size: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
                max: self.max_file_size,
            });
        }
        ctx.checkpoint()
            .map_err(|_| FilesystemProviderError::Cancelled)?;

        let mime_type = detect_mime_type(Path::new(relative_path));
        let content = if is_binary_mime_type(&mime_type) {
            FileContent::Binary(bytes)
        } else {
            let text = String::from_utf8(bytes).map_err(|error| FilesystemProviderError::Io {
                message: format!("Resource {relative_path} is not valid UTF-8: {error}"),
            })?;
            FileContent::Text(text)
        };

        Ok(content)
    }
}

/// A file entry from directory listing.
#[derive(Debug)]
struct FileEntry {
    relative_path: String,
    uri: String,
    #[allow(dead_code)]
    size: Option<u64>,
    mime_type: String,
}

/// File content (text or binary).
enum FileContent {
    Text(String),
    Binary(Vec<u8>),
}

/// Resource handler implementation for the filesystem provider.
#[derive(Clone)]
pub struct FilesystemResourceHandler {
    provider: FilesystemProvider,
}

impl FilesystemResourceHandler {
    /// Creates a new handler from a provider.
    #[cfg(test)]
    fn new(provider: FilesystemProvider) -> Self {
        Self { provider }
    }
}

async fn run_filesystem_blocking<T, F>(ctx: &McpContext, work: F) -> McpOutcome<T>
where
    T: Send + 'static,
    F: FnOnce(&McpContext) -> McpResult<T> + Send + 'static,
{
    let worker_ctx = ctx.clone();
    // Awaited blocking-pool offload for synchronous std::fs I/O, not runtime re-entry.
    match asupersync::runtime::spawn_blocking(move || work(&worker_ctx)).await {
        Ok(value) => Outcome::Ok(value),
        Err(error) => Outcome::Err(error),
    }
}

impl ResourceHandler for FilesystemResourceHandler {
    fn definition(&self) -> Resource {
        // Return a synthetic "root" resource for the provider
        Resource {
            uri: self.provider.uri_template(),
            name: self
                .provider
                .prefix
                .clone()
                .unwrap_or_else(|| "files".to_string()),
            description: self.provider.description.clone(),
            mime_type: None,
            icon: None,
            version: None,
            tags: vec![],
        }
    }

    fn template(&self) -> Option<ResourceTemplate> {
        Some(ResourceTemplate {
            uri_template: self.provider.uri_template(),
            name: self
                .provider
                .prefix
                .clone()
                .unwrap_or_else(|| "files".to_string()),
            description: self.provider.description.clone(),
            mime_type: None,
            icon: None,
            version: None,
            tags: vec![],
        })
    }

    fn final_template_definition(&self) -> Option<FinalResourceTemplate> {
        Some(FinalResourceTemplate {
            uri_template: self.provider.uri_template(),
            name: self
                .provider
                .prefix
                .clone()
                .unwrap_or_else(|| "files".to_string()),
            title: None,
            description: self.provider.description.clone(),
            icons: None,
            mime_type: None,
            annotations: None,
            meta: None,
        })
    }

    fn read_async<'a>(
        &'a self,
        ctx: &'a McpContext,
    ) -> BoxFuture<'a, McpOutcome<Vec<ResourceContent>>> {
        let handler = self.clone();
        Box::pin(
            async move { run_filesystem_blocking(ctx, move |child| handler.read(child)).await },
        )
    }

    fn read_async_with_uri<'a>(
        &'a self,
        ctx: &'a McpContext,
        uri: &'a str,
        params: &'a UriParams,
    ) -> BoxFuture<'a, McpOutcome<Vec<ResourceContent>>> {
        let handler = self.clone();
        let uri = uri.to_owned();
        let params = params.clone();
        Box::pin(async move {
            run_filesystem_blocking(ctx, move |child| {
                handler.read_with_uri(child, &uri, &params)
            })
            .await
        })
    }

    fn read_async_with_uri_in_request<'a>(
        &'a self,
        ctx: &'a McpContext,
        _request_cx: &'a Cx,
        uri: &'a str,
        params: &'a UriParams,
    ) -> BoxFuture<'a, McpOutcome<Vec<ResourceContent>>> {
        let handler = self.clone();
        let uri = uri.to_owned();
        let params = params.clone();
        Box::pin(async move {
            run_filesystem_blocking(ctx, move |child| {
                handler.read_with_uri(child, &uri, &params)
            })
            .await
        })
    }

    fn read_final_outcome_async_with_uri_in_request<'a>(
        &'a self,
        ctx: &'a McpContext,
        _request_cx: &'a Cx,
        uri: &'a str,
        params: &'a UriParams,
    ) -> BoxFuture<'a, McpOutcome<FinalMethodOutcome<FinalReadResourceResult>>> {
        let handler = self.clone();
        let uri = uri.to_owned();
        let params = params.clone();
        Box::pin(async move {
            run_filesystem_blocking(ctx, move |child| {
                handler.read_final_outcome_with_uri(child, &uri, &params)
            })
            .await
        })
    }

    fn read(&self, ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        // For template resources, read() without params returns the file list
        let files = self.provider.list_files(ctx)?;
        let mut listing = String::new();
        listing
            .try_reserve(self.provider.max_listing_bytes.min(64 * 1024))
            .map_err(|error| {
                McpError::internal_error(format!("Cannot allocate filesystem listing: {error}"))
            })?;
        listing.push('[');
        for (index, file) in files.into_iter().enumerate() {
            ctx.checkpoint()
                .map_err(|_| McpError::request_cancelled())?;
            let additional = usize::from(index != 0)
                .saturating_add(LISTING_ENTRY_PREFIX.len())
                .saturating_add(file.uri.len())
                .saturating_add(LISTING_ENTRY_MIME.len())
                .saturating_add(file.mime_type.len());
            let additional = additional.saturating_add(LISTING_ENTRY_SUFFIX.len());
            listing.try_reserve(additional).map_err(|error| {
                McpError::internal_error(format!("Cannot grow filesystem listing: {error}"))
            })?;
            if index != 0 {
                listing.push(',');
            }
            listing.push_str(LISTING_ENTRY_PREFIX);
            listing.push_str(&file.uri);
            listing.push_str(LISTING_ENTRY_MIME);
            listing.push_str(&file.mime_type);
            listing.push_str(LISTING_ENTRY_SUFFIX);
        }
        listing.push(']');
        if listing.len() > self.provider.max_listing_bytes {
            return Err(FilesystemProviderError::ListingTooLarge {
                size: listing.len(),
                max: self.provider.max_listing_bytes,
            }
            .into());
        }

        Ok(vec![ResourceContent {
            uri: self.provider.uri_template(),
            mime_type: Some("application/json".to_string()),
            text: Some(listing),
            blob: None,
        }])
    }

    fn read_with_uri(
        &self,
        ctx: &McpContext,
        uri: &str,
        params: &UriParams,
    ) -> McpResult<Vec<ResourceContent>> {
        // The URI is the resource identity. Template parameters may repeat
        // that identity, but they cannot select a different path.
        let relative_path = self.provider.path_from_uri(uri)?;
        if let Some(path) = params.get("path")
            && path != &relative_path
        {
            return Err(McpError::invalid_params(
                "URI path and template path parameter do not match",
            ));
        }

        let content = self.provider.read_file(ctx, &relative_path)?;

        let resource_content = match content {
            FileContent::Text(text) => ResourceContent {
                uri: uri.to_string(),
                mime_type: Some(detect_mime_type(Path::new(&relative_path))),
                text: Some(text),
                blob: None,
            },
            FileContent::Binary(bytes) => {
                let base64_str = base64_encode(&bytes)?;

                ResourceContent {
                    uri: uri.to_string(),
                    mime_type: Some(detect_mime_type(Path::new(&relative_path))),
                    text: None,
                    blob: Some(base64_str),
                }
            }
        };

        Ok(vec![resource_content])
    }
}

impl std::fmt::Debug for FilesystemResourceHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FilesystemResourceHandler")
            .field("provider", &self.provider)
            .finish()
    }
}

/// Detects the MIME type for a file based on its extension.
fn detect_mime_type(path: &Path) -> String {
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_lowercase);

    match extension.as_deref() {
        // Text formats
        Some("txt") => "text/plain",
        Some("md" | "markdown") => "text/markdown",
        Some("html" | "htm") => "text/html",
        Some("css") => "text/css",
        Some("csv") => "text/csv",
        Some("xml") => "application/xml",

        // Programming languages
        Some("rs") => "text/x-rust",
        Some("py") => "text/x-python",
        Some("js" | "mjs") => "text/javascript",
        Some("ts" | "mts") => "text/typescript",
        Some("json") => "application/json",
        Some("yaml" | "yml") => "application/yaml",
        Some("toml") => "application/toml",
        Some("sh" | "bash") => "text/x-shellscript",
        Some("c") => "text/x-c",
        Some("cpp" | "cc" | "cxx") => "text/x-c++",
        Some("h" | "hpp") => "text/x-c-header",
        Some("java") => "text/x-java",
        Some("go") => "text/x-go",
        Some("rb") => "text/x-ruby",
        Some("php") => "text/x-php",
        Some("swift") => "text/x-swift",
        Some("kt" | "kts") => "text/x-kotlin",
        Some("sql") => "text/x-sql",

        // Images
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("svg") => "image/svg+xml",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("bmp") => "image/bmp",

        // Binary/Documents
        Some("pdf") => "application/pdf",
        Some("zip") => "application/zip",
        Some("gz" | "gzip") => "application/gzip",
        Some("tar") => "application/x-tar",
        Some("wasm") => "application/wasm",
        Some("exe") => "application/octet-stream",
        Some("dll") => "application/octet-stream",
        Some("so") => "application/octet-stream",
        Some("bin") => "application/octet-stream",

        // Default
        _ => "application/octet-stream",
    }
    .to_string()
}

/// Checks if a MIME type represents binary content.
fn is_binary_mime_type(mime_type: &str) -> bool {
    mime_type.starts_with("image/")
        || mime_type.starts_with("audio/")
        || mime_type.starts_with("video/")
        || mime_type == "application/octet-stream"
        || mime_type == "application/pdf"
        || mime_type == "application/zip"
        || mime_type == "application/gzip"
        || mime_type == "application/x-tar"
        || mime_type == "application/wasm"
}

/// Standard base64 alphabet.
const BASE64_CHARS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encodes bytes to standard base64.
fn base64_encode(data: &[u8]) -> Result<String, FilesystemProviderError> {
    if data.len() > MAX_CONFIGURED_FILE_SIZE {
        return Err(FilesystemProviderError::TooLarge {
            path: "<binary-resource>".to_string(),
            size: u64::try_from(data.len()).unwrap_or(u64::MAX),
            max: MAX_CONFIGURED_FILE_SIZE,
        });
    }
    let encoded_len = data
        .len()
        .checked_add(2)
        .map(|length| length / 3)
        .and_then(|length| length.checked_mul(4))
        .filter(|length| *length <= MAX_ENCODED_BINARY_BYTES)
        .ok_or(FilesystemProviderError::TooLarge {
            path: "<binary-resource>".to_string(),
            size: u64::try_from(data.len()).unwrap_or(u64::MAX),
            max: MAX_CONFIGURED_FILE_SIZE,
        })?;
    let mut result = String::new();
    result
        .try_reserve_exact(encoded_len)
        .map_err(|error| FilesystemProviderError::Io {
            message: format!("Cannot allocate encoded binary resource: {error}"),
        })?;

    for chunk in data.chunks(3) {
        let b0 = chunk[0] as usize;
        let b1 = chunk.get(1).copied().unwrap_or(0) as usize;
        let b2 = chunk.get(2).copied().unwrap_or(0) as usize;

        let combined = (b0 << 16) | (b1 << 8) | b2;

        result.push(BASE64_CHARS[(combined >> 18) & 0x3F] as char);
        result.push(BASE64_CHARS[(combined >> 12) & 0x3F] as char);

        if chunk.len() > 1 {
            result.push(BASE64_CHARS[(combined >> 6) & 0x3F] as char);
        } else {
            result.push('=');
        }

        if chunk.len() > 2 {
            result.push(BASE64_CHARS[combined & 0x3F] as char);
        } else {
            result.push('=');
        }
    }

    Ok(result)
}

#[derive(Clone, Copy)]
enum GlobToken {
    Literal(char),
    AnyCharacter,
    AnySegmentSequence,
    AnyRecursivePrefix,
    AnyRecursiveSequence,
}

fn compile_glob_tokens(pattern: &str) -> Option<Vec<GlobToken>> {
    if pattern.len() > MAX_GLOB_PATTERN_BYTES || pattern.chars().any(char::is_control) {
        return None;
    }
    let character_count = pattern.chars().count();
    let mut characters = Vec::new();
    characters.try_reserve_exact(character_count).ok()?;
    characters.extend(pattern.chars());

    let mut tokens = Vec::new();
    tokens.try_reserve_exact(character_count).ok()?;
    let mut index = 0_usize;
    let mut wildcard_count = 0_usize;
    while index < characters.len() {
        match characters[index] {
            '?' => {
                wildcard_count = wildcard_count.checked_add(1)?;
                tokens.push(GlobToken::AnyCharacter);
                index += 1;
            }
            '*' if characters.get(index + 1) == Some(&'*') => {
                wildcard_count = wildcard_count.checked_add(1)?;
                if (index != 0 && characters[index - 1] != '/')
                    || characters
                        .get(index + 2)
                        .is_some_and(|character| *character != '/')
                {
                    return None;
                }
                let has_separator = characters.get(index + 2) == Some(&'/');
                tokens.push(if has_separator {
                    GlobToken::AnyRecursivePrefix
                } else {
                    GlobToken::AnyRecursiveSequence
                });
                index += 2;
                if has_separator {
                    index += 1;
                }
            }
            '*' => {
                wildcard_count = wildcard_count.checked_add(1)?;
                tokens.push(GlobToken::AnySegmentSequence);
                index += 1;
            }
            literal => {
                tokens.push(GlobToken::Literal(literal));
                index += 1;
            }
        }
        if wildcard_count > MAX_GLOB_WILDCARDS_PER_PATTERN {
            return None;
        }
    }
    Some(tokens)
}

fn admit_glob_patterns(patterns: &[&str]) -> Option<Vec<String>> {
    if patterns.len() > MAX_GLOB_PATTERNS {
        return None;
    }
    let mut admitted = Vec::new();
    admitted.try_reserve_exact(patterns.len()).ok()?;
    let mut total_bytes = 0_usize;
    for pattern in patterns {
        total_bytes = total_bytes.checked_add(pattern.len())?;
        if total_bytes > MAX_TOTAL_GLOB_PATTERN_BYTES || compile_glob_tokens(pattern).is_none() {
            return None;
        }
        let mut owned = String::new();
        owned.try_reserve_exact(pattern.len()).ok()?;
        owned.push_str(pattern);
        admitted.push(owned);
    }
    Some(admitted)
}

/// Bounded, polynomial-time glob matching.
///
/// `*` and `?` do not cross `/`; a whole-component `**` does. The dynamic
/// program replaces the prior recursive backtracking implementation so a
/// hostile pattern/path pair cannot trigger exponential work.
fn glob_match(pattern: &str, path: &str) -> bool {
    let Some(tokens) = compile_glob_tokens(pattern) else {
        return false;
    };
    let path_character_count = path.chars().count();
    let mut path_characters = Vec::new();
    if path_characters
        .try_reserve_exact(path_character_count)
        .is_err()
    {
        return false;
    }
    path_characters.extend(path.chars());
    let row_len = match path_characters.len().checked_add(1) {
        Some(row_len) => row_len,
        None => return false,
    };
    let mut previous = Vec::new();
    let mut current = Vec::new();
    if previous.try_reserve_exact(row_len).is_err() || current.try_reserve_exact(row_len).is_err() {
        return false;
    }
    previous.resize(row_len, false);
    current.resize(row_len, false);
    previous[0] = true;

    for token in tokens {
        match token {
            GlobToken::Literal(literal) => {
                for index in 1..row_len {
                    current[index] = previous[index - 1] && path_characters[index - 1] == literal;
                }
            }
            GlobToken::AnyCharacter => {
                for index in 1..row_len {
                    current[index] = previous[index - 1] && path_characters[index - 1] != '/';
                }
            }
            GlobToken::AnySegmentSequence => {
                current[0] = previous[0];
                for index in 1..row_len {
                    current[index] = previous[index]
                        || (current[index - 1] && path_characters[index - 1] != '/');
                }
            }
            GlobToken::AnyRecursiveSequence => {
                current[0] = previous[0];
                for index in 1..row_len {
                    current[index] = previous[index] || current[index - 1];
                }
            }
            GlobToken::AnyRecursivePrefix => {
                current[0] = previous[0];
                let mut reachable = previous[0];
                for index in 1..row_len {
                    reachable |= previous[index];
                    current[index] =
                        previous[index] || (reachable && path_characters[index - 1] == '/');
                }
            }
        }
        std::mem::swap(&mut previous, &mut current);
        current.fill(false);
    }
    previous[path_characters.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEST_DIR_SEQ: AtomicU64 = AtomicU64::new(1);

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new(label: &str) -> Self {
            let mut path = std::env::temp_dir();
            let seq = TEST_DIR_SEQ.fetch_add(1, Ordering::SeqCst);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock before epoch")
                .as_nanos();
            path.push(format!(
                "fastmcp-fs-tests-{label}-{}-{seq}-{nanos}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("create temp test dir");
            Self { path }
        }

        fn join(&self, relative: &str) -> PathBuf {
            self.path.join(relative)
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn write_text(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dir");
        }
        std::fs::write(path, content).expect("write text file");
    }

    fn write_bytes(path: &Path, bytes: &[u8]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dir");
        }
        std::fs::write(path, bytes).expect("write binary file");
    }

    fn test_context() -> McpContext {
        McpContext::new(asupersync::Cx::for_testing(), 1)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn public_build_constructs_a_handler_on_qualified_targets() {
        let root = TestDir::new("public-promotion-gate");
        write_text(&root.join("ordinary.txt"), "ordinary");

        let handler = FilesystemProvider::new(root.path())
            .build()
            .expect("Linux and macOS construct a production filesystem handler");
        let listing = handler
            .read(&test_context())
            .expect("constructed handler can list the root");
        assert_eq!(listing.len(), 1);
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    #[test]
    fn public_build_fails_closed_on_unqualified_targets() {
        let root = TestDir::new("public-promotion-gate");
        write_text(&root.join("ordinary.txt"), "ordinary");

        let error = FilesystemProvider::new(root.path())
            .build()
            .expect_err("unqualified targets remain fail-closed");

        assert!(matches!(
            error,
            FilesystemProviderError::FeatureUnavailable { platform }
                if platform == FILESYSTEM_PROVIDER_PROMOTION_GATE
                || platform == std::env::consts::OS
        ));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn public_build_does_not_probe_an_unusable_root() {
        let root = TestDir::new("missing-root");
        let missing = root.join("does-not-exist");

        let error = FilesystemProvider::new(missing)
            .build()
            .expect_err("a missing root cannot construct a handler");

        assert!(matches!(error, FilesystemProviderError::Io { .. }));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn build_rejects_configuration_outside_hard_safety_bounds() {
        let root = TestDir::new("invalid-config");

        for provider in [
            FilesystemProvider::new(root.path()).with_max_size(MAX_CONFIGURED_FILE_SIZE + 1),
            FilesystemProvider::new(root.path()).with_max_entries(0),
            FilesystemProvider::new(root.path()).with_max_entries(MAX_CONFIGURED_ENTRIES + 1),
            FilesystemProvider::new(root.path()).with_max_depth(MAX_CONFIGURED_DEPTH + 1),
            FilesystemProvider::new(root.path()).with_max_listing_bytes(0),
            FilesystemProvider::new(root.path()).with_max_listing_bytes(1),
            FilesystemProvider::new(root.path())
                .with_max_listing_bytes(MAX_CONFIGURED_LISTING_BYTES + 1),
        ] {
            assert!(matches!(
                provider.build(),
                Err(FilesystemProviderError::InvalidConfiguration { .. })
            ));
        }

        for prefix in ["", "contains/slash", "contains?query", "contains#fragment"] {
            assert!(matches!(
                FilesystemProvider::new(root.path())
                    .with_prefix(prefix)
                    .build(),
                Err(FilesystemProviderError::InvalidConfiguration { field: "prefix" })
            ));
        }

        let long_pattern = "x".repeat(MAX_GLOB_PATTERN_BYTES + 1);
        let too_many_patterns = vec!["*.txt"; MAX_GLOB_PATTERNS + 1];
        for provider in [
            FilesystemProvider::new(root.path()).with_patterns(&[long_pattern.as_str()]),
            FilesystemProvider::new(root.path()).with_patterns(&too_many_patterns),
            FilesystemProvider::new(root.path()).with_patterns(&["prefix**suffix"]),
            FilesystemProvider::new(root.path())
                .with_description("x".repeat(MAX_DESCRIPTION_BYTES + 1)),
            FilesystemProvider::new(root.path()).with_description("forged\nlabel"),
            FilesystemProvider::new(root.path()).with_description("directional\u{202e}label"),
        ] {
            assert!(matches!(
                provider.build(),
                Err(FilesystemProviderError::InvalidConfiguration { .. })
            ));
        }
    }

    #[test]
    fn test_glob_match_star() {
        assert!(glob_match("*.md", "readme.md"));
        assert!(glob_match("*.md", "CHANGELOG.md"));
        assert!(!glob_match("*.md", "readme.txt"));
        assert!(!glob_match("*.md", "dir/readme.md")); // * doesn't match /
    }

    #[test]
    fn test_glob_match_double_star() {
        assert!(glob_match("**/*.md", "readme.md"));
        assert!(glob_match("**/*.md", "docs/readme.md"));
        assert!(glob_match("**/*.md", "docs/api/readme.md"));
        assert!(!glob_match("**/*.md", "readme.txt"));
    }

    #[test]
    fn test_glob_match_question() {
        assert!(glob_match("file?.txt", "file1.txt"));
        assert!(glob_match("file?.txt", "fileA.txt"));
        assert!(!glob_match("file?.txt", "file12.txt"));
    }

    #[test]
    fn test_glob_match_hidden() {
        assert!(glob_match(".*", ".hidden"));
        assert!(glob_match(".*", ".gitignore"));
        assert!(!glob_match(".*", "visible"));
        assert!(glob_match("**/.*", "nested/.hidden"));
        assert!(!glob_match("**/.*", "nested/readme.md"));
    }

    #[test]
    fn glob_match_rejects_ambiguous_recursive_wildcards() {
        assert!(!glob_match("prefix**suffix", "prefix-any-suffix"));
        assert!(!glob_match("***", "anything"));
    }

    #[test]
    fn test_glob_match_uses_utf8_character_boundaries() {
        assert!(glob_match("*.md", "résumé.md"));
        assert!(glob_match("**/*.md", "資料/概要.md"));
        assert!(glob_match("file?.txt", "file界.txt"));
        assert!(!glob_match("*.txt", "資料/概要.txt"));
    }

    #[test]
    fn test_detect_mime_type() {
        assert_eq!(detect_mime_type(Path::new("file.md")), "text/markdown");
        assert_eq!(detect_mime_type(Path::new("file.json")), "application/json");
        assert_eq!(detect_mime_type(Path::new("file.rs")), "text/x-rust");
        assert_eq!(detect_mime_type(Path::new("file.png")), "image/png");
        assert_eq!(
            detect_mime_type(Path::new("file.unknown")),
            "application/octet-stream"
        );
    }

    #[test]
    fn test_is_binary_mime_type() {
        assert!(is_binary_mime_type("image/png"));
        assert!(is_binary_mime_type("application/pdf"));
        assert!(!is_binary_mime_type("text/plain"));
        assert!(!is_binary_mime_type("application/json"));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn test_provider_list_files_respects_patterns_and_recursion() {
        let root = TestDir::new("list-recursive");
        write_text(&root.join("README.md"), "# readme");
        write_text(&root.join("notes.txt"), "notes");
        write_text(&root.join("nested/info.md"), "# nested");
        write_text(&root.join("nested/code.rs"), "fn main() {}");

        let provider = FilesystemProvider::new(root.path())
            .with_patterns(&["**/*.md", "**/*.txt"])
            .with_recursive(true);

        let files = provider.list_files(&test_context()).expect("list files");
        let mut relative_paths = files
            .iter()
            .map(|entry| entry.relative_path.as_str())
            .collect::<Vec<_>>();
        relative_paths.sort_unstable();

        assert_eq!(
            relative_paths,
            vec!["README.md", "nested/info.md", "notes.txt"]
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn test_provider_list_files_non_recursive_skips_subdirectories() {
        let root = TestDir::new("list-flat");
        write_text(&root.join("root.md"), "root");
        write_text(&root.join("nested/child.md"), "child");

        let provider = FilesystemProvider::new(root.path())
            .with_patterns(&["**/*.md"])
            .with_recursive(false);

        let files = provider.list_files(&test_context()).expect("list files");
        let relative_paths = files
            .iter()
            .map(|entry| entry.relative_path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(relative_paths, vec!["root.md"]);
    }

    #[test]
    fn test_validate_path_rejects_absolute_and_parent_escape() {
        let root = TestDir::new("validate-path");
        write_text(&root.join("safe.txt"), "safe");

        let outside_file = root
            .path()
            .parent()
            .expect("temp dir has parent")
            .join("outside-fastmcp-provider-test.txt");
        write_text(&outside_file, "outside");

        let provider = FilesystemProvider::new(root.path());

        // Use a platform-appropriate absolute path: on Windows
        // `/tmp/absolute.txt` is *not* absolute (no drive letter), so the
        // hard-coded Unix string fails to exercise the absolute-path
        // rejection branch and the assertion below misfires.
        let absolute_input = if cfg!(windows) {
            r"C:\Windows\System32\absolute.txt"
        } else {
            "/tmp/absolute.txt"
        };
        let absolute = provider.validate_path(absolute_input);
        assert!(matches!(
            absolute,
            Err(FilesystemProviderError::PathTraversal { .. })
        ));

        let escape = provider.validate_path("../outside-fastmcp-provider-test.txt");
        assert!(matches!(
            escape,
            Err(FilesystemProviderError::PathTraversal { .. })
        ));

        for aliased in ["safe.txt/", "nested//safe.txt"] {
            assert!(matches!(
                provider.validate_path(aliased),
                Err(FilesystemProviderError::PathTraversal { .. })
            ));
        }

        let ok = provider.validate_path("safe.txt").expect("safe path");
        assert_eq!(ok, vec![OsString::from("safe.txt")]);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn test_read_file_text_binary_and_size_limit() {
        let root = TestDir::new("read-file");
        write_text(&root.join("doc.txt"), "hello world");
        write_bytes(&root.join("blob.bin"), &[0x00, 0x7F, 0xAA, 0x55]);
        write_bytes(&root.join("large.bin"), &[0u8; 8]);

        let provider = FilesystemProvider::new(root.path()).with_max_size(32);

        let text = provider
            .read_file(&test_context(), "doc.txt")
            .expect("read text");
        assert!(matches!(text, FileContent::Text(ref t) if t == "hello world"));

        let binary = provider
            .read_file(&test_context(), "blob.bin")
            .expect("read binary");
        assert!(matches!(binary, FileContent::Binary(ref b) if b == &[0x00, 0x7F, 0xAA, 0x55]));

        let size_limited = FilesystemProvider::new(root.path()).with_max_size(4);
        let too_large = size_limited.read_file(&test_context(), "large.bin");
        assert!(matches!(
            too_large,
            Err(FilesystemProviderError::TooLarge { path, size: 8, max: 4 })
                if path == "large.bin"
        ));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn test_handler_read_listing_and_read_with_uri() {
        let root = TestDir::new("handler-read");
        write_text(&root.join("docs/readme.md"), "# docs");

        let handler = FilesystemProvider::new(root.path())
            .with_prefix("docs")
            .with_patterns(&["**/*.md"])
            .with_recursive(true)
            .with_description("Documentation")
            .build_for_test()
            .expect("valid filesystem provider");

        let ctx = McpContext::new(asupersync::Cx::for_testing(), 1);

        let definition = handler.definition();
        assert_eq!(definition.uri, "file:///docs/{+path}");
        assert_eq!(definition.name, "docs");
        assert_eq!(definition.description.as_deref(), Some("Documentation"));

        let template = handler.template().expect("resource template");
        assert_eq!(template.uri_template, "file:///docs/{+path}");

        let listing = handler.read(&ctx).expect("read listing");
        assert_eq!(listing[0].mime_type.as_deref(), Some("application/json"));
        let listing_text = listing[0].text.as_deref().expect("listing text");
        let listing_json: serde_json::Value =
            serde_json::from_str(listing_text).expect("valid JSON listing");
        assert_eq!(
            listing_json,
            serde_json::json!([{
                "uri": "file:///docs/docs/readme.md",
                "mimeType": "text/markdown"
            }])
        );

        let mut params = HashMap::new();
        params.insert("path".to_string(), "docs/readme.md".to_string());
        let content = handler
            .read_with_uri(&ctx, "file:///docs/docs/readme.md", &params)
            .expect("read with params");
        assert_eq!(content[0].text.as_deref(), Some("# docs"));

        let empty_params = HashMap::new();
        let content_from_uri = handler
            .read_with_uri(&ctx, "file:///docs/docs/readme.md", &empty_params)
            .expect("read using uri path");
        assert_eq!(content_from_uri[0].text.as_deref(), Some("# docs"));

        let invalid = handler.read_with_uri(&ctx, "file:///wrong-prefix/readme.md", &empty_params);
        assert!(invalid.is_err());

        params.insert("path".to_string(), "different.md".to_string());
        let mismatch = handler.read_with_uri(&ctx, "file:///docs/docs/readme.md", &params);
        assert_eq!(
            mismatch
                .expect_err("URI and template parameter must identify the same resource")
                .code,
            fastmcp_core::McpErrorCode::InvalidParams
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn listing_is_deterministic_and_omits_control_bearing_names() {
        let root = TestDir::new("deterministic-listing");
        write_text(&root.join("b.txt"), "b");
        write_text(&root.join("a.txt"), "a");
        write_text(&root.join("forged\nentry.txt"), "hidden from URI surface");
        write_text(&root.join("directional\u{202e}.txt"), "encoded safely");

        let handler = FilesystemProvider::new(root.path())
            .with_exclude(&[])
            .build_for_test()
            .expect("valid filesystem provider");
        let listing = handler.read(&test_context()).expect("bounded listing");

        let text = listing[0].text.as_deref().expect("JSON listing");
        assert_eq!(
            text,
            "[{\"uri\":\"file:///a.txt\",\"mimeType\":\"text/plain\"},{\"uri\":\"file:///b.txt\",\"mimeType\":\"text/plain\"},{\"uri\":\"file:///directional%E2%80%AE.txt\",\"mimeType\":\"text/plain\"}]"
        );
        assert!(!text.contains('\u{202e}'));
        serde_json::from_str::<serde_json::Value>(text).expect("listing must remain valid JSON");
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn direct_read_of_fifo_fails_without_waiting_for_a_writer() {
        let root = TestDir::new("fifo");
        let fifo = root.join("pipe.bin");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("invoke mkfifo");
        assert!(status.success(), "mkfifo must create the test fixture");

        let provider = FilesystemProvider::new(root.path()).with_exclude(&[]);
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let _ = sender.send(provider.read_file(&test_context(), "pipe.bin"));
        });
        let result = receiver
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("nonblocking FIFO read must complete promptly");

        assert!(matches!(result, Err(FilesystemProviderError::Io { .. })));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn direct_reads_cannot_bypass_include_or_exclude_policy() {
        let root = TestDir::new("direct-policy");
        write_text(&root.join("visible.md"), "visible");
        write_text(&root.join("excluded.txt"), "excluded by include policy");
        write_text(
            &root.join(".secret.md"),
            "excluded by default hidden policy",
        );
        write_text(
            &root.join("nested/.private/secret.md"),
            "excluded hidden-directory descendant",
        );

        let provider = FilesystemProvider::new(root.path())
            .with_patterns(&["**/*.md"])
            .with_recursive(true);

        assert!(matches!(
            provider.read_file(&test_context(), "visible.md"),
            Ok(FileContent::Text(_))
        ));
        for denied in ["excluded.txt", ".secret.md", "nested/.private/secret.md"] {
            assert!(matches!(
                provider.read_file(&test_context(), denied),
                Err(FilesystemProviderError::NotFound { path }) if path == denied
            ));
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn test_handler_read_async_with_uri() {
        let root = TestDir::new("handler-async");
        write_text(&root.join("notes.md"), "async content");

        let handler = FilesystemProvider::new(root.path())
            .with_patterns(&["*.md"])
            .build_for_test()
            .expect("valid filesystem provider");
        let ctx = McpContext::new(asupersync::Cx::for_testing(), 9);

        let mut params = HashMap::new();
        params.insert("path".to_string(), "notes.md".to_string());
        let outcome =
            fastmcp_core::block_on(handler.read_async_with_uri(&ctx, "file:///notes.md", &params));
        match outcome {
            Outcome::Ok(content) => {
                assert_eq!(content.len(), 1);
                assert_eq!(content[0].text.as_deref(), Some("async content"));
            }
            other => panic!("unexpected async outcome: {other:?}"),
        }
    }

    #[test]
    fn test_base64_encode_padding_variants() {
        assert_eq!(base64_encode(b"").unwrap(), "");
        assert_eq!(base64_encode(b"f").unwrap(), "Zg==");
        assert_eq!(base64_encode(b"fo").unwrap(), "Zm8=");
        assert_eq!(base64_encode(b"foo").unwrap(), "Zm9v");
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn test_symlink_components_are_always_denied() {
        use std::os::unix::fs::symlink;

        let root = TestDir::new("symlink-root");
        let outside = TestDir::new("symlink-outside");

        write_text(&root.join("inside.txt"), "inside");
        write_text(&outside.join("outside.txt"), "outside");

        let inside_link = root.join("inside-link.txt");
        let escape_link = root.join("escape-link.txt");
        let escape_directory_link = root.join("escape-directory");
        symlink(root.join("inside.txt"), &inside_link).expect("create inside symlink");
        symlink(outside.join("outside.txt"), &escape_link).expect("create escape symlink");
        symlink(outside.path(), &escape_directory_link).expect("create directory escape symlink");

        let provider = FilesystemProvider::new(root.path()).with_recursive(true);
        let denied = provider.read_file(&test_context(), "inside-link.txt");
        assert!(matches!(
            denied,
            Err(FilesystemProviderError::SymlinkDenied { .. })
        ));
        let escaped = provider.read_file(&test_context(), "escape-link.txt");
        assert!(matches!(
            escaped,
            Err(FilesystemProviderError::SymlinkDenied { .. })
        ));
        let intermediate_escape =
            provider.read_file(&test_context(), "escape-directory/outside.txt");
        assert!(matches!(
            intermediate_escape,
            Err(FilesystemProviderError::SymlinkDenied { .. })
        ));

        let listed = provider
            .list_files(&test_context())
            .expect("secure listing");
        assert_eq!(
            listed
                .iter()
                .map(|entry| entry.relative_path.as_str())
                .collect::<Vec<_>>(),
            vec!["inside.txt"]
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn test_open_handle_survives_final_component_symlink_swap() {
        use std::os::unix::fs::symlink;

        let root = TestDir::new("symlink-swap-root");
        let outside = TestDir::new("symlink-swap-outside");
        write_text(&root.join("victim.txt"), "inside");
        write_text(&outside.join("secret.txt"), "outside-secret");

        let provider = FilesystemProvider::new(root.path());
        let opened = provider
            .open_file_nofollow("victim.txt")
            .expect("open retained capability handle");

        std::fs::rename(root.join("victim.txt"), root.join("retained.txt"))
            .expect("rename original after handle acquisition");
        symlink(outside.join("secret.txt"), root.join("victim.txt"))
            .expect("replace request name with escaping symlink");

        let content = provider
            .read_open_file(&test_context(), opened, "victim.txt")
            .expect("read already-opened handle");
        assert!(matches!(content, FileContent::Text(ref text) if text == "inside"));
        assert!(matches!(
            provider.read_file(&test_context(), "victim.txt"),
            Err(FilesystemProviderError::SymlinkDenied { .. })
        ));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn test_retained_root_handle_survives_ambient_root_replacement() {
        let outer = TestDir::new("root-swap");
        let served = outer.join("served");
        std::fs::create_dir(&served).expect("create served root");
        write_text(&served.join("value.txt"), "retained-root");

        let provider = FilesystemProvider::new(&served);
        std::fs::rename(&served, outer.join("retained-root"))
            .expect("rename served root after capability acquisition");
        std::fs::create_dir(&served).expect("create ambient replacement root");
        write_text(&served.join("value.txt"), "ambient-replacement");

        let content = provider
            .read_file(&test_context(), "value.txt")
            .expect("read through retained root handle");
        assert!(matches!(content, FileContent::Text(ref text) if text == "retained-root"));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn test_multi_link_file_is_not_exposed() {
        let root = TestDir::new("hardlink-root");
        let outside = TestDir::new("hardlink-outside");
        write_text(&outside.join("shared.txt"), "shared");
        std::fs::hard_link(outside.join("shared.txt"), root.join("shared.txt"))
            .expect("create hard link into provider root");

        let provider = FilesystemProvider::new(root.path());
        assert!(matches!(
            provider.read_file(&test_context(), "shared.txt"),
            Err(FilesystemProviderError::HardLinkDenied { links, .. }) if links >= 2
        ));
        assert!(
            provider
                .list_files(&test_context())
                .expect("list files")
                .is_empty()
        );
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    #[test]
    fn unqualified_target_fails_closed() {
        let root = TestDir::new("unsupported-platform");
        write_text(&root.join("ordinary.txt"), "ordinary");
        let provider = FilesystemProvider::new(root.path());

        assert!(matches!(
            provider.list_files(&test_context()),
            Err(FilesystemProviderError::FeatureUnavailable { .. })
        ));
        assert!(matches!(
            provider.read_file(&test_context(), "ordinary.txt"),
            Err(FilesystemProviderError::FeatureUnavailable { .. })
        ));
    }

    // ── FilesystemProviderError ────────────────────────────────────

    #[test]
    fn error_path_traversal_display() {
        let err = FilesystemProviderError::PathTraversal {
            requested: "../etc/passwd".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("Path traversal attempt blocked"));
        assert!(msg.contains("../etc/passwd"));
    }

    #[test]
    fn error_too_large_display() {
        let err = FilesystemProviderError::TooLarge {
            path: "big.bin".to_string(),
            size: 50_000_000,
            max: 10_000_000,
        };
        let msg = err.to_string();
        assert!(msg.contains("File too large"));
        assert!(msg.contains("big.bin"));
        assert!(msg.contains("50000000"));
        assert!(msg.contains("10000000"));
    }

    #[test]
    fn error_symlink_denied_display() {
        let err = FilesystemProviderError::SymlinkDenied {
            path: "link.txt".to_string(),
        };
        assert!(err.to_string().contains("Symlink access denied"));
    }

    #[test]
    fn error_hard_link_denied_display() {
        let err = FilesystemProviderError::HardLinkDenied {
            path: "aliased.txt".to_string(),
            links: 2,
        };
        let message = err.to_string();
        assert!(message.contains("Hard-linked file access denied"));
        assert!(message.contains("aliased.txt"));
        assert!(message.contains("2 links"));
    }

    #[test]
    fn error_io_display() {
        let err = FilesystemProviderError::Io {
            message: "permission denied".to_string(),
        };
        assert!(err.to_string().contains("IO error"));
        assert!(err.to_string().contains("permission denied"));
    }

    #[test]
    fn error_not_found_display() {
        let err = FilesystemProviderError::NotFound {
            path: "missing.txt".to_string(),
        };
        assert!(err.to_string().contains("File not found"));
        assert!(err.to_string().contains("missing.txt"));
    }

    #[test]
    fn error_debug() {
        let err = FilesystemProviderError::PathTraversal {
            requested: "x".to_string(),
        };
        let debug = format!("{:?}", err);
        assert!(debug.contains("PathTraversal"));
    }

    #[test]
    fn error_clone() {
        let err = FilesystemProviderError::NotFound {
            path: "a.txt".to_string(),
        };
        let cloned = err.clone();
        assert!(cloned.to_string().contains("a.txt"));
    }

    #[test]
    fn error_std_error() {
        let err = FilesystemProviderError::Io {
            message: "oops".to_string(),
        };
        let std_err: &dyn std::error::Error = &err;
        assert!(std_err.to_string().contains("oops"));
    }

    // ── From<FilesystemProviderError> for McpError ─────────────────

    #[test]
    fn error_into_mcp_error_path_traversal() {
        let err = FilesystemProviderError::PathTraversal {
            requested: "forged\npeer-path".to_string(),
        };
        let mcp: McpError = err.into();
        assert_eq!(mcp.message, "Filesystem resource path was rejected");
        assert!(!mcp.message.contains("peer-path"));
    }

    #[test]
    fn error_into_mcp_error_too_large() {
        let err = FilesystemProviderError::TooLarge {
            path: "forged\u{202e}.bin".to_string(),
            size: 100,
            max: 10,
        };
        let mcp: McpError = err.into();
        assert_eq!(
            mcp.message,
            "Filesystem resource exceeds the size limit: 100 > 10 bytes"
        );
        assert!(!mcp.message.contains('\u{202e}'));
    }

    #[test]
    fn error_into_mcp_error_symlink_denied() {
        let err = FilesystemProviderError::SymlinkDenied {
            path: "x".to_string(),
        };
        let mcp: McpError = err.into();
        assert_eq!(mcp.message, "Filesystem resource link access was rejected");
    }

    #[test]
    fn error_into_mcp_error_io() {
        let err = FilesystemProviderError::Io {
            message: "disk fail".to_string(),
        };
        let mcp: McpError = err.into();
        assert_eq!(mcp.message, "Filesystem resource operation failed");
    }

    #[test]
    fn error_into_mcp_error_not_found() {
        let err = FilesystemProviderError::NotFound {
            path: "gone.txt".to_string(),
        };
        let mcp: McpError = err.into();
        assert!(mcp.message.contains(REDACTED_RESOURCE_PATH));
        assert!(!mcp.message.contains("gone.txt"));
    }

    // ── FilesystemProvider construction and builders ───────────────

    #[test]
    fn provider_new_defaults() {
        let root = TestDir::new("defaults");
        let provider = FilesystemProvider::new(root.path());
        assert_eq!(provider.root, root.path().to_path_buf());
        assert!(provider.prefix.is_none());
        assert_eq!(provider.include_patterns.len(), 0);
        assert_eq!(
            provider.exclude_patterns,
            vec![".*".to_string(), "**/.*".to_string()]
        );
        assert!(!provider.recursive);
        assert_eq!(provider.max_file_size, DEFAULT_MAX_SIZE);
        assert!(provider.description.is_none());
    }

    #[test]
    fn provider_with_prefix() {
        let provider = FilesystemProvider::new("/tmp").with_prefix("myprefix");
        assert_eq!(provider.prefix, Some("myprefix".to_string()));
    }

    #[test]
    fn provider_with_patterns() {
        let provider = FilesystemProvider::new("/tmp").with_patterns(&["*.md", "*.txt"]);
        assert_eq!(provider.include_patterns, vec!["*.md", "*.txt"]);
    }

    #[test]
    fn provider_with_exclude() {
        let provider = FilesystemProvider::new("/tmp").with_exclude(&["*.bak", "*.tmp"]);
        // Default hidden file pattern should be replaced
        assert_eq!(provider.exclude_patterns, vec!["*.bak", "*.tmp"]);
    }

    #[test]
    fn provider_with_recursive() {
        let provider = FilesystemProvider::new("/tmp").with_recursive(true);
        assert!(provider.recursive);
    }

    #[test]
    fn provider_with_max_size() {
        let provider = FilesystemProvider::new("/tmp").with_max_size(1024);
        assert_eq!(provider.max_file_size, 1024);
    }

    #[test]
    fn provider_with_description() {
        let provider = FilesystemProvider::new("/tmp").with_description("My files");
        assert_eq!(provider.description, Some("My files".to_string()));
    }

    #[test]
    fn provider_debug_redacts_local_paths_and_policy_text() {
        let root_canary = "/tmp/FAST_MCP_SECRET_ROOT_CANARY";
        let pattern_canary = "FAST_MCP_SECRET_PATTERN_CANARY*";
        let description_canary = "FAST_MCP_SECRET_DESCRIPTION_CANARY";
        let provider = FilesystemProvider::new(root_canary)
            .with_prefix("dbg")
            .with_patterns(&[pattern_canary])
            .with_description(description_canary);
        let debug = format!("{:?}", provider);
        assert!(debug.contains("FilesystemProvider"));
        assert!(debug.contains("dbg"));
        assert!(!debug.contains(root_canary));
        assert!(!debug.contains(pattern_canary));
        assert!(!debug.contains(description_canary));
    }

    #[test]
    fn provider_clone() {
        let provider = FilesystemProvider::new("/tmp")
            .with_prefix("cloned")
            .with_recursive(true)
            .with_max_size(5000);
        let cloned = provider.clone();
        assert_eq!(cloned.prefix, Some("cloned".to_string()));
        assert!(cloned.recursive);
        assert_eq!(cloned.max_file_size, 5000);
    }

    // ── URI methods ───────────────────────────────────────────────

    #[test]
    fn file_uri_with_prefix() {
        let provider = FilesystemProvider::new("/tmp").with_prefix("docs");
        assert_eq!(
            provider.file_uri("readme.md").unwrap(),
            "file:///docs/readme.md"
        );
    }

    #[test]
    fn file_uri_without_prefix() {
        let provider = FilesystemProvider::new("/tmp");
        assert_eq!(provider.file_uri("readme.md").unwrap(), "file:///readme.md");
    }

    #[test]
    fn uri_template_with_prefix() {
        let provider = FilesystemProvider::new("/tmp").with_prefix("data");
        assert_eq!(provider.uri_template(), "file:///data/{+path}");
    }

    #[test]
    fn uri_template_without_prefix() {
        let provider = FilesystemProvider::new("/tmp");
        assert_eq!(provider.uri_template(), "file:///{+path}");
    }

    #[test]
    fn path_from_uri_with_prefix() {
        let provider = FilesystemProvider::new("/tmp").with_prefix("docs");
        assert_eq!(
            provider
                .path_from_uri("file:///docs/readme.md")
                .expect("valid prefixed file URI"),
            "readme.md"
        );
    }

    #[test]
    fn path_from_uri_without_prefix() {
        let provider = FilesystemProvider::new("/tmp");
        assert_eq!(
            provider
                .path_from_uri("file:///readme.md")
                .expect("valid file URI"),
            "readme.md"
        );
    }

    #[test]
    fn path_from_uri_wrong_prefix() {
        let provider = FilesystemProvider::new("/tmp").with_prefix("docs");
        assert!(provider.path_from_uri("file:///other/readme.md").is_err());
    }

    #[test]
    fn path_from_uri_completely_wrong() {
        let provider = FilesystemProvider::new("/tmp").with_prefix("docs");
        assert!(provider.path_from_uri("http://example.com").is_err());
    }

    #[test]
    fn path_from_uri_rejects_query_fragment_and_control_delimiters() {
        let provider = FilesystemProvider::new("/tmp").with_prefix("docs");

        for uri in [
            "file:///docs/readme.md?version=2",
            "file:///docs/readme.md#section",
            "file:///docs/readme.md\nforged",
        ] {
            assert!(
                provider.path_from_uri(uri).is_err(),
                "ambiguous URI must be rejected: {uri:?}"
            );
        }
    }

    #[test]
    fn resource_paths_have_one_canonical_reserved_expansion_uri() {
        let provider = FilesystemProvider::new("/tmp").with_prefix("docs");
        let path = "nested/hello world/資料.md";
        let uri = provider.file_uri(path).expect("canonical resource URI");
        assert_eq!(
            uri,
            "file:///docs/nested/hello%20world/%E8%B3%87%E6%96%99.md"
        );
        assert_eq!(
            provider.path_from_uri(&uri).expect("canonical URI decodes"),
            path
        );

        for alias in [
            "file:///docs/nested%2Fhello.txt",
            "file:///docs/hello world.txt",
            "file:///docs/hello%2fworld.txt",
            "file:///docs/%2E%2E/secret.txt",
            "file:///docs/truncated%2",
            "file:///docs/invalid%GG",
        ] {
            assert!(
                provider.path_from_uri(alias).is_err(),
                "non-canonical or unsafe alias must be rejected: {alias:?}"
            );
        }
    }

    #[test]
    fn rejected_resource_uri_diagnostics_do_not_echo_peer_input() {
        let provider = FilesystemProvider::new("/tmp").with_prefix("docs");
        let canary = "PEER-PATH-CANARY\nforged";
        let error = provider
            .path_from_uri(&format!("file:///docs/{canary}"))
            .expect_err("control-bearing URI must be rejected");
        let message = error.to_string();

        assert!(!message.contains(canary));
        assert!(!message.chars().any(char::is_control));
        assert!(message.contains(REDACTED_RESOURCE_PATH));
    }

    #[test]
    fn generated_file_uris_have_an_empty_authority_and_valid_path_encoding() {
        let provider = FilesystemProvider::new("/tmp");
        for path in ["foo:bar", "[brackets]", "at@sign", "nested/a;b.txt"] {
            let uri = provider.file_uri(path).expect("canonical file URI");
            let parsed = url::Url::parse(&uri).expect("generated URI must parse");

            assert_eq!(parsed.scheme(), "file");
            assert!(parsed.host_str().is_none());
            assert_eq!(
                provider.path_from_uri(&uri).expect("generated URI decodes"),
                path
            );
        }
        assert_eq!(
            provider.file_uri("[brackets]").unwrap(),
            "file:///%5Bbrackets%5D"
        );
    }

    // ── matches_patterns ──────────────────────────────────────────

    #[test]
    fn matches_patterns_no_includes_no_excludes() {
        let provider = FilesystemProvider::new("/tmp").with_exclude(&[]);
        assert!(provider.matches_patterns("anything.txt"));
        assert!(provider.matches_patterns(".hidden"));
    }

    #[test]
    fn matches_patterns_excludes_only() {
        let provider = FilesystemProvider::new("/tmp"); // default excludes .*
        assert!(provider.matches_patterns("visible.txt"));
        assert!(!provider.matches_patterns(".hidden"));
    }

    #[test]
    fn matches_patterns_includes_only() {
        let provider = FilesystemProvider::new("/tmp")
            .with_exclude(&[])
            .with_patterns(&["*.md"]);
        assert!(provider.matches_patterns("readme.md"));
        assert!(!provider.matches_patterns("readme.txt"));
    }

    #[test]
    fn matches_patterns_exclude_takes_priority() {
        let provider = FilesystemProvider::new("/tmp")
            .with_patterns(&["*.md"])
            .with_exclude(&["secret.md"]);
        assert!(provider.matches_patterns("readme.md"));
        assert!(!provider.matches_patterns("secret.md"));
    }

    // ── handle-relative open edge cases ───────────────────────────

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn open_file_nofollow_not_found() {
        let root = TestDir::new("validate-notfound");
        let provider = FilesystemProvider::new(root.path());
        let result = provider.open_file_nofollow("nonexistent.txt");
        assert!(matches!(
            result,
            Err(FilesystemProviderError::NotFound { .. })
        ));
    }

    // ── read_file edge cases ──────────────────────────────────────

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn read_file_not_found() {
        let root = TestDir::new("read-notfound");
        let provider = FilesystemProvider::new(root.path());
        let result = provider.read_file(&test_context(), "missing.txt");
        assert!(matches!(
            result,
            Err(FilesystemProviderError::NotFound { .. })
        ));
    }

    // ── detect_mime_type extended ──────────────────────────────────

    #[test]
    fn detect_mime_type_text_formats() {
        assert_eq!(detect_mime_type(Path::new("f.txt")), "text/plain");
        assert_eq!(detect_mime_type(Path::new("f.html")), "text/html");
        assert_eq!(detect_mime_type(Path::new("f.htm")), "text/html");
        assert_eq!(detect_mime_type(Path::new("f.css")), "text/css");
        assert_eq!(detect_mime_type(Path::new("f.csv")), "text/csv");
        assert_eq!(detect_mime_type(Path::new("f.xml")), "application/xml");
        assert_eq!(detect_mime_type(Path::new("f.markdown")), "text/markdown");
    }

    #[test]
    fn detect_mime_type_programming_languages() {
        assert_eq!(detect_mime_type(Path::new("f.py")), "text/x-python");
        assert_eq!(detect_mime_type(Path::new("f.js")), "text/javascript");
        assert_eq!(detect_mime_type(Path::new("f.mjs")), "text/javascript");
        assert_eq!(detect_mime_type(Path::new("f.ts")), "text/typescript");
        assert_eq!(detect_mime_type(Path::new("f.mts")), "text/typescript");
        assert_eq!(detect_mime_type(Path::new("f.yaml")), "application/yaml");
        assert_eq!(detect_mime_type(Path::new("f.yml")), "application/yaml");
        assert_eq!(detect_mime_type(Path::new("f.toml")), "application/toml");
        assert_eq!(detect_mime_type(Path::new("f.sh")), "text/x-shellscript");
        assert_eq!(detect_mime_type(Path::new("f.bash")), "text/x-shellscript");
        assert_eq!(detect_mime_type(Path::new("f.c")), "text/x-c");
        assert_eq!(detect_mime_type(Path::new("f.cpp")), "text/x-c++");
        assert_eq!(detect_mime_type(Path::new("f.cc")), "text/x-c++");
        assert_eq!(detect_mime_type(Path::new("f.cxx")), "text/x-c++");
        assert_eq!(detect_mime_type(Path::new("f.h")), "text/x-c-header");
        assert_eq!(detect_mime_type(Path::new("f.hpp")), "text/x-c-header");
        assert_eq!(detect_mime_type(Path::new("f.java")), "text/x-java");
        assert_eq!(detect_mime_type(Path::new("f.go")), "text/x-go");
        assert_eq!(detect_mime_type(Path::new("f.rb")), "text/x-ruby");
        assert_eq!(detect_mime_type(Path::new("f.php")), "text/x-php");
        assert_eq!(detect_mime_type(Path::new("f.swift")), "text/x-swift");
        assert_eq!(detect_mime_type(Path::new("f.kt")), "text/x-kotlin");
        assert_eq!(detect_mime_type(Path::new("f.kts")), "text/x-kotlin");
        assert_eq!(detect_mime_type(Path::new("f.sql")), "text/x-sql");
    }

    #[test]
    fn detect_mime_type_images() {
        assert_eq!(detect_mime_type(Path::new("f.jpg")), "image/jpeg");
        assert_eq!(detect_mime_type(Path::new("f.jpeg")), "image/jpeg");
        assert_eq!(detect_mime_type(Path::new("f.gif")), "image/gif");
        assert_eq!(detect_mime_type(Path::new("f.svg")), "image/svg+xml");
        assert_eq!(detect_mime_type(Path::new("f.webp")), "image/webp");
        assert_eq!(detect_mime_type(Path::new("f.ico")), "image/x-icon");
        assert_eq!(detect_mime_type(Path::new("f.bmp")), "image/bmp");
    }

    #[test]
    fn detect_mime_type_binary() {
        assert_eq!(detect_mime_type(Path::new("f.pdf")), "application/pdf");
        assert_eq!(detect_mime_type(Path::new("f.zip")), "application/zip");
        assert_eq!(detect_mime_type(Path::new("f.gz")), "application/gzip");
        assert_eq!(detect_mime_type(Path::new("f.gzip")), "application/gzip");
        assert_eq!(detect_mime_type(Path::new("f.tar")), "application/x-tar");
        assert_eq!(detect_mime_type(Path::new("f.wasm")), "application/wasm");
        assert_eq!(
            detect_mime_type(Path::new("f.exe")),
            "application/octet-stream"
        );
        assert_eq!(
            detect_mime_type(Path::new("f.dll")),
            "application/octet-stream"
        );
        assert_eq!(
            detect_mime_type(Path::new("f.so")),
            "application/octet-stream"
        );
        assert_eq!(
            detect_mime_type(Path::new("f.bin")),
            "application/octet-stream"
        );
    }

    #[test]
    fn detect_mime_type_no_extension() {
        assert_eq!(
            detect_mime_type(Path::new("Makefile")),
            "application/octet-stream"
        );
    }

    // ── is_binary_mime_type extended ───────────────────────────────

    #[test]
    fn is_binary_mime_type_audio_video() {
        assert!(is_binary_mime_type("audio/mpeg"));
        assert!(is_binary_mime_type("video/mp4"));
    }

    #[test]
    fn is_binary_mime_type_archives() {
        assert!(is_binary_mime_type("application/zip"));
        assert!(is_binary_mime_type("application/gzip"));
        assert!(is_binary_mime_type("application/x-tar"));
        assert!(is_binary_mime_type("application/wasm"));
        assert!(is_binary_mime_type("application/octet-stream"));
    }

    #[test]
    fn is_binary_mime_type_text_types_false() {
        assert!(!is_binary_mime_type("text/html"));
        assert!(!is_binary_mime_type("text/markdown"));
        assert!(!is_binary_mime_type("application/yaml"));
        assert!(!is_binary_mime_type("application/toml"));
    }

    // ── base64_encode extended ────────────────────────────────────

    #[test]
    fn base64_encode_hello_world() {
        assert_eq!(
            base64_encode(b"Hello, World!").unwrap(),
            "SGVsbG8sIFdvcmxkIQ=="
        );
    }

    #[test]
    fn base64_encode_binary_sequence() {
        // Known value: bytes [0, 1, 2] → AAEC
        assert_eq!(base64_encode(&[0, 1, 2]).unwrap(), "AAEC");
    }

    // ── glob_match edge cases ─────────────────────────────────────

    #[test]
    fn glob_match_exact() {
        assert!(glob_match("readme.md", "readme.md"));
        assert!(!glob_match("readme.md", "other.md"));
    }

    #[test]
    fn glob_match_empty_pattern_empty_path() {
        assert!(glob_match("", ""));
    }

    #[test]
    fn glob_match_star_empty() {
        assert!(glob_match("*", ""));
        assert!(glob_match("*", "anything"));
    }

    #[test]
    fn glob_match_double_star_alone() {
        assert!(glob_match("**", ""));
        assert!(glob_match("**", "a/b/c"));
    }

    #[test]
    fn glob_match_mixed_pattern() {
        assert!(glob_match("src/*.rs", "src/main.rs"));
        assert!(!glob_match("src/*.rs", "src/sub/main.rs"));
        assert!(glob_match("src/**/*.rs", "src/sub/main.rs"));
    }

    // ── FilesystemResourceHandler ─────────────────────────────────

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn handler_debug() {
        let root = TestDir::new("handler-debug");
        write_text(&root.join("a.txt"), "hello");
        let handler = FilesystemProvider::new(root.path())
            .build_for_test()
            .expect("valid filesystem provider");
        let debug = format!("{:?}", handler);
        assert!(debug.contains("FilesystemResourceHandler"));
        assert!(debug.contains("provider"));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn handler_definition_without_prefix() {
        let root = TestDir::new("handler-no-prefix");
        let handler = FilesystemProvider::new(root.path())
            .build_for_test()
            .expect("valid filesystem provider");
        let def = handler.definition();
        assert_eq!(def.name, "files");
        assert_eq!(def.uri, "file:///{+path}");
        assert!(def.description.is_none());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn handler_template_without_prefix() {
        let root = TestDir::new("handler-tmpl-no-prefix");
        let handler = FilesystemProvider::new(root.path())
            .build_for_test()
            .expect("valid filesystem provider");
        let tmpl = handler.template().unwrap();
        assert_eq!(tmpl.uri_template, "file:///{+path}");
        assert_eq!(tmpl.name, "files");
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn handler_listing_is_a_live_view_not_a_stale_snapshot() {
        let root = TestDir::new("handler-live-view");
        write_text(&root.join("one.txt"), "1");
        write_text(&root.join("two.md"), "2");
        let handler = FilesystemProvider::new(root.path())
            .with_exclude(&[])
            .build_for_test()
            .expect("valid filesystem provider");
        write_text(&root.join("added-after-build.txt"), "3");

        let ctx = McpContext::new(asupersync::Cx::for_testing(), 1);
        let listing = handler.read(&ctx).expect("read live listing");
        let text = listing[0].text.as_deref().expect("text listing");
        assert!(text.contains("file:///one.txt"));
        assert!(text.contains("file:///two.md"));
        assert!(text.contains("file:///added-after-build.txt"));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn handler_read_with_uri_missing_path_param() {
        let root = TestDir::new("handler-missing-param");
        let handler = FilesystemProvider::new(root.path())
            .with_prefix("p")
            .build_for_test()
            .expect("valid filesystem provider");
        let ctx = McpContext::new(asupersync::Cx::for_testing(), 1);
        let empty_params = HashMap::new();
        // URI doesn't match prefix either
        let result = handler.read_with_uri(&ctx, "file://wrong/x", &empty_params);
        assert!(result.is_err());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn handler_read_binary_file_returns_blob() {
        let root = TestDir::new("handler-binary");
        write_bytes(&root.join("data.bin"), &[0xDE, 0xAD, 0xBE, 0xEF]);

        let handler = FilesystemProvider::new(root.path())
            .with_exclude(&[])
            .build_for_test()
            .expect("valid filesystem provider");
        let ctx = McpContext::new(asupersync::Cx::for_testing(), 1);
        let mut params = HashMap::new();
        params.insert("path".to_string(), "data.bin".to_string());
        let result = handler
            .read_with_uri(&ctx, "file:///data.bin", &params)
            .unwrap();
        assert!(result[0].text.is_none());
        assert!(result[0].blob.is_some());
    }

    // ── list_files with exclude ───────────────────────────────────

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn list_files_excludes_hidden_by_default() {
        let root = TestDir::new("list-hidden");
        write_text(&root.join("visible.txt"), "v");
        write_text(&root.join(".hidden"), "h");
        write_text(&root.join("nested/.hidden"), "nested hidden file");
        write_text(
            &root.join("nested/.private/visible-name.txt"),
            "hidden directory descendant",
        );

        let provider = FilesystemProvider::new(root.path()).with_recursive(true);
        let files = provider.list_files(&test_context()).unwrap();
        let paths: Vec<&str> = files.iter().map(|e| e.relative_path.as_str()).collect();
        assert!(paths.contains(&"visible.txt"));
        assert!(!paths.contains(&".hidden"));
        assert!(!paths.contains(&"nested/.hidden"));
        assert!(!paths.contains(&"nested/.private/visible-name.txt"));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn list_files_no_patterns_includes_all() {
        let root = TestDir::new("list-all");
        write_text(&root.join("a.txt"), "a");
        write_text(&root.join("b.rs"), "b");

        let provider = FilesystemProvider::new(root.path()).with_exclude(&[]);
        let files = provider.list_files(&test_context()).unwrap();
        assert!(files.len() >= 2);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn listing_entry_ceiling_accepts_n_and_rejects_n_plus_one() {
        let root = TestDir::new("list-entry-limit");
        write_text(&root.join("a.txt"), "a");
        write_text(&root.join("b.txt"), "b");

        let exact = FilesystemProvider::new(root.path())
            .with_exclude(&[])
            .with_max_entries(2);
        assert_eq!(exact.list_files(&test_context()).unwrap().len(), 2);

        let too_small = FilesystemProvider::new(root.path())
            .with_exclude(&[])
            .with_max_entries(1);
        assert!(matches!(
            too_small.list_files(&test_context()),
            Err(FilesystemProviderError::TooManyEntries { count: 2, max: 1 })
        ));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn listing_byte_ceiling_accepts_n_and_rejects_n_plus_one() {
        let root = TestDir::new("list-byte-limit");
        write_text(&root.join("a.txt"), "a");
        let expected_bytes = "[{\"uri\":\"file:///a.txt\",\"mimeType\":\"text/plain\"}]".len();

        let exact = FilesystemProvider::new(root.path())
            .with_exclude(&[])
            .with_max_listing_bytes(expected_bytes);
        assert_eq!(exact.list_files(&test_context()).unwrap().len(), 1);

        let too_small = FilesystemProvider::new(root.path())
            .with_exclude(&[])
            .with_max_listing_bytes(expected_bytes - 1);
        assert!(matches!(
            too_small.list_files(&test_context()),
            Err(FilesystemProviderError::ListingTooLarge { size, max })
                if size == expected_bytes && max == expected_bytes - 1
        ));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn listing_depth_ceiling_accepts_n_and_rejects_n_plus_one() {
        let root = TestDir::new("list-depth-limit");
        write_text(&root.join("nested/file.txt"), "nested");

        let exact = FilesystemProvider::new(root.path())
            .with_exclude(&[])
            .with_recursive(true)
            .with_max_depth(1);
        assert_eq!(exact.list_files(&test_context()).unwrap().len(), 1);

        let too_shallow = FilesystemProvider::new(root.path())
            .with_exclude(&[])
            .with_recursive(true)
            .with_max_depth(0);
        assert!(matches!(
            too_shallow.list_files(&test_context()),
            Err(FilesystemProviderError::TooDeep {
                depth: 1,
                max: 0,
                ..
            })
        ));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn listing_and_direct_reads_reject_cancelled_contexts() {
        let root = TestDir::new("cancelled-listing");
        write_text(&root.join("a.txt"), "a");
        let provider = FilesystemProvider::new(root.path()).with_exclude(&[]);
        let cx = asupersync::Cx::for_testing();
        cx.set_cancel_requested(true);
        let ctx = McpContext::new(cx, 1);

        assert!(matches!(
            provider.list_files(&ctx),
            Err(FilesystemProviderError::Cancelled)
        ));
        assert!(matches!(
            provider.read_file(&ctx, "a.txt"),
            Err(FilesystemProviderError::Cancelled)
        ));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn nonrecursive_provider_rejects_direct_nested_uri_bypass() {
        let root = TestDir::new("nonrecursive-direct-read");
        write_text(&root.join("nested/file.txt"), "nested");

        let nonrecursive = FilesystemProvider::new(root.path()).with_exclude(&[]);
        assert!(matches!(
            nonrecursive.read_file(&test_context(), "nested/file.txt"),
            Err(FilesystemProviderError::NotFound { .. })
        ));

        let recursive = FilesystemProvider::new(root.path())
            .with_exclude(&[])
            .with_recursive(true)
            .with_max_depth(1);
        assert!(matches!(
            recursive.read_file(&test_context(), "nested/file.txt"),
            Ok(FileContent::Text(text)) if text == "nested"
        ));

        write_text(&root.join("nested/deeper/file.txt"), "too deep");
        assert!(matches!(
            recursive.read_file(&test_context(), "nested/deeper/file.txt"),
            Err(FilesystemProviderError::TooDeep {
                depth: 2,
                max: 1,
                ..
            })
        ));
    }

    // ── DEFAULT_MAX_SIZE ──────────────────────────────────────────

    #[test]
    fn default_max_size_is_10mb() {
        assert_eq!(DEFAULT_MAX_SIZE, 10 * 1024 * 1024);
    }

    // ── FileEntry ─────────────────────────────────────────────────

    #[test]
    fn file_entry_debug() {
        let entry = FileEntry {
            relative_path: "test.txt".to_string(),
            uri: "file:///test.txt".to_string(),
            size: Some(42),
            mime_type: "text/plain".to_string(),
        };
        let debug = format!("{:?}", entry);
        assert!(debug.contains("test.txt"));
        assert!(debug.contains("42"));
    }

    // ── Provider builder chaining ─────────────────────────────────

    #[test]
    fn provider_builder_chaining() {
        let root = TestDir::new("builder-chain");
        let provider = FilesystemProvider::new(root.path())
            .with_prefix("chain")
            .with_patterns(&["*.md"])
            .with_exclude(&["*.bak"])
            .with_recursive(true)
            .with_max_size(2048)
            .with_max_entries(20)
            .with_max_depth(3)
            .with_max_listing_bytes(4096)
            .with_description("Chain test");

        assert_eq!(provider.prefix, Some("chain".to_string()));
        assert_eq!(provider.include_patterns, vec!["*.md"]);
        assert_eq!(provider.exclude_patterns, vec!["*.bak"]);
        assert!(provider.recursive);
        assert_eq!(provider.max_file_size, 2048);
        assert_eq!(provider.max_entries, 20);
        assert_eq!(provider.max_depth, 3);
        assert_eq!(provider.max_listing_bytes, 4096);
        assert_eq!(provider.description, Some("Chain test".to_string()));
    }

    // ── Additional coverage ─────────────────────────────────────────

    #[test]
    fn detect_mime_type_case_insensitive() {
        assert_eq!(detect_mime_type(Path::new("README.MD")), "text/markdown");
        assert_eq!(detect_mime_type(Path::new("photo.JPG")), "image/jpeg");
        assert_eq!(detect_mime_type(Path::new("data.JSON")), "application/json");
    }

    #[test]
    fn glob_match_question_mark_at_end_fails_when_no_char() {
        assert!(!glob_match("file?", "file"));
        assert!(glob_match("file?", "fileA"));
    }

    #[test]
    fn base64_encode_round_trips_with_std_decoder() {
        use base64::Engine as _;
        let data = b"The quick brown fox jumps over the lazy dog";
        let encoded = base64_encode(data).expect("bounded base64");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&encoded)
            .expect("valid base64");
        assert_eq!(decoded, data);
    }

    #[test]
    fn base64_encode_rejects_payload_above_raw_input_ceiling() {
        let oversized = vec![0_u8; MAX_CONFIGURED_FILE_SIZE + 1];

        assert!(matches!(
            base64_encode(&oversized),
            Err(FilesystemProviderError::TooLarge { .. })
        ));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn handler_empty_root_has_an_empty_live_listing() {
        let root = TestDir::new("handler-empty");
        let handler = FilesystemProvider::new(root.path())
            .build_for_test()
            .expect("valid filesystem provider");
        let ctx = McpContext::new(asupersync::Cx::for_testing(), 1);
        let listing = handler.read(&ctx).expect("read empty listing");
        assert_eq!(listing[0].text.as_deref(), Some("[]"));
    }

    #[test]
    fn list_files_nonexistent_root_returns_error() {
        let provider = FilesystemProvider::new("/nonexistent-fastmcp-test-dir-xyz");
        let result = provider.list_files(&test_context());
        assert!(result.is_err());
    }

    #[test]
    fn read_file_path_traversal_blocked() {
        let root = TestDir::new("read-traversal");
        write_text(&root.join("safe.txt"), "ok");
        let provider = FilesystemProvider::new(root.path());
        let result = provider.read_file(&test_context(), "../../../etc/passwd");
        assert!(matches!(
            result,
            Err(FilesystemProviderError::PathTraversal { .. }
                | FilesystemProviderError::NotFound { .. })
        ));
    }
}
