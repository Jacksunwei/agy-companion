use std::path::PathBuf;

use companion_core::FsError;
use companion_core::fs_safe::{SymlinkPolicy, read_regular_file_utf8_bounded};

use crate::types::UiHtmlSource;

pub const UI_HTML_ENV_VAR: &str = "AGY_COMPANION_UI_HTML";
pub const MAX_UI_HTML_BYTES: u64 = 10 * 1024 * 1024;
pub const EMBEDDED_UI_HTML: &str = include_str!("../../../ui/dist/index.html");

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

pub(crate) async fn load_ui_html_override(path: PathBuf) -> Result<String, String> {
    tokio::task::spawn_blocking(move || {
        let loaded =
            read_regular_file_utf8_bounded(&path, MAX_UI_HTML_BYTES, SymlinkPolicy::Follow)
                .map_err(|err| match err {
                    FsError::EmptyPath | FsError::ContainsNul | FsError::ParentDirTraversal => {
                        format!("invalid {UI_HTML_ENV_VAR} path: {err}")
                    }
                    FsError::NotRegularFile { .. } => {
                        format!("{UI_HTML_ENV_VAR} path must point to a regular file")
                    }
                    FsError::SizeLimitExceeded { .. } => {
                        format!(
                            "{UI_HTML_ENV_VAR} exceeds maximum size of {MAX_UI_HTML_BYTES} bytes"
                        )
                    }
                    FsError::InvalidUtf8 { .. } => {
                        format!("{UI_HTML_ENV_VAR} must be valid UTF-8")
                    }
                    FsError::ConcurrentModification { .. } => {
                        format!("{UI_HTML_ENV_VAR} file changed concurrently during read")
                    }
                    other => format!("failed to read {UI_HTML_ENV_VAR} file: {other}"),
                })?;

        let Some((html, _meta)) = loaded else {
            return Err(format!(
                "failed to open {UI_HTML_ENV_VAR} file: file not found"
            ));
        };

        if html.trim().is_empty() {
            return Err(format!("{UI_HTML_ENV_VAR} must not be empty"));
        }

        Ok(html)
    })
    .await
    .map_err(|err| format!("UI HTML load task failed: {err}"))?
}
