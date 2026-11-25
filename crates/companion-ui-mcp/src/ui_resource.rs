use std::path::PathBuf;
use std::sync::OnceLock;

use companion_core::FsError;
use companion_core::fs_safe::{SymlinkPolicy, read_regular_file_utf8_bounded};
use rmcp::model::{ErrorData, Icon, MetaObject};
use serde_json::{Map, Value, json};

use crate::types::UiHtmlSource;

pub const UI_RESOURCE_URI: &str = "ui://companion/settings.html";
pub const UI_MIME_TYPE: &str = "text/html;profile=mcp-app";
pub const UI_HTML_ENV_VAR: &str = "AGY_COMPANION_UI_HTML";
pub const MAX_UI_HTML_BYTES: u64 = 10 * 1024 * 1024;
pub const EMBEDDED_UI_HTML: &str = include_str!("../../../ui/dist/index.html");
pub const EMBEDDED_LOGO_SVG: &[u8] = include_bytes!("../../../plugin/assets/logo.svg");

static LOGO_DATA_URI: OnceLock<String> = OnceLock::new();

fn meta_from_value(value: Value) -> MetaObject {
    match value {
        Value::Object(map) => MetaObject::from(map),
        _ => MetaObject::from(Map::new()),
    }
}

pub(crate) fn ui_resource_meta() -> MetaObject {
    meta_from_value(json!({
        "ui": {
            "prefersBorder": false
        }
    }))
}

pub(crate) fn encode_base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    let mut chunks = input.chunks_exact(3);
    for chunk in &mut chunks {
        let n = ((chunk[0] as u32) << 16) | ((chunk[1] as u32) << 8) | (chunk[2] as u32);
        out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 6) & 0x3f) as usize] as char);
        out.push(TABLE[(n & 0x3f) as usize] as char);
    }
    match chunks.remainder() {
        [a] => {
            let n = (*a as u32) << 16;
            out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
            out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
            out.push('=');
            out.push('=');
        }
        [a, b] => {
            let n = ((*a as u32) << 16) | ((*b as u32) << 8);
            out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
            out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
            out.push(TABLE[((n >> 6) & 0x3f) as usize] as char);
            out.push('=');
        }
        [] => {}
        _ => unreachable!(),
    }
    out
}

pub(crate) fn logo_data_uri() -> &'static str {
    LOGO_DATA_URI.get_or_init(|| {
        format!(
            "data:image/svg+xml;base64,{}",
            encode_base64(EMBEDDED_LOGO_SVG)
        )
    })
}

pub(crate) fn companion_icons() -> Vec<Icon> {
    vec![
        Icon::new(logo_data_uri())
            .with_mime_type("image/svg+xml")
            .with_sizes(vec!["any".to_string()]),
    ]
}

pub(crate) fn open_settings_meta() -> MetaObject {
    meta_from_value(json!({
        "ui": {
            "resourceUri": UI_RESOURCE_URI
        },
        "com.google.antigravity/ui": {
            "entrypoints": [
                { "type": "aux_pane", "title": "Antigravity Companion" },
                { "type": "global", "title": "Antigravity Companion" }
            ]
        }
    }))
}

pub(crate) fn app_only_meta() -> MetaObject {
    meta_from_value(json!({
        "ui": {
            "visibility": ["app"]
        }
    }))
}

pub(crate) fn resolve_ui_html_override_path(source: &UiHtmlSource) -> Option<PathBuf> {
    match source {
        UiHtmlSource::ProcessEnv => {
            let raw = std::env::var_os(UI_HTML_ENV_VAR)?;
            if raw.is_empty() {
                return None;
            }
            if let Some(text) = raw.to_str() {
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    return None;
                }
                return Some(PathBuf::from(trimmed));
            }
            Some(PathBuf::from(raw))
        }
        UiHtmlSource::Fixed(path) => path.clone(),
    }
}

pub(crate) async fn load_ui_html_override(path: PathBuf) -> Result<String, ErrorData> {
    tokio::task::spawn_blocking(move || {
        let loaded =
            read_regular_file_utf8_bounded(&path, MAX_UI_HTML_BYTES, SymlinkPolicy::Follow)
                .map_err(|err| match err {
                    FsError::EmptyPath | FsError::ContainsNul | FsError::ParentDirTraversal => {
                        ErrorData::internal_error(
                            format!("invalid {UI_HTML_ENV_VAR} path: {err}"),
                            None,
                        )
                    }
                    FsError::NotRegularFile { .. } => ErrorData::internal_error(
                        format!("{UI_HTML_ENV_VAR} path must point to a regular file"),
                        None,
                    ),
                    FsError::SizeLimitExceeded { .. } => ErrorData::internal_error(
                        format!(
                            "{UI_HTML_ENV_VAR} exceeds maximum size of {MAX_UI_HTML_BYTES} bytes"
                        ),
                        None,
                    ),
                    FsError::InvalidUtf8 { .. } => ErrorData::internal_error(
                        format!("{UI_HTML_ENV_VAR} must be valid UTF-8"),
                        None,
                    ),
                    FsError::ConcurrentModification { .. } => ErrorData::internal_error(
                        format!("{UI_HTML_ENV_VAR} file changed concurrently during read"),
                        None,
                    ),
                    other => ErrorData::internal_error(
                        format!("failed to read {UI_HTML_ENV_VAR} file: {other}"),
                        None,
                    ),
                })?;

        let Some((html, _meta)) = loaded else {
            return Err(ErrorData::internal_error(
                format!("failed to open {UI_HTML_ENV_VAR} file: file not found"),
                None,
            ));
        };

        if html.trim().is_empty() {
            return Err(ErrorData::internal_error(
                format!("{UI_HTML_ENV_VAR} must not be empty"),
                None,
            ));
        }

        Ok(html)
    })
    .await
    .map_err(|err| ErrorData::internal_error(format!("UI HTML load task failed: {err}"), None))?
}
