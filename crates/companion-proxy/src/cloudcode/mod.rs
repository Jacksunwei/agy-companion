use bytes::Bytes;
use companion_core::CoreError;
use companion_providers::ByteStream;
use http::StatusCode;
use http::header::HeaderMap;
use thiserror::Error;

mod catalog;
mod client;
mod metadata;
mod routing;
mod slots;

#[cfg(test)]
mod tests;

pub use catalog::{merge_available_models, pretty_model_name};
pub use client::CloudCodeClient;
pub(crate) use routing::is_uncompressed_content_encoding;
pub use routing::{is_cloudcode_path, validate_cloudcode_path};

pub const MAX_CLOUDCODE_CATALOG_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum CloudCodeError {
    #[error("invalid cloudcode upstream URL: {0}")]
    InvalidConfig(#[from] CoreError),
    #[error("invalid cloudcode request path: {0}")]
    InvalidPath(String),
    #[error("cloudcode upstream request failed: {0}")]
    Upstream(String),
    #[error("cloudcode upstream response exceeded {MAX_CLOUDCODE_CATALOG_BYTES} bytes")]
    ResponseTooLarge,
}

pub struct CloudCodeStreamResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub response_is_uncompressed: bool,
    pub stream: ByteStream,
}

pub struct CloudCodeBufferedResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub response_is_uncompressed: bool,
    pub body: Bytes,
}
