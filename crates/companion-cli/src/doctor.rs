use std::fmt::Write as _;
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::PathBuf;
use std::time::Duration;

use companion_core::protocol::sanitize_health_token;
use companion_core::{
    Config, DEFAULT_LISTEN_PORT, HealthResponse, PROTOCOL_VERSION, ProxyHealthSummary,
    is_placeholder_api_key, sanitize_base_url,
};
use serde::{Deserialize, Serialize};

use crate::{CliError, MAX_CONFIG_FILE_BYTES, resolve_cli_config_path};

pub const GEMINI_API_KEY_ENV: &str = "GEMINI_API_KEY";
pub const GOOGLE_GEMINI_BASE_URL_ENV: &str = "GOOGLE_GEMINI_BASE_URL";
pub const MAX_HEALTH_BODY_BYTES: usize = 64 * 1024;
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthEvaluation {
    pub status_ok: bool,
    pub version_skew: bool,
    pub protocol_skew: bool,
    pub health_message: String,
    pub version_message: String,
}

pub fn evaluate_health_response(
    health: &HealthResponse,
    expected_version: &str,
    expected_protocol: u32,
) -> HealthEvaluation {
    let status_ok = health.status == "ok";
    let version_skew = health.version != expected_version;
    let protocol_skew = health.protocol_version != expected_protocol;

    let safe_status = sanitize_health_token(&health.status);
    let safe_version = sanitize_health_token(&health.version);

    let health_message = if status_ok {
        format!(
            "proxy /health responded ok (version {safe_version}, protocol {})",
            health.protocol_version
        )
    } else {
        format!("proxy /health reported non-ok status '{safe_status}'")
    };

    let version_message = match (version_skew, protocol_skew) {
        (false, false) => format!(
            "binary v{expected_version} matches proxy v{safe_version} (protocol v{expected_protocol})"
        ),
        (true, true) => format!(
            "version skew and protocol skew: binary v{expected_version} (protocol v{expected_protocol}) vs proxy v{safe_version} (protocol v{})",
            health.protocol_version
        ),
        (true, false) => {
            format!("version skew: binary v{expected_version} vs proxy v{safe_version}")
        }
        (false, true) => format!(
            "protocol skew: expected protocol v{expected_protocol} vs proxy protocol v{}",
            health.protocol_version
        ),
    };

    HealthEvaluation {
        status_ok,
        version_skew,
        protocol_skew,
        health_message,
        version_message,
    }
}

fn sanitize_transport_error(err: &reqwest::Error) -> String {
    if err.is_timeout() {
        "proxy /health request timed out".to_string()
    } else if err.is_connect() {
        "proxy /health unreachable (connection failed)".to_string()
    } else if err.is_redirect() {
        "proxy /health redirect rejected".to_string()
    } else {
        "proxy /health request failed".to_string()
    }
}

async fn probe_proxy_health(
    port: u16,
    timeout: Duration,
    connect_timeout: Duration,
) -> Result<HealthResponse, String> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        .connect_timeout(connect_timeout)
        .user_agent(concat!("agy-companion/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|_| "failed to build loopback HTTP probe client".to_string())?;

    let url = format!("http://127.0.0.1:{port}/health");
    let mut resp = client
        .get(&url)
        .send()
        .await
        .map_err(|err| sanitize_transport_error(&err))?;

    if resp.status() != reqwest::StatusCode::OK {
        return Err(format!(
            "proxy /health returned HTTP {}",
            resp.status().as_u16()
        ));
    }

    let mut body = Vec::new();
    let mut total_bytes: usize = 0;
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|err| sanitize_transport_error(&err))?
    {
        total_bytes = total_bytes.saturating_add(chunk.len());
        if total_bytes > MAX_HEALTH_BODY_BYTES {
            return Err(format!(
                "proxy /health response exceeded {MAX_HEALTH_BODY_BYTES}-byte size limit"
            ));
        }
        body.extend_from_slice(&chunk);
    }

    serde_json::from_slice::<HealthResponse>(&body)
        .map_err(|_| "proxy /health returned invalid JSON".to_string())
}

