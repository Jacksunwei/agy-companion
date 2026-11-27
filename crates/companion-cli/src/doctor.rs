use std::fmt::Write as _;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use companion_core::fs_safe::{self, FsError};
use companion_core::{
    AuthStore, CODEX_AUTH_FILENAME, CODEX_REFRESH_LOCK_FILENAME, Config, DEFAULT_LISTEN_PORT,
    FileAuthStore, GEMINI_AUTH_FILENAME, MAX_AUTH_FILE_BYTES, PROTOCOL_VERSION, ProxyHealthSummary,
    auth_dir_for_config_path, is_placeholder_api_key, sanitize_base_url,
};
use companion_providers::LoopbackProxyClient;
use serde::{Deserialize, Serialize};

use crate::{CliError, MAX_CONFIG_FILE_BYTES, resolve_cli_config_path};

pub use companion_providers::{
    HealthEvaluation, MAX_PROBE_BODY_BYTES as MAX_HEALTH_BODY_BYTES,
    evaluate_health as evaluate_health_response,
};

pub const GEMINI_API_KEY_ENV: &str = "GEMINI_API_KEY";
pub const GOOGLE_GEMINI_BASE_URL_ENV: &str = "GOOGLE_GEMINI_BASE_URL";
pub const DEFAULT_HEALTH_TIMEOUT_SECS: u64 = 3;
pub const DEFAULT_HEALTH_CONNECT_TIMEOUT_SECS: u64 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvKeyState {
    Unset,
    Placeholder,
    Set,
    SetInvalidUtf8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvUrlState {
    Unset,
    Set(String),
    SetInvalidUtf8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorEnv {
    pub gemini_api_key: EnvKeyState,
    pub google_gemini_base_url: EnvUrlState,
}

impl DoctorEnv {
    pub fn from_process() -> Self {
        let gemini_api_key = match std::env::var_os(GEMINI_API_KEY_ENV) {
            None => EnvKeyState::Unset,
            Some(os_val) => match os_val.to_str() {
                Some(s) if s.trim().is_empty() => EnvKeyState::Unset,
                Some(s) if is_placeholder_api_key(s) => EnvKeyState::Placeholder,
                Some(_) => EnvKeyState::Set,
                None => EnvKeyState::SetInvalidUtf8,
            },
        };

        let google_gemini_base_url = match std::env::var_os(GOOGLE_GEMINI_BASE_URL_ENV) {
            None => EnvUrlState::Unset,
            Some(os_val) => match os_val.to_str() {
                Some(s) if s.trim().is_empty() => EnvUrlState::Unset,
                Some(s) => EnvUrlState::Set(s.trim().to_string()),
                None => EnvUrlState::SetInvalidUtf8,
            },
        };

        Self {
            gemini_api_key,
            google_gemini_base_url,
        }
    }

    pub fn from_raw_values(api_key: Option<&str>, base_url: Option<&str>) -> Self {
        let gemini_api_key = match api_key {
            Some(val) if val.trim().is_empty() => EnvKeyState::Unset,
            Some(val) if is_placeholder_api_key(val) => EnvKeyState::Placeholder,
            Some(_) => EnvKeyState::Set,
            None => EnvKeyState::Unset,
        };
        let google_gemini_base_url = match base_url {
            Some(val) if !val.trim().is_empty() => EnvUrlState::Set(val.trim().to_string()),
            _ => EnvUrlState::Unset,
        };

        Self {
            gemini_api_key,
            google_gemini_base_url,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    Pass,
    Warn,
    Info,
    Fail,
    Skip,
}

impl CheckStatus {
    fn tag(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Warn => "WARN",
            Self::Info => "INFO",
            Self::Fail => "FAIL",
            Self::Skip => "SKIP",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorCheck {
    pub name: String,
    pub status: CheckStatus,
    pub critical: bool,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvSummary {
    pub gemini_api_key_set: bool,
    pub google_gemini_base_url_set: bool,
    pub google_gemini_base_url_matches_proxy: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorReport {
    pub ok: bool,
    pub binary_version: String,
    pub expected_protocol_version: u32,
    pub config_path: String,
    pub target_port: u16,
    pub proxy: ProxyHealthSummary,
    pub version_skew: bool,
    pub protocol_skew: bool,
    pub env: EnvSummary,
    pub checks: Vec<DoctorCheck>,
}

#[derive(Debug, Clone)]
pub struct DoctorOptions {
    pub config_override: Option<PathBuf>,
    pub port_override: Option<u16>,
    pub binary_version: String,
    pub expected_protocol_version: u32,
    pub health_timeout: Duration,
    pub health_connect_timeout: Duration,
}

impl Default for DoctorOptions {
    fn default() -> Self {
        Self {
            config_override: None,
            port_override: None,
            binary_version: env!("CARGO_PKG_VERSION").to_string(),
            expected_protocol_version: PROTOCOL_VERSION,
            health_timeout: Duration::from_secs(DEFAULT_HEALTH_TIMEOUT_SECS),
            health_connect_timeout: Duration::from_secs(DEFAULT_HEALTH_CONNECT_TIMEOUT_SECS),
        }
    }
}

enum AuthFileInspection {
    Missing,
    Present {
        #[cfg(unix)]
        mode: u32,
        perm_err: Option<String>,
    },
    Rejected {
        perm_err: String,
        file_err: String,
    },
}

fn inspect_auth_file(auth_dir: &Path, filename: &str, rel_label: &str) -> AuthFileInspection {
    let path = auth_dir.join(filename);
    match fs_safe::inspect_regular_file_if_exists(&path, Some(MAX_AUTH_FILE_BYTES)) {
        Ok(None) => AuthFileInspection::Missing,
        Ok(Some(file_meta)) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;

                let mode = file_meta.permissions().mode() & 0o7777;
                let perm_err = if (mode & 0o077) == 0 && (mode & 0o7000) == 0 {
                    None
                } else {
                    Some(format!(
                        "insecure {rel_label} permissions {mode:04o}; run `chmod 600` on {rel_label}"
                    ))
                };
                AuthFileInspection::Present { mode, perm_err }
            }
            #[cfg(not(unix))]
            {
                let _ = file_meta;
                AuthFileInspection::Present { perm_err: None }
            }
        }
        Err(FsError::SymlinkRejected { .. }) => AuthFileInspection::Rejected {
            perm_err: format!(
                "{rel_label} is a symlink (rejected for security); use a regular file with chmod 600"
            ),
            file_err: format!("refused to load symlinked {rel_label}"),
        },
        Err(FsError::NotRegularFile { .. }) => AuthFileInspection::Rejected {
            perm_err: format!("{rel_label} is not a regular file"),
            file_err: format!("refused to load non-regular {rel_label}"),
        },
        Err(FsError::SizeLimitExceeded { max_bytes, .. }) => AuthFileInspection::Rejected {
            perm_err: format!("{rel_label} exceeds maximum allowed size ({max_bytes} bytes)"),
            file_err: format!("refused to load oversized {rel_label}"),
        },
        Err(FsError::Io { source, .. }) => AuthFileInspection::Rejected {
            perm_err: format!("unable to inspect {rel_label} metadata: {}", source.kind()),
            file_err: format!("{rel_label} metadata unreadable"),
        },
        Err(err) => {
            let msg = err.to_string();
            AuthFileInspection::Rejected {
                perm_err: msg.clone(),
                file_err: msg,
            }
        }
    }
}

fn current_utc_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

pub async fn run_doctor(options: DoctorOptions, env: DoctorEnv) -> Result<DoctorReport, CliError> {
    if options.port_override == Some(0) {
        return Err(CliError::InvalidPort(0));
    }

    let mut checks = Vec::with_capacity(8);
    let mut loaded_config: Option<Config> = None;
    let mut resolved_config_path: Option<PathBuf> = None;

    let config_path_display = match resolve_cli_config_path(options.config_override.as_deref()) {
        Ok(config_path) => {
            resolved_config_path = Some(config_path.clone());
            let display = config_path.display().to_string();
            match fs_safe::inspect_regular_file_if_exists(&config_path, Some(MAX_CONFIG_FILE_BYTES))
            {
                Ok(None) => {
                    checks.push(DoctorCheck {
                        name: "config_permissions".to_string(),
                        status: CheckStatus::Pass,
                        critical: true,
                        message: "config file not present; using defaults".to_string(),
                    });
                    checks.push(DoctorCheck {
                        name: "config".to_string(),
                        status: CheckStatus::Pass,
                        critical: true,
                        message: "config file not present; using defaults".to_string(),
                    });
                }
                Err(FsError::SymlinkRejected { .. }) => {
                    checks.push(DoctorCheck {
                        name: "config_permissions".to_string(),
                        status: CheckStatus::Fail,
                        critical: true,
                        message: "config path is a symlink (rejected for security); use a regular file with chmod 600".to_string(),
                    });
                    checks.push(DoctorCheck {
                        name: "config".to_string(),
                        status: CheckStatus::Fail,
                        critical: true,
                        message: "refused to load symlinked config file".to_string(),
                    });
                }
                Err(FsError::NotRegularFile { .. }) => {
                    checks.push(DoctorCheck {
                        name: "config_permissions".to_string(),
                        status: CheckStatus::Fail,
                        critical: true,
                        message: "config path is not a regular file".to_string(),
                    });
                    checks.push(DoctorCheck {
                        name: "config".to_string(),
                        status: CheckStatus::Fail,
                        critical: true,
                        message: "refused to load non-regular config file".to_string(),
                    });
                }
                Err(FsError::SizeLimitExceeded { max_bytes, .. }) => {
                    checks.push(DoctorCheck {
                        name: "config_permissions".to_string(),
                        status: CheckStatus::Fail,
                        critical: true,
                        message: format!(
                            "config file exceeds maximum allowed size ({max_bytes} bytes)"
                        ),
                    });
                    checks.push(DoctorCheck {
                        name: "config".to_string(),
                        status: CheckStatus::Fail,
                        critical: true,
                        message: "refused to load oversized config file".to_string(),
                    });
                }
                Err(FsError::Io { source, .. }) => {
                    checks.push(DoctorCheck {
                        name: "config_permissions".to_string(),
                        status: CheckStatus::Fail,
                        critical: true,
                        message: format!(
                            "unable to inspect config file metadata: {}",
                            source.kind()
                        ),
                    });
                    checks.push(DoctorCheck {
                        name: "config".to_string(),
                        status: CheckStatus::Fail,
                        critical: true,
                        message: "config file metadata unreadable".to_string(),
                    });
                }
                Err(err) => {
                    let msg = err.to_string();
                    checks.push(DoctorCheck {
                        name: "config_permissions".to_string(),
                        status: CheckStatus::Fail,
                        critical: true,
                        message: msg.clone(),
                    });
                    checks.push(DoctorCheck {
                        name: "config".to_string(),
                        status: CheckStatus::Fail,
                        critical: true,
                        message: msg,
                    });
                }
                Ok(Some(meta)) => {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;

                        let mode = meta.permissions().mode() & 0o7777;
                        if (mode & 0o077) == 0 && (mode & 0o7000) == 0 {
                            checks.push(DoctorCheck {
                                name: "config_permissions".to_string(),
                                status: CheckStatus::Pass,
                                critical: true,
                                message: format!("permissions {mode:04o} are owner-only"),
                            });
                        } else {
                            checks.push(DoctorCheck {
                                name: "config_permissions".to_string(),
                                status: CheckStatus::Fail,
                                critical: true,
                                message: format!(
                                    "insecure config permissions {mode:04o}; run `chmod 600` on the config file"
                                ),
                            });
                        }
                    }

                    #[cfg(not(unix))]
                    {
                        let _ = meta;
                        checks.push(DoctorCheck {
                            name: "config_permissions".to_string(),
                            status: CheckStatus::Pass,
                            critical: true,
                            message: "Unix permission check not applicable on this platform"
                                .to_string(),
                        });
                    }

                    match Config::load_or_default(&config_path) {
                        Ok(cfg) => {
                            checks.push(DoctorCheck {
                                name: "config".to_string(),
                                status: CheckStatus::Pass,
                                critical: true,
                                message: format!(
                                    "valid config (schema_version {})",
                                    cfg.schema_version
                                ),
                            });
                            loaded_config = Some(cfg);
                        }
                        Err(err) => {
                            checks.push(DoctorCheck {
                                name: "config".to_string(),
                                status: CheckStatus::Fail,
                                critical: true,
                                message: err.to_string(),
                            });
                        }
                    }
                }
            }
            display
        }
        Err(err) => {
            checks.push(DoctorCheck {
                name: "config_permissions".to_string(),
                status: CheckStatus::Skip,
                critical: true,
                message: "skipped because config path is invalid".to_string(),
            });
            checks.push(DoctorCheck {
                name: "config".to_string(),
                status: CheckStatus::Fail,
                critical: true,
                message: err.to_string(),
            });
            "<invalid>".to_string()
        }
    };

    let mut has_stored_gemini_key = false;
    let mut has_valid_codex_auth = false;
    match resolved_config_path.as_deref() {
        None => {
            checks.push(DoctorCheck {
                name: "auth_permissions".to_string(),
                status: CheckStatus::Skip,
                critical: true,
                message: "skipped because config path is invalid".to_string(),
            });
            checks.push(DoctorCheck {
                name: "auth_gemini".to_string(),
                status: CheckStatus::Skip,
                critical: true,
                message: "skipped because config path is invalid".to_string(),
            });
            checks.push(DoctorCheck {
                name: "auth_codex".to_string(),
                status: CheckStatus::Skip,
                critical: true,
                message: "skipped because config path is invalid".to_string(),
            });
        }
        Some(config_path) => match auth_dir_for_config_path(config_path) {
            Err(err) => {
                let msg = err.to_string();
                checks.push(DoctorCheck {
                    name: "auth_permissions".to_string(),
                    status: CheckStatus::Fail,
                    critical: true,
                    message: msg.clone(),
                });
                checks.push(DoctorCheck {
                    name: "auth_gemini".to_string(),
                    status: CheckStatus::Fail,
                    critical: true,
                    message: msg.clone(),
                });
                checks.push(DoctorCheck {
                    name: "auth_codex".to_string(),
                    status: CheckStatus::Fail,
                    critical: true,
                    message: msg,
                });
            }
            Ok(auth_dir) => match fs_safe::inspect_existing_dir(&auth_dir) {
                Ok(None) => {
                    checks.push(DoctorCheck {
                        name: "auth_permissions".to_string(),
                        status: CheckStatus::Pass,
                        critical: true,
                        message: "auth directory not present; no stored provider credentials"
                            .to_string(),
                    });
                    checks.push(DoctorCheck {
                        name: "auth_gemini".to_string(),
                        status: CheckStatus::Pass,
                        critical: true,
                        message: "Gemini credentials file not present".to_string(),
                    });
                    checks.push(DoctorCheck {
                        name: "auth_codex".to_string(),
                        status: CheckStatus::Pass,
                        critical: true,
                        message: "OpenAI Codex credentials file not present".to_string(),
                    });
                }
                Err(FsError::SymlinkRejected { .. }) => {
                    checks.push(DoctorCheck {
                        name: "auth_permissions".to_string(),
                        status: CheckStatus::Fail,
                        critical: true,
                        message: "auth directory is a symlink (rejected for security); use a real directory with chmod 700".to_string(),
                    });
                    checks.push(DoctorCheck {
                        name: "auth_gemini".to_string(),
                        status: CheckStatus::Fail,
                        critical: true,
                        message: "refused to load Gemini credentials from symlinked auth directory"
                            .to_string(),
                    });
                    checks.push(DoctorCheck {
                        name: "auth_codex".to_string(),
                        status: CheckStatus::Fail,
                        critical: true,
                        message:
                            "refused to load OpenAI Codex credentials from symlinked auth directory"
                                .to_string(),
                    });
                }
                Err(FsError::NotDirectory { .. }) => {
                    checks.push(DoctorCheck {
                        name: "auth_permissions".to_string(),
                        status: CheckStatus::Fail,
                        critical: true,
                        message: "auth path is not a directory".to_string(),
                    });
                    checks.push(DoctorCheck {
                        name: "auth_gemini".to_string(),
                        status: CheckStatus::Fail,
                        critical: true,
                        message: "refused to load Gemini credentials because auth path is not a directory".to_string(),
                    });
                    checks.push(DoctorCheck {
                        name: "auth_codex".to_string(),
                        status: CheckStatus::Fail,
                        critical: true,
                        message: "refused to load OpenAI Codex credentials because auth path is not a directory".to_string(),
                    });
                }
                Err(FsError::Io { source, .. }) => {
                    checks.push(DoctorCheck {
                        name: "auth_permissions".to_string(),
                        status: CheckStatus::Fail,
                        critical: true,
                        message: format!(
                            "unable to inspect auth directory metadata: {}",
                            source.kind()
                        ),
                    });
                    checks.push(DoctorCheck {
                        name: "auth_gemini".to_string(),
                        status: CheckStatus::Fail,
                        critical: true,
                        message: "auth directory metadata unreadable".to_string(),
                    });
                    checks.push(DoctorCheck {
                        name: "auth_codex".to_string(),
                        status: CheckStatus::Fail,
                        critical: true,
                        message: "auth directory metadata unreadable".to_string(),
                    });
                }
                Err(err) => {
                    let msg = err.to_string();
                    checks.push(DoctorCheck {
                        name: "auth_permissions".to_string(),
                        status: CheckStatus::Fail,
                        critical: true,
                        message: msg.clone(),
                    });
                    checks.push(DoctorCheck {
                        name: "auth_gemini".to_string(),
                        status: CheckStatus::Fail,
                        critical: true,
                        message: msg.clone(),
                    });
                    checks.push(DoctorCheck {
                        name: "auth_codex".to_string(),
                        status: CheckStatus::Fail,
                        critical: true,
                        message: msg,
                    });
                }
                Ok(Some(dir_meta)) => {
                    let mut perm_errors: Vec<String> = Vec::new();
                    #[cfg(unix)]
                    let dir_mode = {
                        use std::os::unix::fs::PermissionsExt;

                        let mode = dir_meta.permissions().mode() & 0o7777;
                        if (mode & 0o077) != 0 || (mode & 0o7000) != 0 {
                            perm_errors.push(format!(
                                "insecure auth directory permissions {mode:04o}; run `chmod 700` on the auth directory"
                            ));
                        }
                        mode
                    };
                    #[cfg(not(unix))]
                    let _ = dir_meta;

                    let gemini_insp =
                        inspect_auth_file(&auth_dir, GEMINI_AUTH_FILENAME, "auth/gemini.json");
                    let codex_insp =
                        inspect_auth_file(&auth_dir, CODEX_AUTH_FILENAME, "auth/codex.json");
                    let lock_insp = inspect_auth_file(
                        &auth_dir,
                        CODEX_REFRESH_LOCK_FILENAME,
                        "auth/.codex-refresh.lock",
                    );

                    #[cfg(unix)]
                    let mut ok_parts: Vec<String> =
                        vec![format!("auth directory ({dir_mode:04o})")];

                    for (label, insp) in [
                        ("auth/gemini.json", &gemini_insp),
                        ("auth/codex.json", &codex_insp),
                        ("auth/.codex-refresh.lock", &lock_insp),
                    ] {
                        match insp {
                            AuthFileInspection::Missing => {}
                            AuthFileInspection::Present {
                                #[cfg(unix)]
                                mode,
                                perm_err,
                            } => {
                                if let Some(err) = perm_err {
                                    perm_errors.push(err.clone());
                                } else {
                                    #[cfg(unix)]
                                    ok_parts.push(format!("{label} ({mode:04o})"));
                                    #[cfg(not(unix))]
                                    let _ = label;
                                }
                            }
                            AuthFileInspection::Rejected { perm_err, .. } => {
                                perm_errors.push(perm_err.clone());
                            }
                        }
                    }

                    if !perm_errors.is_empty() {
                        checks.push(DoctorCheck {
                            name: "auth_permissions".to_string(),
                            status: CheckStatus::Fail,
                            critical: true,
                            message: perm_errors.join("; "),
                        });
                    } else {
                        #[cfg(unix)]
                        let msg = format!("{} permissions are owner-only", ok_parts.join(" and "));
                        #[cfg(not(unix))]
                        let msg =
                            "Unix permission check not applicable on this platform".to_string();
                        checks.push(DoctorCheck {
                            name: "auth_permissions".to_string(),
                            status: CheckStatus::Pass,
                            critical: true,
                            message: msg,
                        });
                    }

                    // Gemini credentials check
                    match gemini_insp {
                        AuthFileInspection::Missing => {
                            checks.push(DoctorCheck {
                                name: "auth_gemini".to_string(),
                                status: CheckStatus::Pass,
                                critical: true,
                                message: "Gemini credentials file not present".to_string(),
                            });
                        }
                        AuthFileInspection::Rejected { file_err, .. } => {
                            checks.push(DoctorCheck {
                                name: "auth_gemini".to_string(),
                                status: CheckStatus::Fail,
                                critical: true,
                                message: file_err,
                            });
                        }
                        AuthFileInspection::Present { .. } => {
                            match FileAuthStore::for_config_path(config_path)
                                .and_then(|store| store.load_gemini())
                            {
                                Ok(Some(_)) => {
                                    has_stored_gemini_key = true;
                                    checks.push(DoctorCheck {
                                        name: "auth_gemini".to_string(),
                                        status: CheckStatus::Pass,
                                        critical: true,
                                        message:
                                            "Gemini credentials valid (stored in auth/gemini.json)"
                                                .to_string(),
                                    });
                                }
                                Ok(None) => {
                                    checks.push(DoctorCheck {
                                        name: "auth_gemini".to_string(),
                                        status: CheckStatus::Pass,
                                        critical: true,
                                        message:
                                            "Gemini credentials file contains no active API key"
                                                .to_string(),
                                    });
                                }
                                Err(err) => {
                                    checks.push(DoctorCheck {
                                        name: "auth_gemini".to_string(),
                                        status: CheckStatus::Fail,
                                        critical: true,
                                        message: err.to_string(),
                                    });
                                }
                            }
                        }
                    }

                    // OpenAI Codex credentials check
                    match codex_insp {
                        AuthFileInspection::Missing => {
                            checks.push(DoctorCheck {
                                name: "auth_codex".to_string(),
                                status: CheckStatus::Pass,
                                critical: true,
                                message: "OpenAI Codex credentials file not present".to_string(),
                            });
                        }
                        AuthFileInspection::Rejected { file_err, .. } => {
                            checks.push(DoctorCheck {
                                name: "auth_codex".to_string(),
                                status: CheckStatus::Fail,
                                critical: true,
                                message: file_err,
                            });
                        }
                        AuthFileInspection::Present { .. } => {
                            match FileAuthStore::for_config_path(config_path)
                                .and_then(|store| store.load_codex())
                            {
                                Ok(None) => {
                                    checks.push(DoctorCheck {
                                        name: "auth_codex".to_string(),
                                        status: CheckStatus::Pass,
                                        critical: true,
                                        message: "OpenAI Codex credentials file not present"
                                            .to_string(),
                                    });
                                }
                                Ok(Some(creds)) => {
                                    let now_ms = current_utc_epoch_ms();
                                    if creds.expires_at_ms > now_ms {
                                        has_valid_codex_auth = true;
                                        let mut details: Vec<String> = Vec::new();
                                        if let Some(ref plan) = creds.plan_type {
                                            details.push(format!("plan: {plan}"));
                                        }
                                        if let Some(hint) = creds.account_hint() {
                                            details.push(format!("account: {hint}"));
                                        }
                                        if let Some(exp) = creds.expires_at_rfc3339() {
                                            details.push(format!("expires: {exp}"));
                                        }
                                        let suffix = if details.is_empty() {
                                            String::new()
                                        } else {
                                            format!(" ({})", details.join(", "))
                                        };
                                        checks.push(DoctorCheck {
                                            name: "auth_codex".to_string(),
                                            status: CheckStatus::Pass,
                                            critical: true,
                                            message: format!(
                                                "OpenAI Codex credentials valid in auth/codex.json{suffix}"
                                            ),
                                        });
                                    } else {
                                        checks.push(DoctorCheck {
                                            name: "auth_codex".to_string(),
                                            status: CheckStatus::Warn,
                                            critical: false,
                                            message: "OpenAI Codex session expired (auth/codex.json) — will refresh on next request or sign in again in Antigravity Companion".to_string(),
                                        });
                                    }
                                }
                                Err(err) => {
                                    checks.push(DoctorCheck {
                                        name: "auth_codex".to_string(),
                                        status: CheckStatus::Fail,
                                        critical: true,
                                        message: err.to_string(),
                                    });
                                }
                            }
                        }
                    }
                }
            },
        },
    }

    let target_port = options
        .port_override
        .or_else(|| loaded_config.as_ref().map(|c| c.proxy.port.get()))
        .unwrap_or(DEFAULT_LISTEN_PORT);

    let probe_result =
        match LoopbackProxyClient::new(options.health_timeout, options.health_connect_timeout) {
            Ok(client) => client
                .probe_health(target_port, None)
                .await
                .map_err(|err| err.to_string()),
            Err(err) => Err(err.to_string()),
        };

    let (proxy_summary, version_skew, protocol_skew) = match probe_result {
        Ok(health) => {
            let eval = evaluate_health_response(
                &health,
                &options.binary_version,
                options.expected_protocol_version,
            );
            checks.push(DoctorCheck {
                name: "proxy_health".to_string(),
                status: if eval.status_ok {
                    CheckStatus::Pass
                } else {
                    CheckStatus::Fail
                },
                critical: true,
                message: eval.health_message,
            });
            checks.push(DoctorCheck {
                name: "version_match".to_string(),
                status: if eval.version_skew || eval.protocol_skew {
                    CheckStatus::Fail
                } else {
                    CheckStatus::Pass
                },
                critical: true,
                message: eval.version_message,
            });
            let summary = ProxyHealthSummary::from_health(&health);
            (summary, eval.version_skew, eval.protocol_skew)
        }
        Err(err_msg) => {
            checks.push(DoctorCheck {
                name: "proxy_health".to_string(),
                status: CheckStatus::Fail,
                critical: true,
                message: err_msg.clone(),
            });
            checks.push(DoctorCheck {
                name: "version_match".to_string(),
                status: CheckStatus::Skip,
                critical: true,
                message: "skipped because proxy /health did not return a valid response"
                    .to_string(),
            });
            let summary = ProxyHealthSummary::unreachable(err_msg);
            (summary, false, false)
        }
    };

    let gemini_api_key_set = matches!(
        env.gemini_api_key,
        EnvKeyState::Set | EnvKeyState::SetInvalidUtf8
    );
    let has_gemini_configured =
        has_stored_gemini_key || matches!(env.gemini_api_key, EnvKeyState::Set);
    let has_codex_configured = has_valid_codex_auth;
    let (api_key_status, api_key_message) = match env.gemini_api_key {
        EnvKeyState::Set => (CheckStatus::Pass, "GEMINI_API_KEY is set".to_string()),
        EnvKeyState::SetInvalidUtf8 => (
            CheckStatus::Warn,
            "GEMINI_API_KEY is set (invalid UTF-8)".to_string(),
        ),
        EnvKeyState::Placeholder if has_stored_gemini_key => (
            CheckStatus::Pass,
            "GEMINI_API_KEY is set to the local proxy placeholder (using API key saved in auth/gemini.json)"
                .to_string(),
        ),
        EnvKeyState::Placeholder if has_codex_configured => (
            CheckStatus::Info,
            "GEMINI_API_KEY is set to the local proxy placeholder (OpenAI Codex is configured in auth/codex.json)"
                .to_string(),
        ),
        EnvKeyState::Placeholder => (
            CheckStatus::Warn,
            "GEMINI_API_KEY is set to the local proxy placeholder and no API key is saved in auth/gemini.json"
                .to_string(),
        ),
        EnvKeyState::Unset if has_stored_gemini_key => (
            CheckStatus::Pass,
            "GEMINI_API_KEY is unset (using API key saved in auth/gemini.json)".to_string(),
        ),
        EnvKeyState::Unset if has_codex_configured => (
            CheckStatus::Info,
            "GEMINI_API_KEY is unset (OpenAI Codex is configured in auth/codex.json)".to_string(),
        ),
        EnvKeyState::Unset => (
            CheckStatus::Warn,
            "GEMINI_API_KEY is unset and no API key is saved in auth/gemini.json".to_string(),
        ),
    };
    checks.push(DoctorCheck {
        name: "env_gemini_api_key".to_string(),
        status: api_key_status,
        critical: false,
        message: api_key_message,
    });

    let (provider_auth_status, provider_auth_message) =
        match (has_gemini_configured, has_codex_configured) {
            (true, true) => (
                CheckStatus::Pass,
                "configured providers: Google AI Studio (Gemini), OpenAI Codex".to_string(),
            ),
            (true, false) => (
                CheckStatus::Pass,
                "configured providers: Google AI Studio (Gemini)".to_string(),
            ),
            (false, true) => (
                CheckStatus::Pass,
                "configured providers: OpenAI Codex".to_string(),
            ),
            (false, false) => (
                CheckStatus::Warn,
                "no active provider credentials configured (configure Google AI Studio or sign in with OpenAI Codex in Antigravity Companion)"
                    .to_string(),
            ),
        };
    checks.push(DoctorCheck {
        name: "provider_auth".to_string(),
        status: provider_auth_status,
        critical: false,
        message: provider_auth_message,
    });

    let (base_url_set, base_url_matches_proxy, base_url_status, base_url_message) = match env
        .google_gemini_base_url
    {
        EnvUrlState::Unset => (
            false,
            false,
            CheckStatus::Info,
            format!(
                "GOOGLE_GEMINI_BASE_URL is unset (set to http://127.0.0.1:{target_port} or use customModelsConfig)"
            ),
        ),
        EnvUrlState::SetInvalidUtf8 => (
            true,
            false,
            CheckStatus::Warn,
            "GOOGLE_GEMINI_BASE_URL is set (invalid UTF-8)".to_string(),
        ),
        EnvUrlState::Set(raw) => match sanitize_base_url(&raw, target_port) {
            Some((sanitized, true)) => (
                true,
                true,
                CheckStatus::Pass,
                format!("GOOGLE_GEMINI_BASE_URL targets local proxy ({sanitized})"),
            ),
            Some((sanitized, false)) => (
                true,
                false,
                CheckStatus::Warn,
                format!(
                    "GOOGLE_GEMINI_BASE_URL ({sanitized}) does not target http://127.0.0.1:{target_port}"
                ),
            ),
            None => (
                true,
                false,
                CheckStatus::Warn,
                "GOOGLE_GEMINI_BASE_URL is set but is not a valid http/https URL".to_string(),
            ),
        },
    };
    checks.push(DoctorCheck {
        name: "env_google_gemini_base_url".to_string(),
        status: base_url_status,
        critical: false,
        message: base_url_message,
    });

    let ok = !checks
        .iter()
        .any(|check| check.critical && check.status == CheckStatus::Fail);

    Ok(DoctorReport {
        ok,
        binary_version: options.binary_version,
        expected_protocol_version: options.expected_protocol_version,
        config_path: config_path_display,
        target_port,
        proxy: proxy_summary,
        version_skew,
        protocol_skew,
        env: EnvSummary {
            gemini_api_key_set,
            google_gemini_base_url_set: base_url_set,
            google_gemini_base_url_matches_proxy: base_url_matches_proxy,
        },
        checks,
    })
}

pub fn format_doctor_json(report: &DoctorReport) -> Result<String, CliError> {
    let pretty = serde_json::to_string_pretty(report)
        .map_err(|err| CliError::Serialization(err.to_string()))?;
    Ok(format!("{pretty}\n"))
}

pub fn format_doctor_human(report: &DoctorReport) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "agy-companion doctor (v{}, protocol v{})",
        report.binary_version, report.expected_protocol_version
    );
    let _ = writeln!(out, "Config path: {}", report.config_path);
    let _ = writeln!(out, "Proxy target: http://127.0.0.1:{}", report.target_port);
    for check in &report.checks {
        let _ = writeln!(
            out,
            "[{}] {}: {}",
            check.status.tag(),
            check.name,
            check.message
        );
    }
    let _ = writeln!(out, "Result: {}", if report.ok { "OK" } else { "FAILED" });
    out
}

pub async fn execute_doctor(
    config_override: Option<PathBuf>,
    port_override: Option<u16>,
    json: bool,
    out: &mut impl Write,
) -> Result<bool, CliError> {
    let options = DoctorOptions {
        config_override,
        port_override,
        ..DoctorOptions::default()
    };
    let report = run_doctor(options, DoctorEnv::from_process()).await?;
    let rendered = if json {
        format_doctor_json(&report)?
    } else {
        format_doctor_human(&report)
    };
    out.write_all(rendered.as_bytes())?;
    out.flush()?;
    Ok(report.ok)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use axum::Router;
    use axum::http::StatusCode;
    use axum::routing::get;
    use companion_core::{
        ApiKey, AuthStore, Config, FileAuthStore, GEMINI_AUTH_FILENAME, HealthResponse,
        MAX_AUTH_FILE_BYTES, PROTOCOL_VERSION, auth_dir_for_config_path,
    };
    use tempfile::TempDir;
    use tokio::net::TcpListener;

    use super::{
        CheckStatus, DoctorEnv, DoctorOptions, EnvKeyState, EnvUrlState, evaluate_health_response,
        format_doctor_human, format_doctor_json, run_doctor,
    };

    async fn spawn_mock_health_router(router: Router) -> (u16, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let port = listener.local_addr().expect("local_addr").port();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        (port, handle)
    }

    fn healthy_router() -> Router {
        Router::new().route(
            "/health",
            get(|| async {
                axum::Json(HealthResponse {
                    status: "ok".to_string(),
                    version: env!("CARGO_PKG_VERSION").to_string(),
                    protocol_version: PROTOCOL_VERSION,
                    role: None,
                })
            }),
        )
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn doctor_detects_version_skew_between_binary_and_health_response() {
        let router = Router::new().route(
            "/health",
            get(|| async {
                axum::Json(HealthResponse {
                    status: "ok".to_string(),
                    version: "9.9.9".to_string(),
                    protocol_version: PROTOCOL_VERSION,
                    role: None,
                })
            }),
        );
        let (port, handle) = spawn_mock_health_router(router).await;
        let temp = TempDir::new().expect("tempdir");
        let cfg_path = temp.path().join("config.json");
        Config::default()
            .save_secure(&cfg_path)
            .expect("save_secure");

        let report = run_doctor(
            DoctorOptions {
                config_override: Some(cfg_path),
                port_override: Some(port),
                ..DoctorOptions::default()
            },
            DoctorEnv::from_raw_values(None, None),
        )
        .await
        .expect("doctor report");
        handle.abort();

        assert!(report.version_skew);
        assert!(!report.protocol_skew);
        assert!(!report.ok);
        let version_check = report
            .checks
            .iter()
            .find(|c| c.name == "version_match")
            .expect("version_match check");
        assert_eq!(version_check.status, CheckStatus::Fail);
        assert!(version_check.message.contains("version skew"));
        assert!(version_check.message.contains(env!("CARGO_PKG_VERSION")));
        assert!(version_check.message.contains("9.9.9"));
    }

    #[test]
    fn doctor_detects_protocol_skew_and_non_ok_status() {
        let protocol_skew = evaluate_health_response(
            &HealthResponse {
                status: "ok".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                protocol_version: 99,
                role: None,
            },
            env!("CARGO_PKG_VERSION"),
            PROTOCOL_VERSION,
        );
        assert!(protocol_skew.status_ok);
        assert!(!protocol_skew.version_skew);
        assert!(protocol_skew.protocol_skew);
        assert!(protocol_skew.version_message.contains("protocol skew"));
        assert!(protocol_skew.version_message.contains("99"));

        let degraded = evaluate_health_response(
            &HealthResponse {
                status: "degraded".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                protocol_version: PROTOCOL_VERSION,
                role: None,
            },
            env!("CARGO_PKG_VERSION"),
            PROTOCOL_VERSION,
        );
        assert!(!degraded.status_ok);
        assert!(!degraded.version_skew);
        assert!(!degraded.protocol_skew);

        let healthy = evaluate_health_response(
            &HealthResponse {
                status: "ok".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                protocol_version: PROTOCOL_VERSION,
                role: None,
            },
            env!("CARGO_PKG_VERSION"),
            PROTOCOL_VERSION,
        );
        assert!(healthy.status_ok);
        assert!(!healthy.version_skew);
        assert!(!healthy.protocol_skew);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn doctor_maps_probe_failure_to_failed_health_and_skipped_version_without_body_leak() {
        let temp = TempDir::new().expect("tempdir");
        let missing_cfg = temp.path().join("missing.json");

        let err500_router = Router::new().route(
            "/health",
            get(|| async {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "secret-body-should-not-echo",
                )
            }),
        );
        let (port, handle) = spawn_mock_health_router(err500_router).await;
        let rep = run_doctor(
            DoctorOptions {
                config_override: Some(missing_cfg),
                port_override: Some(port),
                ..DoctorOptions::default()
            },
            DoctorEnv::from_raw_values(None, None),
        )
        .await
        .expect("500 report");
        handle.abort();

        assert!(!rep.ok);
        assert!(!rep.proxy.reachable);
        assert!(rep.proxy.error.as_deref().unwrap_or("").contains("500"));
        assert!(!rep.version_skew);
        assert!(!rep.protocol_skew);

        let health_check = rep
            .checks
            .iter()
            .find(|c| c.name == "proxy_health")
            .expect("proxy_health check");
        assert_eq!(health_check.status, CheckStatus::Fail);
        assert!(health_check.critical);

        let version_check = rep
            .checks
            .iter()
            .find(|c| c.name == "version_match")
            .expect("version_match check");
        assert_eq!(version_check.status, CheckStatus::Skip);
        assert!(version_check.critical);

        let json = format_doctor_json(&rep).expect("json");
        assert!(json.contains("500"));
        assert!(!json.contains("secret-body-should-not-echo"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn doctor_checks_config_permissions_0600_0400_0644_missing_symlink_and_dir() {
        let (port, handle) = spawn_mock_health_router(healthy_router()).await;
        let temp = TempDir::new().expect("tempdir");

        // 1. Missing config -> non-fatal pass/info, overall ok because proxy is healthy
        let missing = temp.path().join("does-not-exist.json");
        let rep = run_doctor(
            DoctorOptions {
                config_override: Some(missing),
                port_override: Some(port),
                ..DoctorOptions::default()
            },
            DoctorEnv::from_raw_values(None, None),
        )
        .await
        .expect("missing report");
        assert!(rep.ok, "missing config should not fail doctor");

        // 2. Valid 0600 regular file -> Pass
        let cfg_0600 = temp.path().join("cfg_0600.json");
        Config::default().save_secure(&cfg_0600).expect("save");
        let rep = run_doctor(
            DoctorOptions {
                config_override: Some(cfg_0600.clone()),
                port_override: Some(port),
                ..DoctorOptions::default()
            },
            DoctorEnv::from_raw_values(None, None),
        )
        .await
        .expect("0600 report");
        assert!(rep.ok);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            // 3. Valid 0400 regular file -> Pass
            let cfg_0400 = temp.path().join("cfg_0400.json");
            Config::default().save_secure(&cfg_0400).expect("save");
            fs::set_permissions(&cfg_0400, fs::Permissions::from_mode(0o400)).expect("chmod 0400");
            let rep = run_doctor(
                DoctorOptions {
                    config_override: Some(cfg_0400),
                    port_override: Some(port),
                    ..DoctorOptions::default()
                },
                DoctorEnv::from_raw_values(None, None),
            )
            .await
            .expect("0400 report");
            assert!(rep.ok);

            // 4. Insecure modes 0644, 0640, 0604 -> Fail with octal mode & chmod 600 remediation
            for mode in [0o644u32, 0o640, 0o604] {
                let p = temp.path().join(format!("cfg_{mode:o}.json"));
                Config::default().save_secure(&p).expect("save");
                fs::set_permissions(&p, fs::Permissions::from_mode(mode)).expect("chmod");
                let rep = run_doctor(
                    DoctorOptions {
                        config_override: Some(p),
                        port_override: Some(port),
                        ..DoctorOptions::default()
                    },
                    DoctorEnv::from_raw_values(None, None),
                )
                .await
                .expect("insecure mode report");
                assert!(!rep.ok, "mode {mode:04o} must fail doctor");
                let perm_check = rep
                    .checks
                    .iter()
                    .find(|c| c.name == "config_permissions")
                    .expect("config_permissions check");
                assert_eq!(perm_check.status, CheckStatus::Fail);
                assert!(perm_check.message.contains(&format!("{mode:04o}")));
                assert!(perm_check.message.contains("chmod 600"));
            }

            // 5. Symlink pointing to a 0600 regular file -> Fail
            let link_path = temp.path().join("cfg_symlink.json");
            std::os::unix::fs::symlink(&cfg_0600, &link_path).expect("symlink");
            let rep = run_doctor(
                DoctorOptions {
                    config_override: Some(link_path),
                    port_override: Some(port),
                    ..DoctorOptions::default()
                },
                DoctorEnv::from_raw_values(None, None),
            )
            .await
            .expect("symlink report");
            assert!(!rep.ok, "symlink config must fail doctor");
            let perm_check = rep
                .checks
                .iter()
                .find(|c| c.name == "config_permissions")
                .expect("config_permissions check");
            assert_eq!(perm_check.status, CheckStatus::Fail);
            assert!(perm_check.message.to_lowercase().contains("symlink"));

            // 5b. Dangling symlink -> Fail (must not be treated as missing)
            let dangling_link = temp.path().join("cfg_dangling.json");
            std::os::unix::fs::symlink(temp.path().join("nonexistent_target.json"), &dangling_link)
                .expect("dangling symlink");
            let rep = run_doctor(
                DoctorOptions {
                    config_override: Some(dangling_link),
                    port_override: Some(port),
                    ..DoctorOptions::default()
                },
                DoctorEnv::from_raw_values(None, None),
            )
            .await
            .expect("dangling symlink report");
            assert!(!rep.ok, "dangling symlink must fail doctor");

            // 5c. Special mode bit (sticky 01600) -> Fail
            let sticky_path = temp.path().join("cfg_sticky.json");
            Config::default().save_secure(&sticky_path).expect("save");
            fs::set_permissions(&sticky_path, fs::Permissions::from_mode(0o1600))
                .expect("chmod 1600");
            let rep = run_doctor(
                DoctorOptions {
                    config_override: Some(sticky_path),
                    port_override: Some(port),
                    ..DoctorOptions::default()
                },
                DoctorEnv::from_raw_values(None, None),
            )
            .await
            .expect("sticky mode report");
            assert!(!rep.ok, "sticky bit 01600 must fail doctor");
        }

        // 6. Directory at config path -> Fail before reading
        let dir_path = temp.path().join("cfg_dir");
        fs::create_dir(&dir_path).expect("mkdir");
        let rep = run_doctor(
            DoctorOptions {
                config_override: Some(dir_path),
                port_override: Some(port),
                ..DoctorOptions::default()
            },
            DoctorEnv::from_raw_values(None, None),
        )
        .await
        .expect("dir report");
        assert!(!rep.ok);

        // 7. Oversized > 1 MiB file -> Fail before reading
        let big_path = temp.path().join("cfg_big.json");
        fs::write(&big_path, vec![b' '; (1024 * 1024) + 16]).expect("write big");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&big_path, fs::Permissions::from_mode(0o600)).expect("chmod");
        }
        let rep = run_doctor(
            DoctorOptions {
                config_override: Some(big_path),
                port_override: Some(port),
                ..DoctorOptions::default()
            },
            DoctorEnv::from_raw_values(None, None),
        )
        .await
        .expect("oversized file report");
        assert!(!rep.ok);

        handle.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn doctor_fails_on_invalid_config_json_and_ssrf_host_without_leaking_secret() {
        let (port, handle) = spawn_mock_health_router(healthy_router()).await;
        let temp = TempDir::new().expect("tempdir");

        // Corrupt JSON containing a secret
        let corrupt = temp.path().join("corrupt.json");
        fs::write(
            &corrupt,
            r#"{"gemini":{"api_key":"AIzaSyDoctorCorruptSecret999"}, invalid"#,
        )
        .expect("write corrupt");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&corrupt, fs::Permissions::from_mode(0o600)).expect("chmod");
        }

        let rep = run_doctor(
            DoctorOptions {
                config_override: Some(corrupt),
                port_override: Some(port),
                ..DoctorOptions::default()
            },
            DoctorEnv::from_raw_values(None, None),
        )
        .await
        .expect("corrupt report");
        assert!(!rep.ok);
        assert!(rep.proxy.reachable, "remaining checks must still run");
        let json = format_doctor_json(&rep).expect("json");
        let human = format_doctor_human(&rep);
        assert!(!json.contains("AIzaSyDoctorCorruptSecret999"));
        assert!(!human.contains("AIzaSyDoctorCorruptSecret999"));

        // Non-loopback host + remote http:// upstream
        let ssrf = temp.path().join("ssrf.json");
        fs::write(
            &ssrf,
            r#"{"schema_version":1,"proxy":{"host":"0.0.0.0","port":4141},"providers":{"gemini":{"base_url":"http://evil.com","api_key":"AIzaSyDoctorSsrfSecret888","default_model":"gemini-4-hydrogen","timeout_secs":900}}}"#,
        )
        .expect("write ssrf");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&ssrf, fs::Permissions::from_mode(0o600)).expect("chmod");
        }

        let rep = run_doctor(
            DoctorOptions {
                config_override: Some(ssrf),
                port_override: Some(port),
                ..DoctorOptions::default()
            },
            DoctorEnv::from_raw_values(None, None),
        )
        .await
        .expect("ssrf report");
        assert!(!rep.ok);
        let json = format_doctor_json(&rep).expect("json");
        let human = format_doctor_human(&rep);
        assert!(!json.contains("AIzaSyDoctorSsrfSecret888"));
        assert!(!human.contains("AIzaSyDoctorSsrfSecret888"));

        handle.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn doctor_redacts_gemini_api_key_and_sanitizes_base_url_env() {
        let (port, handle) = spawn_mock_health_router(healthy_router()).await;
        let temp = TempDir::new().expect("tempdir");
        let cfg = temp.path().join("config.json");
        Config::default().save_secure(&cfg).expect("save");

        // 1. Secret in GEMINI_API_KEY + query key in GOOGLE_GEMINI_BASE_URL
        // Use non-numeric suffixes (total length != 39 chars) so OS ephemeral ports never collide.
        let env = DoctorEnv::from_raw_values(
            Some("AIzaSyEnvSecretNeverPrintAlpha"),
            Some(&format!("http://127.0.0.1:{port}?key=AIzaSyUrlSecretBeta")),
        );
        let rep = run_doctor(
            DoctorOptions {
                config_override: Some(cfg.clone()),
                port_override: Some(port),
                ..DoctorOptions::default()
            },
            env,
        )
        .await
        .expect("env report");
        assert!(rep.ok);
        assert!(rep.env.gemini_api_key_set);
        assert!(rep.env.google_gemini_base_url_set);
        assert!(rep.env.google_gemini_base_url_matches_proxy);

        let json = format_doctor_json(&rep).expect("json");
        let human = format_doctor_human(&rep);
        for forbidden in [
            "AIzaSyEnvSecretNeverPrintAlpha",
            "AIza",
            "NeverPrintAlpha",
            "AIzaSyUrlSecretBeta",
            "SecretBeta",
            "?key=",
        ] {
            assert!(!json.contains(forbidden), "JSON leaked '{forbidden}'");
            assert!(!human.contains(forbidden), "Human leaked '{forbidden}'");
        }

        // 2. Whitespace-only GEMINI_API_KEY -> treated as unset
        let blank_env = DoctorEnv::from_raw_values(
            Some("   \t\n  "),
            Some("https://user:supersecretpass@example.com/v1beta#frag"),
        );
        let rep2 = run_doctor(
            DoctorOptions {
                config_override: Some(cfg.clone()),
                port_override: Some(port),
                ..DoctorOptions::default()
            },
            blank_env,
        )
        .await
        .expect("blank env report");
        assert!(rep2.ok, "advisory env warnings must not fail doctor");
        assert!(!rep2.env.gemini_api_key_set);
        assert!(rep2.env.google_gemini_base_url_set);
        assert!(!rep2.env.google_gemini_base_url_matches_proxy);
        let json2 = format_doctor_json(&rep2).expect("json2");
        let human2 = format_doctor_human(&rep2);
        for forbidden in ["user:", "supersecretpass", "#frag"] {
            assert!(!json2.contains(forbidden), "JSON2 leaked '{forbidden}'");
            assert!(!human2.contains(forbidden), "Human2 leaked '{forbidden}'");
        }

        // 3. Invalid UTF-8 env states -> advisory warning, never fatal
        let non_utf8_env = DoctorEnv {
            gemini_api_key: EnvKeyState::SetInvalidUtf8,
            google_gemini_base_url: EnvUrlState::SetInvalidUtf8,
        };
        let rep3 = run_doctor(
            DoctorOptions {
                config_override: Some(cfg),
                port_override: Some(port),
                ..DoctorOptions::default()
            },
            non_utf8_env,
        )
        .await
        .expect("non-utf8 env report");
        assert!(rep3.ok);
        assert!(rep3.env.gemini_api_key_set);
        assert!(rep3.env.google_gemini_base_url_set);

        handle.abort();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn doctor_rejects_trailing_slash_and_dot_suffix_symlinks() {
        use std::os::unix::fs::symlink;
        use std::path::PathBuf;

        let (port, handle) = spawn_mock_health_router(healthy_router()).await;
        let temp = TempDir::new().expect("tempdir");

        let real_file = temp.path().join("real_0600.json");
        Config::default().save_secure(&real_file).expect("save");
        let real_dir = temp.path().join("real_dir");
        fs::create_dir(&real_dir).expect("mkdir");

        let dangling_link = temp.path().join("dangling_symlink.json");
        symlink(temp.path().join("nonexistent_target.json"), &dangling_link)
            .expect("dangling symlink");

        let file_link = temp.path().join("file_symlink.json");
        symlink(&real_file, &file_link).expect("file symlink");

        let dir_link = temp.path().join("dir_symlink");
        symlink(&real_dir, &dir_link).expect("dir symlink");

        let cases = [
            (
                "dangling/",
                PathBuf::from(format!("{}/", dangling_link.display())),
            ),
            (
                "dangling//",
                PathBuf::from(format!("{}//", dangling_link.display())),
            ),
            (
                "dangling/.",
                PathBuf::from(format!("{}/.", dangling_link.display())),
            ),
            (
                "sym_file/",
                PathBuf::from(format!("{}/", file_link.display())),
            ),
            (
                "sym_file/.",
                PathBuf::from(format!("{}/.", file_link.display())),
            ),
            (
                "sym_dir/",
                PathBuf::from(format!("{}/", dir_link.display())),
            ),
            (
                "sym_dir/.",
                PathBuf::from(format!("{}/.", dir_link.display())),
            ),
        ];

        for (label, path) in cases {
            let rep = run_doctor(
                DoctorOptions {
                    config_override: Some(path),
                    port_override: Some(port),
                    ..DoctorOptions::default()
                },
                DoctorEnv::from_raw_values(None, None),
            )
            .await
            .expect("doctor report");

            assert!(!rep.ok, "doctor must reject {label}");
            let perm_check = rep
                .checks
                .iter()
                .find(|c| c.name == "config_permissions")
                .expect("config_permissions check");
            assert_eq!(
                perm_check.status,
                CheckStatus::Fail,
                "{label}: expected config_permissions fail, got {:?}",
                perm_check
            );
            assert!(
                perm_check.message.to_lowercase().contains("symlink"),
                "{label}: expected symlink rejection message, got '{}'",
                perm_check.message
            );

            let cfg_check = rep
                .checks
                .iter()
                .find(|c| c.name == "config")
                .expect("config check");
            assert_eq!(
                cfg_check.status,
                CheckStatus::Fail,
                "{label}: expected config fail, got {:?}",
                cfg_check
            );
            assert!(
                cfg_check.message.to_lowercase().contains("symlink"),
                "{label}: expected symlink message in config check, got '{}'",
                cfg_check.message
            );

            let json = format_doctor_json(&rep).expect("format json");
            assert!(
                json.contains("\"ok\": false"),
                "{label}: json missing ok:false"
            );
            let human = format_doctor_human(&rep);
            assert!(
                human.contains("Result: FAILED"),
                "{label}: human missing FAILED"
            );
            assert!(
                human.to_lowercase().contains("symlink"),
                "{label}: human missing symlink"
            );
        }

        // Edge case: regular 0600 file with trailing slash still fails safely on load (ENOTDIR).
        let regular_trailing_slash = PathBuf::from(format!("{}/", real_file.display()));
        let rep = run_doctor(
            DoctorOptions {
                config_override: Some(regular_trailing_slash),
                port_override: Some(port),
                ..DoctorOptions::default()
            },
            DoctorEnv::from_raw_values(None, None),
        )
        .await
        .expect("regular trailing slash report");
        assert!(
            !rep.ok,
            "regular config file with trailing slash must fail safely"
        );

        handle.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn doctor_fails_on_non_header_safe_api_key_in_auth_store() {
        let (port, handle) = spawn_mock_health_router(healthy_router()).await;
        let temp = TempDir::new().expect("tempdir");

        for (label, bad_key) in [
            ("internal space", "AIzaSy Space In Key 1234"),
            ("nbsp", "AIzaSy\u{00A0}NbspKey1234"),
            ("emoji", "AIzaSy\u{1F511}EmojiKey1234"),
        ] {
            let case_dir = temp.path().join(label.replace(' ', "_"));
            fs::create_dir_all(&case_dir).expect("mkdir case");
            let cfg_path = case_dir.join("config.json");
            Config::default()
                .save_secure(&cfg_path)
                .expect("save config");

            let auth_dir = auth_dir_for_config_path(&cfg_path).expect("auth dir");
            fs::create_dir_all(&auth_dir).expect("mkdir auth");
            let auth_file = auth_dir.join(GEMINI_AUTH_FILENAME);
            fs::write(
                &auth_file,
                serde_json::json!({
                    "schema_version": 1,
                    "api_key": bad_key
                })
                .to_string(),
            )
            .expect("write bad auth");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&auth_dir, fs::Permissions::from_mode(0o700)).expect("chmod");
                fs::set_permissions(&auth_file, fs::Permissions::from_mode(0o600)).expect("chmod");
            }

            let rep = run_doctor(
                DoctorOptions {
                    config_override: Some(cfg_path),
                    port_override: Some(port),
                    ..DoctorOptions::default()
                },
                DoctorEnv::from_raw_values(None, None),
            )
            .await
            .expect("doctor report");
            assert!(
                !rep.ok,
                "{label}: doctor must fail on non-header-safe api_key in auth/gemini.json"
            );

            let auth_check = rep
                .checks
                .iter()
                .find(|c| c.name == "auth_gemini")
                .expect("auth_gemini check");
            assert_eq!(
                auth_check.status,
                CheckStatus::Fail,
                "{label}: auth_gemini check must fail, got {auth_check:?}"
            );

            let json = format_doctor_json(&rep).expect("format json");
            let human = format_doctor_human(&rep);
            assert!(!json.contains("AIzaSy"), "{label}: JSON leaked secret");
            assert!(!human.contains("AIzaSy"), "{label}: human leaked secret");
        }

        handle.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn doctor_quarantines_placeholder_gemini_api_key_in_env_and_auth_store() {
        let (port, handle) = spawn_mock_health_router(healthy_router()).await;
        let temp = TempDir::new().expect("tempdir");

        // 1. Placeholder in env (exact & padded) with no auth key -> WARN, gemini_api_key_set == false.
        let cfg_empty = temp.path().join("empty").join("config.json");
        Config::default().save_secure(&cfg_empty).expect("save");
        for raw_env in [
            companion_core::PROXY_PLACEHOLDER_API_KEY,
            "  agy-companion-local  ",
        ] {
            let env = DoctorEnv::from_raw_values(Some(raw_env), None);
            assert_eq!(env.gemini_api_key, EnvKeyState::Placeholder);
            let rep = run_doctor(
                DoctorOptions {
                    config_override: Some(cfg_empty.clone()),
                    port_override: Some(port),
                    ..DoctorOptions::default()
                },
                env,
            )
            .await
            .expect("doctor report");
            assert!(rep.ok, "advisory placeholder warning must not fail doctor");
            assert!(!rep.env.gemini_api_key_set);
            let check = rep
                .checks
                .iter()
                .find(|c| c.name == "env_gemini_api_key")
                .expect("env_gemini_api_key check");
            assert_eq!(check.status, CheckStatus::Warn);
            assert!(check.message.contains("local proxy placeholder"));
        }

        // 2. Placeholder in BOTH env and legacy config.json -> ignored, still WARN, not PASS.
        let legacy_dir = temp.path().join("legacy");
        fs::create_dir_all(&legacy_dir).expect("mkdir legacy");
        let cfg_legacy = legacy_dir.join("config.json");
        fs::write(
            &cfg_legacy,
            serde_json::json!({
                "schema_version": 1,
                "providers": {
                    "gemini": {
                        "api_key": companion_core::PROXY_PLACEHOLDER_API_KEY
                    }
                }
            })
            .to_string(),
        )
        .expect("write legacy placeholder");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&cfg_legacy, fs::Permissions::from_mode(0o600))
                .expect("chmod legacy");
        }
        let rep_legacy = run_doctor(
            DoctorOptions {
                config_override: Some(cfg_legacy),
                port_override: Some(port),
                ..DoctorOptions::default()
            },
            DoctorEnv::from_raw_values(Some(companion_core::PROXY_PLACEHOLDER_API_KEY), None),
        )
        .await
        .expect("doctor legacy report");
        assert!(rep_legacy.ok);
        assert!(!rep_legacy.env.gemini_api_key_set);
        let check_legacy = rep_legacy
            .checks
            .iter()
            .find(|c| c.name == "env_gemini_api_key")
            .expect("env_gemini_api_key check");
        assert_eq!(check_legacy.status, CheckStatus::Warn);

        // 2b. Placeholder in auth/gemini.json -> quarantined to None -> WARN (never false-passes!).
        let ph_auth_dir = temp.path().join("ph_auth");
        let cfg_ph_auth = ph_auth_dir.join("config.json");
        Config::default().save_secure(&cfg_ph_auth).expect("save");
        let auth_dir = auth_dir_for_config_path(&cfg_ph_auth).expect("auth_dir");
        fs::create_dir_all(&auth_dir).expect("mkdir auth");
        let ph_auth_file = auth_dir.join(GEMINI_AUTH_FILENAME);
        fs::write(
            &ph_auth_file,
            serde_json::json!({
                "schema_version": 1,
                "api_key": companion_core::PROXY_PLACEHOLDER_API_KEY
            })
            .to_string(),
        )
        .expect("write ph auth");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&auth_dir, fs::Permissions::from_mode(0o700)).expect("chmod");
            fs::set_permissions(&ph_auth_file, fs::Permissions::from_mode(0o600)).expect("chmod");
        }
        let rep_ph_auth = run_doctor(
            DoctorOptions {
                config_override: Some(cfg_ph_auth),
                port_override: Some(port),
                ..DoctorOptions::default()
            },
            DoctorEnv::from_raw_values(Some(companion_core::PROXY_PLACEHOLDER_API_KEY), None),
        )
        .await
        .expect("doctor ph auth report");
        assert!(rep_ph_auth.ok);
        let check_ph_auth = rep_ph_auth
            .checks
            .iter()
            .find(|c| c.name == "env_gemini_api_key")
            .expect("env_gemini_api_key check");
        assert_eq!(check_ph_auth.status, CheckStatus::Warn);

        // 3. Placeholder in env with a REAL saved key in auth/gemini.json -> PASS, gemini_api_key_set == false, zero AIza leak.
        let real_dir = temp.path().join("real");
        let cfg_real = real_dir.join("config.json");
        Config::default().save_secure(&cfg_real).expect("save real");
        FileAuthStore::for_config_path(&cfg_real)
            .expect("store")
            .save_gemini(&companion_core::GeminiCredentials::new(
                ApiKey::parse_strict("AIzaSyRealSavedKey1234").expect("valid key"),
            ))
            .expect("save real auth");
        let rep_real = run_doctor(
            DoctorOptions {
                config_override: Some(cfg_real),
                port_override: Some(port),
                ..DoctorOptions::default()
            },
            DoctorEnv::from_raw_values(Some(companion_core::PROXY_PLACEHOLDER_API_KEY), None),
        )
        .await
        .expect("doctor real report");
        assert!(rep_real.ok);
        assert!(!rep_real.env.gemini_api_key_set);
        let check_real = rep_real
            .checks
            .iter()
            .find(|c| c.name == "env_gemini_api_key")
            .expect("env_gemini_api_key check");
        assert_eq!(check_real.status, CheckStatus::Pass);
        assert!(check_real.message.contains("local proxy placeholder"));
        assert!(check_real.message.contains("auth/gemini.json"));
        let json = format_doctor_json(&rep_real).expect("json");
        let human = format_doctor_human(&rep_real);
        assert!(!json.contains("AIza"));
        assert!(!human.contains("AIza"));

        handle.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn doctor_checks_auth_dir_and_gemini_auth_permissions_symlink_size_and_corrupt() {
        let (port, handle) = spawn_mock_health_router(healthy_router()).await;
        let temp = TempDir::new().expect("tempdir");

        // 1. Oversized (> 64 KiB) auth/gemini.json fails auth_permissions & auth_gemini
        let big_dir = temp.path().join("big");
        let big_cfg = big_dir.join("config.json");
        Config::default().save_secure(&big_cfg).unwrap();
        let big_auth_dir = auth_dir_for_config_path(&big_cfg).unwrap();
        fs::create_dir_all(&big_auth_dir).unwrap();
        let big_auth_file = big_auth_dir.join(GEMINI_AUTH_FILENAME);
        fs::write(
            &big_auth_file,
            vec![b' '; (MAX_AUTH_FILE_BYTES as usize) + 1],
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&big_auth_dir, fs::Permissions::from_mode(0o700)).unwrap();
            fs::set_permissions(&big_auth_file, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let rep_big = run_doctor(
            DoctorOptions {
                config_override: Some(big_cfg),
                port_override: Some(port),
                ..DoctorOptions::default()
            },
            DoctorEnv::from_raw_values(None, None),
        )
        .await
        .unwrap();
        assert!(!rep_big.ok);
        assert_eq!(
            rep_big
                .checks
                .iter()
                .find(|c| c.name == "auth_gemini")
                .unwrap()
                .status,
            CheckStatus::Fail
        );

        // 2. Corrupt JSON in auth/gemini.json fails auth_gemini with zero secret leak
        let corrupt_dir = temp.path().join("corrupt");
        let corrupt_cfg = corrupt_dir.join("config.json");
        Config::default().save_secure(&corrupt_cfg).unwrap();
        let corrupt_auth_dir = auth_dir_for_config_path(&corrupt_cfg).unwrap();
        fs::create_dir_all(&corrupt_auth_dir).unwrap();
        let corrupt_auth_file = corrupt_auth_dir.join(GEMINI_AUTH_FILENAME);
        fs::write(
            &corrupt_auth_file,
            r#"{"schema_version":1,"api_key":"AIzaSyNeverLeakDoc01", broken"#,
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&corrupt_auth_dir, fs::Permissions::from_mode(0o700)).unwrap();
            fs::set_permissions(&corrupt_auth_file, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let rep_corrupt = run_doctor(
            DoctorOptions {
                config_override: Some(corrupt_cfg),
                port_override: Some(port),
                ..DoctorOptions::default()
            },
            DoctorEnv::from_raw_values(None, None),
        )
        .await
        .unwrap();
        assert!(!rep_corrupt.ok);
        assert_eq!(
            rep_corrupt
                .checks
                .iter()
                .find(|c| c.name == "auth_gemini")
                .unwrap()
                .status,
            CheckStatus::Fail
        );
        assert!(
            !format_doctor_json(&rep_corrupt)
                .unwrap()
                .contains("NeverLeakDoc01")
        );
        assert!(!format_doctor_human(&rep_corrupt).contains("NeverLeakDoc01"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::{PermissionsExt, symlink};

            // 3. Open permissions on auth/ (0755) and auth/gemini.json (0644) fail auth_permissions
            let perm_dir = temp.path().join("perms");
            let perm_cfg = perm_dir.join("config.json");
            Config::default().save_secure(&perm_cfg).unwrap();
            let store = FileAuthStore::for_config_path(&perm_cfg).unwrap();
            store
                .save_gemini(&companion_core::GeminiCredentials::new(
                    ApiKey::parse_strict("AIzaSyPermCheckKey01").unwrap(),
                ))
                .unwrap();

            fs::set_permissions(store.auth_dir().unwrap(), fs::Permissions::from_mode(0o755))
                .unwrap();
            let rep_dir_perm = run_doctor(
                DoctorOptions {
                    config_override: Some(perm_cfg.clone()),
                    port_override: Some(port),
                    ..DoctorOptions::default()
                },
                DoctorEnv::from_raw_values(None, None),
            )
            .await
            .unwrap();
            assert!(!rep_dir_perm.ok);
            let dir_check = rep_dir_perm
                .checks
                .iter()
                .find(|c| c.name == "auth_permissions")
                .unwrap();
            assert_eq!(dir_check.status, CheckStatus::Fail);
            assert!(dir_check.message.contains("chmod 700"));

            fs::set_permissions(store.auth_dir().unwrap(), fs::Permissions::from_mode(0o700))
                .unwrap();
            fs::set_permissions(store.gemini_path(), fs::Permissions::from_mode(0o644)).unwrap();
            let rep_file_perm = run_doctor(
                DoctorOptions {
                    config_override: Some(perm_cfg),
                    port_override: Some(port),
                    ..DoctorOptions::default()
                },
                DoctorEnv::from_raw_values(None, None),
            )
            .await
            .unwrap();
            assert!(!rep_file_perm.ok);
            let file_check = rep_file_perm
                .checks
                .iter()
                .find(|c| c.name == "auth_permissions")
                .unwrap();
            assert_eq!(file_check.status, CheckStatus::Fail);
            assert!(file_check.message.contains("chmod 600"));

            // 4. Symlinked auth/ dir and symlinked auth/gemini.json fail closed
            let victim_dir = temp.path().join("victim_auth");
            fs::create_dir_all(&victim_dir).unwrap();
            let victim_file = victim_dir.join(GEMINI_AUTH_FILENAME);
            fs::write(
                &victim_file,
                r#"{"schema_version":1,"api_key":"AIzaSyVictimDocKey01"}"#,
            )
            .unwrap();

            for case in ["sym_dir", "sym_file", "dangling_file"] {
                let root = temp.path().join(case);
                let cfg = root.join("config.json");
                Config::default().save_secure(&cfg).unwrap();
                let adir = auth_dir_for_config_path(&cfg).unwrap();
                match case {
                    "sym_dir" => symlink(&victim_dir, &adir).unwrap(),
                    "sym_file" => {
                        fs::create_dir_all(&adir).unwrap();
                        fs::set_permissions(&adir, fs::Permissions::from_mode(0o700)).unwrap();
                        symlink(&victim_file, adir.join(GEMINI_AUTH_FILENAME)).unwrap();
                    }
                    _ => {
                        fs::create_dir_all(&adir).unwrap();
                        fs::set_permissions(&adir, fs::Permissions::from_mode(0o700)).unwrap();
                        symlink(
                            temp.path().join("nonexistent"),
                            adir.join(GEMINI_AUTH_FILENAME),
                        )
                        .unwrap();
                    }
                }
                let rep = run_doctor(
                    DoctorOptions {
                        config_override: Some(cfg),
                        port_override: Some(port),
                        ..DoctorOptions::default()
                    },
                    DoctorEnv::from_raw_values(None, None),
                )
                .await
                .unwrap();
                assert!(!rep.ok, "{case}: doctor must fail");
                assert_eq!(
                    rep.checks
                        .iter()
                        .find(|c| c.name == "auth_permissions")
                        .unwrap()
                        .status,
                    CheckStatus::Fail,
                    "{case}: auth_permissions must fail"
                );
                assert_eq!(
                    rep.checks
                        .iter()
                        .find(|c| c.name == "auth_gemini")
                        .unwrap()
                        .status,
                    CheckStatus::Fail,
                    "{case}: auth_gemini must fail"
                );
            }
        }

        handle.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn doctor_checks_codex_auth_valid_expired_multi_provider_corrupt_oversized_and_unix_guards()
     {
        use super::current_utc_epoch_ms;
        use companion_core::{
            AUTH_SCHEMA_VERSION, CODEX_AUTH_FILENAME, CodexAuthMode, CodexCredentials,
            GeminiCredentials, SecretString,
        };

        let (port, handle) = spawn_mock_health_router(healthy_router()).await;
        let temp = TempDir::new().expect("tempdir");

        let make_codex = |expires_at_ms: u64| CodexCredentials {
            schema_version: AUTH_SCHEMA_VERSION,
            auth_mode: CodexAuthMode::ChatGptOAuth,
            access_token: SecretString::new("eyJhbGciOiJSUzI1NiJ9.doctor_codex_access_secret.sig"),
            refresh_token: SecretString::new("rt_doctor_codex_refresh_secret"),
            id_token: Some(SecretString::new(
                "eyJhbGciOiJSUzI1NiJ9.doctor_codex_id_secret.sig",
            )),
            account_id: "acct_doctor_99887766wxyz".to_string(),
            expires_at_ms,
            plan_type: Some("pro".to_string()),
            email_hint: Some("j***@example.com".to_string()),
            last_refresh_utc: "2026-11-26T22:00:00Z".to_string(),
        };

        // 1. Codex-only valid auth -> ok == true, auth_codex == Pass, provider_auth == Pass,
        //    env_gemini_api_key == Info, zero raw token/account_id leak.
        let codex_only_cfg = temp.path().join("codex_only").join("config.json");
        Config::default().save_secure(&codex_only_cfg).unwrap();
        let store = FileAuthStore::for_config_path(&codex_only_cfg).unwrap();
        store
            .save_codex(&make_codex(current_utc_epoch_ms() + 3_600_000))
            .unwrap();
        let rep_codex = run_doctor(
            DoctorOptions {
                config_override: Some(codex_only_cfg.clone()),
                port_override: Some(port),
                ..DoctorOptions::default()
            },
            DoctorEnv::from_raw_values(None, None),
        )
        .await
        .unwrap();
        assert!(rep_codex.ok);
        assert_eq!(
            rep_codex
                .checks
                .iter()
                .find(|c| c.name == "auth_codex")
                .unwrap()
                .status,
            CheckStatus::Pass
        );
        assert_eq!(
            rep_codex
                .checks
                .iter()
                .find(|c| c.name == "provider_auth")
                .unwrap()
                .status,
            CheckStatus::Pass
        );
        assert_eq!(
            rep_codex
                .checks
                .iter()
                .find(|c| c.name == "env_gemini_api_key")
                .unwrap()
                .status,
            CheckStatus::Info
        );
        let json = format_doctor_json(&rep_codex).unwrap();
        let human = format_doctor_human(&rep_codex);
        for forbidden in [
            "doctor_codex_access_secret",
            "rt_doctor_codex_refresh_secret",
            "doctor_codex_id_secret",
            "acct_doctor_99887766wxyz",
        ] {
            assert!(
                !json.contains(forbidden),
                "doctor JSON leaked '{forbidden}'"
            );
            assert!(
                !human.contains(forbidden),
                "doctor human output leaked '{forbidden}'"
            );
        }

        // 2. Both Gemini and Codex configured -> Pass on auth_gemini, auth_codex, provider_auth.
        store
            .save_gemini(&GeminiCredentials::new(
                ApiKey::parse_strict("AIzaSyDocBothKey0001").unwrap(),
            ))
            .unwrap();
        let rep_both = run_doctor(
            DoctorOptions {
                config_override: Some(codex_only_cfg),
                port_override: Some(port),
                ..DoctorOptions::default()
            },
            DoctorEnv::from_raw_values(None, None),
        )
        .await
        .unwrap();
        assert!(rep_both.ok);
        assert_eq!(
            rep_both
                .checks
                .iter()
                .find(|c| c.name == "auth_gemini")
                .unwrap()
                .status,
            CheckStatus::Pass
        );
        assert_eq!(
            rep_both
                .checks
                .iter()
                .find(|c| c.name == "auth_codex")
                .unwrap()
                .status,
            CheckStatus::Pass
        );
        let both_pa = rep_both
            .checks
            .iter()
            .find(|c| c.name == "provider_auth")
            .unwrap();
        assert_eq!(both_pa.status, CheckStatus::Pass);
        assert!(both_pa.message.contains("Gemini") && both_pa.message.contains("OpenAI Codex"));

        // 3. Expired Codex credentials (no Gemini) -> ok == true, auth_codex == Warn, provider_auth == Warn.
        let expired_cfg = temp.path().join("expired").join("config.json");
        Config::default().save_secure(&expired_cfg).unwrap();
        FileAuthStore::for_config_path(&expired_cfg)
            .unwrap()
            .save_codex(&make_codex(1_000))
            .unwrap();
        let rep_exp = run_doctor(
            DoctorOptions {
                config_override: Some(expired_cfg),
                port_override: Some(port),
                ..DoctorOptions::default()
            },
            DoctorEnv::from_raw_values(None, None),
        )
        .await
        .unwrap();
        assert!(rep_exp.ok);
        assert_eq!(
            rep_exp
                .checks
                .iter()
                .find(|c| c.name == "auth_codex")
                .unwrap()
                .status,
            CheckStatus::Warn
        );
        assert_eq!(
            rep_exp
                .checks
                .iter()
                .find(|c| c.name == "provider_auth")
                .unwrap()
                .status,
            CheckStatus::Warn
        );

        // 4. Corrupt and oversized auth/codex.json -> ok == false, auth_codex == Fail, zero secret leak.
        for (case, bytes) in [
            (
                "corrupt",
                br#"{"schema_version":1,"access_token":"tok_never_leak_doc", broken"#.to_vec(),
            ),
            ("oversized", vec![b' '; (MAX_AUTH_FILE_BYTES as usize) + 1]),
        ] {
            let cfg = temp
                .path()
                .join(format!("codex_{case}"))
                .join("config.json");
            Config::default().save_secure(&cfg).unwrap();
            let adir = auth_dir_for_config_path(&cfg).unwrap();
            fs::create_dir_all(&adir).unwrap();
            let cfile = adir.join(CODEX_AUTH_FILENAME);
            fs::write(&cfile, bytes).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&adir, fs::Permissions::from_mode(0o700)).unwrap();
                fs::set_permissions(&cfile, fs::Permissions::from_mode(0o600)).unwrap();
            }
            let rep = run_doctor(
                DoctorOptions {
                    config_override: Some(cfg),
                    port_override: Some(port),
                    ..DoctorOptions::default()
                },
                DoctorEnv::from_raw_values(None, None),
            )
            .await
            .unwrap();
            assert!(!rep.ok, "{case}: doctor must fail");
            assert_eq!(
                rep.checks
                    .iter()
                    .find(|c| c.name == "auth_codex")
                    .unwrap()
                    .status,
                CheckStatus::Fail,
                "{case}: auth_codex must fail"
            );
            assert!(
                !format_doctor_json(&rep)
                    .unwrap()
                    .contains("tok_never_leak_doc")
            );
        }

        // 5. Unix 0644 permissions + symlink + dangling symlink on auth/codex.json
        #[cfg(unix)]
        {
            use std::os::unix::fs::{PermissionsExt, symlink};

            let perm_cfg = temp.path().join("codex_0644").join("config.json");
            Config::default().save_secure(&perm_cfg).unwrap();
            let perm_store = FileAuthStore::for_config_path(&perm_cfg).unwrap();
            perm_store
                .save_codex(&make_codex(current_utc_epoch_ms() + 3_600_000))
                .unwrap();
            fs::set_permissions(perm_store.codex_path(), fs::Permissions::from_mode(0o644))
                .unwrap();
            let rep_perm = run_doctor(
                DoctorOptions {
                    config_override: Some(perm_cfg),
                    port_override: Some(port),
                    ..DoctorOptions::default()
                },
                DoctorEnv::from_raw_values(None, None),
            )
            .await
            .unwrap();
            assert!(!rep_perm.ok);
            let perm_chk = rep_perm
                .checks
                .iter()
                .find(|c| c.name == "auth_permissions")
                .unwrap();
            assert_eq!(perm_chk.status, CheckStatus::Fail);
            assert!(perm_chk.message.contains("chmod 600"));

            let victim = temp.path().join("codex_victim.json");
            fs::write(&victim, "keep-victim").unwrap();
            for (case, target) in [
                ("sym_codex", victim.clone()),
                ("dangling_codex", temp.path().join("does-not-exist")),
            ] {
                let cfg = temp.path().join(case).join("config.json");
                Config::default().save_secure(&cfg).unwrap();
                let adir = auth_dir_for_config_path(&cfg).unwrap();
                fs::create_dir_all(&adir).unwrap();
                fs::set_permissions(&adir, fs::Permissions::from_mode(0o700)).unwrap();
                symlink(&target, adir.join(CODEX_AUTH_FILENAME)).unwrap();
                let rep = run_doctor(
                    DoctorOptions {
                        config_override: Some(cfg),
                        port_override: Some(port),
                        ..DoctorOptions::default()
                    },
                    DoctorEnv::from_raw_values(None, None),
                )
                .await
                .unwrap();
                assert!(!rep.ok, "{case}: doctor must fail");
                assert_eq!(
                    rep.checks
                        .iter()
                        .find(|c| c.name == "auth_permissions")
                        .unwrap()
                        .status,
                    CheckStatus::Fail
                );
                assert_eq!(
                    rep.checks
                        .iter()
                        .find(|c| c.name == "auth_codex")
                        .unwrap()
                        .status,
                    CheckStatus::Fail
                );
            }
            assert_eq!(fs::read_to_string(&victim).unwrap(), "keep-victim");
        }

        handle.abort();
    }
}
