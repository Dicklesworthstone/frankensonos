//! Multipart form data parser.
//!
//! `fastapi-http` re-exports the canonical multipart implementation from `fastapi-core`
//! so both the HTTP server and core extractors share identical behavior.

pub use fastapi_core::multipart::{
    DEFAULT_MAX_FIELDS, DEFAULT_MAX_FILE_SIZE, DEFAULT_MAX_TOTAL_SIZE, MultipartConfig,
    MultipartError, MultipartForm, MultipartParser, Part, UploadFile, parse_boundary,
};