pub async fn run_doctor(options: DoctorOptions, env: DoctorEnv) -> Result<DoctorReport, CliError> {
    if options.port_override == Some(0) {
        return Err(CliError::InvalidPort(0));
    }

    let mut checks = Vec::with_capacity(6);
    let mut loaded_config: Option<Config> = None;

    let config_path_display = match resolve_cli_config_path(options.config_override.as_deref()) {
        Ok(config_path) => {
            let display = config_path.display().to_string();
            let metadata_path = crate::normalized_metadata_path(&config_path);
            match fs::symlink_metadata(&metadata_path) {
                Err(err) if err.kind() == ErrorKind::NotFound => {
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
                Err(err) => {
                    checks.push(DoctorCheck {
                        name: "config_permissions".to_string(),
                        status: CheckStatus::Fail,
                        critical: true,
                        message: format!("unable to inspect config file metadata: {}", err.kind()),
                    });
                    checks.push(DoctorCheck {
                        name: "config".to_string(),
                        status: CheckStatus::Fail,
                        critical: true,
                        message: "config file metadata unreadable".to_string(),
                    });
                }
                Ok(meta) => {
                    if meta.file_type().is_symlink() {
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
                    } else if !meta.file_type().is_file() {
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
                    } else if meta.len() > MAX_CONFIG_FILE_BYTES {
                        checks.push(DoctorCheck {
                            name: "config_permissions".to_string(),
                            status: CheckStatus::Fail,
                            critical: true,
                            message: format!(
                                "config file exceeds maximum allowed size ({MAX_CONFIG_FILE_BYTES} bytes)"
                            ),
                        });
                        checks.push(DoctorCheck {
                            name: "config".to_string(),
                            status: CheckStatus::Fail,
                            critical: true,
                            message: "refused to load oversized config file".to_string(),
                        });
                    } else {
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

    let target_port = options
        .port_override
        .or_else(|| loaded_config.as_ref().map(|c| c.proxy.port.get()))
        .unwrap_or(DEFAULT_LISTEN_PORT);

    let (proxy_summary, version_skew, protocol_skew) = match probe_proxy_health(
        target_port,
        options.health_timeout,
        options.health_connect_timeout,
    )
    .await
    {
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

    let config_has_api_key = loaded_config
        .as_ref()
        .map(|cfg| cfg.redacted().providers.gemini.api_key_configured)
        .unwrap_or(false);

    let gemini_api_key_set = matches!(
        env.gemini_api_key,
        EnvKeyState::Set | EnvKeyState::SetInvalidUtf8
    );
    let (api_key_status, api_key_message) = match env.gemini_api_key {
        EnvKeyState::Set => (CheckStatus::Pass, "GEMINI_API_KEY is set".to_string()),
        EnvKeyState::SetInvalidUtf8 => (
            CheckStatus::Warn,
            "GEMINI_API_KEY is set (invalid UTF-8)".to_string(),
        ),
        EnvKeyState::Placeholder if config_has_api_key => (
            CheckStatus::Pass,
            "GEMINI_API_KEY is set to the local proxy placeholder (using API key saved in config file)"
                .to_string(),
        ),
        EnvKeyState::Placeholder => (
            CheckStatus::Warn,
            "GEMINI_API_KEY is set to the local proxy placeholder and no API key is saved in config file"
                .to_string(),
        ),
        EnvKeyState::Unset if config_has_api_key => (
            CheckStatus::Pass,
            "GEMINI_API_KEY is unset (using API key saved in config file)".to_string(),
        ),
        EnvKeyState::Unset => (
            CheckStatus::Warn,
            "GEMINI_API_KEY is unset and no API key is saved in config file".to_string(),
        ),
    };
    checks.push(DoctorCheck {
        name: "env_gemini_api_key".to_string(),
        status: api_key_status,
        critical: false,
        message: api_key_message,
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
    use std::time::Duration;

    use axum::Router;
    use axum::http::{HeaderValue, StatusCode, header};
    use axum::response::IntoResponse;
    use axum::routing::get;
    use companion_core::{ApiKey, Config, HealthResponse, PROTOCOL_VERSION};
    use tempfile::TempDir;
    use tokio::net::TcpListener;

    use super::{
        CheckStatus, DoctorEnv, DoctorOptions, EnvKeyState, EnvUrlState, MAX_HEALTH_BODY_BYTES,
        evaluate_health_response, format_doctor_human, format_doctor_json, run_doctor,
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
            },
            env!("CARGO_PKG_VERSION"),
            PROTOCOL_VERSION,
        );
        assert!(healthy.status_ok);
        assert!(!healthy.version_skew);
        assert!(!healthy.protocol_skew);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn doctor_health_probe_rejects_oversized_redirect_non200_and_hanging_server() {
        let temp = TempDir::new().expect("tempdir");
        let missing_cfg = temp.path().join("missing.json");

        // 1. Oversized (> 64 KiB) body
        let oversized_router = Router::new().route(
            "/health",
            get(|| async { "x".repeat(MAX_HEALTH_BODY_BYTES + 1024) }),
        );
        let (port, h) = spawn_mock_health_router(oversized_router).await;
        let rep = run_doctor(
            DoctorOptions {
                config_override: Some(missing_cfg.clone()),
                port_override: Some(port),
                ..DoctorOptions::default()
            },
            DoctorEnv::from_raw_values(None, None),
        )
        .await
        .expect("oversized report");
        h.abort();
        assert!(!rep.ok);
        assert!(
            rep.proxy
                .error
                .as_deref()
                .unwrap_or("")
                .to_lowercase()
                .contains("limit")
        );

        // 2. 302 redirect (must NOT be followed)
        let redirect_router = Router::new().route(
            "/health",
            get(|| async {
                (
                    StatusCode::FOUND,
                    [(
                        header::LOCATION,
                        HeaderValue::from_static("http://169.254.169.254/latest/meta-data/"),
                    )],
                    "",
                )
                    .into_response()
            }),
        );
        let (port, h) = spawn_mock_health_router(redirect_router).await;
        let rep = run_doctor(
            DoctorOptions {
                config_override: Some(missing_cfg.clone()),
                port_override: Some(port),
                ..DoctorOptions::default()
            },
            DoctorEnv::from_raw_values(None, None),
        )
        .await
        .expect("redirect report");
        h.abort();
        assert!(!rep.ok);
        assert!(rep.proxy.error.as_deref().unwrap_or("").contains("302"));

        // 3. 500 Internal Server Error
        let err500_router = Router::new().route(
            "/health",
            get(|| async {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "secret-body-should-not-echo",
                )
            }),
        );
        let (port, h) = spawn_mock_health_router(err500_router).await;
        let rep = run_doctor(
            DoctorOptions {
                config_override: Some(missing_cfg.clone()),
                port_override: Some(port),
                ..DoctorOptions::default()
            },
            DoctorEnv::from_raw_values(None, None),
        )
        .await
        .expect("500 report");
        h.abort();
        assert!(!rep.ok);
        let json = format_doctor_json(&rep).expect("json");
        assert!(json.contains("500"));
        assert!(!json.contains("secret-body-should-not-echo"));

        // 4. Slow / hanging server -> bounded timeout
        let slow_router = Router::new().route(
            "/health",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(5)).await;
                "too late"
            }),
        );
        let (port, h) = spawn_mock_health_router(slow_router).await;
        let rep = run_doctor(
            DoctorOptions {
                config_override: Some(missing_cfg.clone()),
                port_override: Some(port),
                health_timeout: Duration::from_millis(250),
                health_connect_timeout: Duration::from_millis(250),
                ..DoctorOptions::default()
            },
            DoctorEnv::from_raw_values(None, None),
        )
        .await
        .expect("timeout report");
        h.abort();
        assert!(!rep.ok);
        assert!(
            rep.proxy
                .error
                .as_deref()
                .unwrap_or("")
                .to_lowercase()
                .contains("timed out")
        );

        // 5. Invalid JSON -> fail without echoing body
        let bad_json_router = Router::new().route(
            "/health",
            get(|| async { "not-json-body-AIzaSyNeverEchoHealthBody" }),
        );
        let (port, h) = spawn_mock_health_router(bad_json_router).await;
        let rep = run_doctor(
            DoctorOptions {
                config_override: Some(missing_cfg),
                port_override: Some(port),
                ..DoctorOptions::default()
            },
            DoctorEnv::from_raw_values(None, None),
        )
        .await
        .expect("bad json report");
        h.abort();
        assert!(!rep.ok);
        let json = format_doctor_json(&rep).expect("json");
        assert!(!json.contains("AIzaSyNeverEchoHealthBody"));
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
    async fn doctor_fails_on_non_header_safe_api_key_in_config() {
        let (port, handle) = spawn_mock_health_router(healthy_router()).await;
        let temp = TempDir::new().expect("tempdir");

        for (label, bad_key) in [
            ("internal space", "AIzaSy Space In Key 1234"),
            ("nbsp", "AIzaSy\u{00A0}NbspKey1234"),
            ("emoji", "AIzaSy\u{1F511}EmojiKey1234"),
        ] {
            let cfg_path = temp
                .path()
                .join(format!("{}.json", label.replace(' ', "_")));
            let raw = serde_json::json!({
                "schema_version": 1,
                "proxy": {
                    "host": "127.0.0.1",
                    "port": port
                },
                "providers": {
                    "gemini": {
                        "base_url": "https://generativelanguage.googleapis.com",
                        "api_key": bad_key,
                        "default_model": "gemini-4-hydrogen",
                        "timeout_secs": 900
                    }
                }
            });
            fs::write(&cfg_path, raw.to_string()).expect("write config");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&cfg_path, fs::Permissions::from_mode(0o600)).expect("chmod");
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
                "{label}: doctor must fail on non-header-safe api_key"
            );

            let cfg_check = rep
                .checks
                .iter()
                .find(|c| c.name == "config")
                .expect("config check");
            assert_eq!(
                cfg_check.status,
                CheckStatus::Fail,
                "{label}: config check must fail, got {cfg_check:?}"
            );

            let json = format_doctor_json(&rep).expect("format json");
            let human = format_doctor_human(&rep);
            assert!(!json.contains("AIzaSy"), "{label}: JSON leaked secret");
            assert!(!human.contains("AIzaSy"), "{label}: human leaked secret");
        }

        handle.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn doctor_quarantines_placeholder_gemini_api_key_in_env_and_config() {
        let (port, handle) = spawn_mock_health_router(healthy_router()).await;
        let temp = TempDir::new().expect("tempdir");

        // 1. Placeholder in env (exact & padded) with no config key -> WARN, gemini_api_key_set == false.
        let cfg_empty = temp.path().join("empty.json");
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

        // 2. Placeholder in BOTH env and legacy config.json -> still WARN, not PASS.
        let cfg_legacy = temp.path().join("legacy_placeholder.json");
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

        // 3. Placeholder in env with a REAL saved key in config.json -> PASS, gemini_api_key_set == false, no leak.
        let cfg_real = temp.path().join("real_key.json");
        let mut real = Config::default();
        real.providers.gemini.api_key =
            Some(ApiKey::parse_strict("AIzaSyRealSavedKey1234").expect("valid key"));
        real.save_secure(&cfg_real).expect("save real");
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
        assert!(check_real.message.contains("config file"));
        let json = format_doctor_json(&rep_real).expect("json");
        let human = format_doctor_human(&rep_real);
        assert!(!json.contains("AIzaSyRealSavedKey1234"));
        assert!(!human.contains("AIzaSyRealSavedKey1234"));

        handle.abort();
    }
}
