use std::io::ErrorKind;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::extract::{RawQuery, State};
use axum::http::header::{CACHE_CONTROL, CONNECTION, CONTENT_TYPE};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{SecondsFormat, Utc};
use companion_core::{
    AUTH_SCHEMA_VERSION, AuthStore, CodexAuthMode, CodexCredentials, CodexRefreshLock,
    SecretString, validate_upstream_url,
};
use ring::digest;
use serde::Deserialize;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, warn};
use url::{Origin, Url, form_urlencoded};

use crate::gemini::url::is_loopback_url;
use crate::{BoxFuture, ProviderError};

pub const DEFAULT_CODEX_OAUTH_ISSUER: &str = "https://auth.openai.com";
pub const CODEX_OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const CODEX_CALLBACK_PORT: u16 = 1455;
pub const CODEX_CALLBACK_PATH: &str = "/auth/callback";
pub const CODEX_OAUTH_SCOPES: &str = "openid profile email offline_access";
pub const CODEX_OAUTH_ORIGINATOR: &str = "codex_cli_rs";
pub const DEFAULT_CODEX_LOGIN_TTL: Duration = Duration::from_secs(5 * 60);
pub const DEFAULT_CODEX_REFRESH_MARGIN: Duration = Duration::from_secs(5 * 60);
pub const DEFAULT_CODEX_OAUTH_TIMEOUT: Duration = Duration::from_secs(30);

const MAX_OAUTH_RESPONSE_BYTES: usize = 128 * 1024;
const MAX_OAUTH_ERROR_BODY_BYTES: usize = 16 * 1024;
const MAX_JWT_PAYLOAD_BYTES: usize = 64 * 1024;
const MAX_MANUAL_CALLBACK_INPUT_BYTES: usize = 8 * 1024;
const MAX_CONNECT_TIMEOUT_SECS: u64 = 30;
const MAX_ACCOUNT_ID_LEN: usize = 128;
const MAX_PLAN_TYPE_LEN: usize = 32;
const MAX_EMAIL_LEN: usize = 254;
const CANCEL_TEARDOWN_GRACE: Duration = Duration::from_millis(50);
const CALLBACK_DRAIN_GRACE: Duration = Duration::from_secs(2);

const CODEX_SESSION_EXPIRED_MSG: &str = "OpenAI Codex session expired — sign in again";
const CODEX_NOT_SIGNED_IN_MSG: &str =
    "OpenAI Codex is not signed in — sign in with ChatGPT in Antigravity Companion settings";
const CODEX_NO_ACTIVE_LOGIN_MSG: &str =
    "No active OpenAI Codex sign-in session — start sign-in again";

const HTML_SIGNED_IN: &str = "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>Antigravity Companion</title></head><body><h1>Signed in — return to Antigravity</h1><p>Authentication succeeded. You may close this browser tab.</p></body></html>";
const HTML_INVALID_STATE: &str = "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>Antigravity Companion</title></head><body><h1>Invalid or expired sign-in state — return to Antigravity</h1><p>Please restart sign-in from Antigravity Companion settings.</p></body></html>";
const HTML_OAUTH_DENIED: &str = "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>Antigravity Companion</title></head><body><h1>Sign-in was canceled or denied — return to Antigravity</h1><p>No existing credentials were modified.</p></body></html>";
const HTML_EXCHANGE_FAILED: &str = "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>Antigravity Companion</title></head><body><h1>Sign-in failed — return to Antigravity and try again</h1><p>Token exchange could not be completed.</p></body></html>";

const X_CONTENT_TYPE_OPTIONS: HeaderName = HeaderName::from_static("x-content-type-options");

/// Authentication seam for acquiring valid OpenAI Codex credentials.
pub trait CodexAuth: Send + Sync {
    fn valid_credentials(
        &self,
        force_refresh: bool,
    ) -> BoxFuture<'_, Result<CodexCredentials, ProviderError>>;
}

/// Dependency-injection configuration for [`CodexAuthManager`].
#[derive(Debug, Clone)]
pub struct CodexAuthOptions {
    pub oauth_issuer: String,
    pub callback_bind_port: u16,
    pub login_ttl: Duration,
    pub refresh_margin: Duration,
    pub request_timeout: Duration,
}

impl Default for CodexAuthOptions {
    fn default() -> Self {
        Self {
            oauth_issuer: DEFAULT_CODEX_OAUTH_ISSUER.to_string(),
            callback_bind_port: CODEX_CALLBACK_PORT,
            login_ttl: DEFAULT_CODEX_LOGIN_TTL,
            refresh_margin: DEFAULT_CODEX_REFRESH_MARGIN,
            request_timeout: DEFAULT_CODEX_OAUTH_TIMEOUT,
        }
    }
}

/// Result returned by [`CodexAuthManager::start_login`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexLoginStart {
    pub authorize_url: String,
    pub callback_listening: bool,
    pub manual_fallback: bool,
    pub callback_port: Option<u16>,
}

struct PendingLogin {
    session_id: u64,
    code_verifier: SecretString,
    state: SecretString,
    redirect_uri: String,
    expires_at: Instant,
    exchanging: bool,
    cancel_tx: Option<oneshot::Sender<()>>,
    task_handle: Option<JoinHandle<()>>,
}

struct CodexAuthInner {
    auth_store: Arc<dyn AuthStore>,
    options: CodexAuthOptions,
    authorize_endpoint: String,
    token_url: Url,
    client: reqwest::Client,
    refresh_mutex: Mutex<()>,
    login_op_mutex: Mutex<()>,
    pending: Mutex<Option<PendingLogin>>,
    next_session_id: AtomicU64,
    disconnect_epoch: AtomicU64,
}

impl Drop for CodexAuthInner {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.pending.try_lock()
            && let Some(mut pending) = guard.take()
        {
            if let Some(tx) = pending.cancel_tx.take() {
                let _ = tx.send(());
            }
            if let Some(handle) = pending.task_handle.take() {
                handle.abort();
            }
        }
    }
}

/// Manages OpenAI Codex OAuth 2.0 + PKCE S256 login flows, bounded loopback callbacks,
/// manual paste fallback, and cross-process locked token refreshes.
#[derive(Clone)]
pub struct CodexAuthManager {
    inner: Arc<CodexAuthInner>,
}

impl CodexAuthManager {
    pub fn new(auth_store: Arc<dyn AuthStore>) -> Result<Self, ProviderError> {
        Self::with_options(auth_store, CodexAuthOptions::default())
    }

    pub fn with_options(
        auth_store: Arc<dyn AuthStore>,
        options: CodexAuthOptions,
    ) -> Result<Self, ProviderError> {
        if options.login_ttl.is_zero() {
            return Err(ProviderError::InvalidConfig(
                "codex OAuth login_ttl must be greater than 0".to_string(),
            ));
        }
        if options.request_timeout.is_zero() {
            return Err(ProviderError::InvalidConfig(
                "codex OAuth request_timeout must be greater than 0".to_string(),
            ));
        }

        let (issuer_url, authorize_endpoint, token_url) =
            normalize_and_build_issuer_endpoints(&options.oauth_issuer)?;

        let connect_timeout = options
            .request_timeout
            .min(Duration::from_secs(MAX_CONNECT_TIMEOUT_SECS));

        let mut builder = reqwest::Client::builder()
            .timeout(options.request_timeout)
            .connect_timeout(connect_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("agy-companion/", env!("CARGO_PKG_VERSION")));

        // Bypass ambient HTTP_PROXY/HTTPS_PROXY strictly for loopback test issuers so
        // hostile proxy envs never hijack local tests while remote HTTPS honors proxies.
        if is_loopback_url(&issuer_url) {
            builder = builder.no_proxy();
        }

        let client = builder.build().map_err(|err| {
            ProviderError::InvalidConfig(format!(
                "failed to initialize OAuth HTTP client: {}",
                err.without_url()
            ))
        })?;

        Ok(Self {
            inner: Arc::new(CodexAuthInner {
                auth_store,
                options,
                authorize_endpoint,
                token_url,
                client,
                refresh_mutex: Mutex::new(()),
                login_op_mutex: Mutex::new(()),
                pending: Mutex::new(None),
                next_session_id: AtomicU64::new(1),
                disconnect_epoch: AtomicU64::new(0),
            }),
        })
    }

    /// Starts (or atomically replaces) a single bounded OAuth 2.0 + PKCE S256 login session.
    ///
    /// Binds strictly to `127.0.0.1:<callback_bind_port>` (S1). If that port is already in use,
    /// returns `Ok(CodexLoginStart { callback_listening: false, manual_fallback: true, .. })`
    /// while keeping the PKCE `state` and `code_verifier` alive for [`Self::complete_login_manual`].
    pub async fn start_login(&self) -> Result<CodexLoginStart, ProviderError> {
        let _op_guard = self.inner.login_op_mutex.lock().await;
        self.teardown_pending_login().await;

        let (code_verifier, code_challenge) = generate_pkce_pair()?;
        let state = generate_random_base64url_32()?;
        let session_id = self.inner.next_session_id.fetch_add(1, Ordering::Relaxed);
        let ttl = self.inner.options.login_ttl;
        let bind_port = self.inner.options.callback_bind_port;

        let bind_addr = SocketAddr::from((Ipv4Addr::LOCALHOST, bind_port));
        let listener_opt = match TcpListener::bind(bind_addr).await {
            Ok(listener) => {
                let local = listener.local_addr().map_err(|err| {
                    ProviderError::OAuth(format!("failed to inspect callback listener: {err}"))
                })?;
                if local.ip() != Ipv4Addr::LOCALHOST {
                    return Err(ProviderError::OAuth(
                        "OAuth callback listener bound non-loopback interface".to_string(),
                    ));
                }
                Some((listener, local.port()))
            }
            Err(err) if err.kind() == ErrorKind::AddrInUse => {
                debug!(
                    port = bind_port,
                    "Codex OAuth callback port already in use; falling back to manual paste mode"
                );
                None
            }
            Err(err) => {
                return Err(ProviderError::OAuth(format!(
                    "failed to bind loopback OAuth callback listener on 127.0.0.1:{bind_port}: {err}"
                )));
            }
        };

        let (redirect_uri, callback_listening, manual_fallback, callback_port) = match &listener_opt
        {
            Some((_, actual_port)) => (
                format!("http://localhost:{actual_port}{CODEX_CALLBACK_PATH}"),
                true,
                false,
                Some(*actual_port),
            ),
            None => {
                let fallback_port = if bind_port == 0 {
                    CODEX_CALLBACK_PORT
                } else {
                    bind_port
                };
                (
                    format!("http://localhost:{fallback_port}{CODEX_CALLBACK_PATH}"),
                    false,
                    true,
                    None,
                )
            }
        };

        let authorize_url = build_authorize_url(
            &self.inner.authorize_endpoint,
            &redirect_uri,
            &code_challenge,
            state.expose(),
        );

        let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
        let weak_inner = Arc::downgrade(&self.inner);

        let task_handle = if let Some((listener, _)) = listener_opt {
            let (done_tx, done_rx) = oneshot::channel::<()>();
            let callback_state = CallbackRouteState {
                inner: weak_inner.clone(),
                session_id,
                done_tx: Arc::new(Mutex::new(Some(done_tx))),
            };
            let router = Router::new()
                .route(CODEX_CALLBACK_PATH, get(handle_oauth_callback))
                .with_state(callback_state);

            tokio::spawn(async move {
                run_callback_listener(
                    listener, router, weak_inner, session_id, ttl, cancel_rx, done_rx,
                )
                .await;
            })
        } else {
            tokio::spawn(async move {
                tokio::select! {
                    _ = cancel_rx => {}
                    _ = tokio::time::sleep(ttl) => {
                        clear_pending_if_matching(&weak_inner, session_id).await;
                    }
                }
            })
        };

        let expires_at = Instant::now() + ttl;
        let mut guard = self.inner.pending.lock().await;
        *guard = Some(PendingLogin {
            session_id,
            code_verifier,
            state,
            redirect_uri,
            expires_at,
            exchanging: false,
            cancel_tx: Some(cancel_tx),
            task_handle: Some(task_handle),
        });
        drop(guard);

        Ok(CodexLoginStart {
            authorize_url,
            callback_listening,
            manual_fallback,
            callback_port,
        })
    }

    /// Aborts any active pending login session and deterministically releases its loopback port.
    pub async fn cancel_login(&self) {
        let _op_guard = self.inner.login_op_mutex.lock().await;
        self.teardown_pending_login().await;
    }

    /// Returns `true` if a non-expired OAuth login session is currently awaiting completion.
    pub async fn is_login_pending(&self) -> bool {
        let expired = {
            let mut guard = self.inner.pending.lock().await;
            match guard.as_ref() {
                Some(pending) if Instant::now() < pending.expires_at => return true,
                Some(_) => guard.take(),
                None => return false,
            }
        };
        if let Some(pending) = expired {
            finish_pending_teardown(pending).await;
        }
        false
    }

    /// Completes an active pending login from a manually pasted callback URL, query string,
    /// `code#state` shorthand, or raw authorization code.
    ///
    /// Invalid or wrong-state input fails closed without destroying the active pending session
    /// or calling `/oauth/token`.
    pub async fn complete_login_manual(
        &self,
        input: &str,
    ) -> Result<CodexCredentials, ProviderError> {
        let parsed = parse_manual_callback_input(input)?;

        // Phase 1: Validate against active non-expired session and mark exchanging atomically.
        let (session_id, code, code_verifier, redirect_uri, start_epoch) = {
            let mut guard = self.inner.pending.lock().await;
            match guard.as_mut() {
                Some(pending) if Instant::now() < pending.expires_at => {
                    if pending.exchanging {
                        return Err(ProviderError::OAuth(
                            "OpenAI Codex sign-in token exchange is already in progress"
                                .to_string(),
                        ));
                    }
                    let expected_state_matches = match parsed.state.as_deref() {
                        Some(given_state) => constant_time_eq(
                            given_state.as_bytes(),
                            pending.state.expose().as_bytes(),
                        ),
                        None => !parsed.requires_state,
                    };

                    if let Some(ref oauth_err) = parsed.error {
                        // If caller pasted a state alongside error, ensure state matches first so
                        // unrelated pastes never interfere with an active login.
                        if parsed.state.is_some() && !expected_state_matches {
                            return Err(ProviderError::OAuth(
                                "OAuth state mismatch — paste the redirect URL from the current sign-in attempt"
                                    .to_string(),
                            ));
                        }
                        debug!(
                            oauth_error_present = !oauth_err.is_empty(),
                            "Manual Codex OAuth callback contained error"
                        );
                        return Err(ProviderError::OAuth(
                            "OpenAI sign-in was canceled or denied".to_string(),
                        ));
                    }

                    if !expected_state_matches {
                        return Err(ProviderError::OAuth(
                            "OAuth state mismatch — paste the redirect URL from the current sign-in attempt"
                                .to_string(),
                        ));
                    }

                    let code = parsed.code.ok_or_else(|| {
                        ProviderError::OAuth(
                            "missing authorization code in pasted callback input".to_string(),
                        )
                    })?;

                    pending.exchanging = true;
                    let start_epoch = self.inner.disconnect_epoch.load(Ordering::SeqCst);
                    (
                        pending.session_id,
                        code,
                        pending.code_verifier.clone(),
                        pending.redirect_uri.clone(),
                        start_epoch,
                    )
                }
                Some(_) => {
                    let expired = guard.take();
                    drop(guard);
                    if let Some(p) = expired {
                        finish_pending_teardown(p).await;
                    }
                    return Err(ProviderError::OAuth(CODEX_NO_ACTIVE_LOGIN_MSG.to_string()));
                }
                None => {
                    return Err(ProviderError::OAuth(CODEX_NO_ACTIVE_LOGIN_MSG.to_string()));
                }
            }
        };

        // Phase 2: Exchange code without holding login_op_mutex or pending across network I/O.
        match self
            .exchange_authorization_code(&code, &redirect_uri, code_verifier.expose())
            .await
        {
            Ok(creds) => {
                let _op_guard = self.inner.login_op_mutex.lock().await;
                let mut guard = self.inner.pending.lock().await;
                let still_active = self.inner.disconnect_epoch.load(Ordering::SeqCst)
                    == start_epoch
                    && guard.as_ref().is_some_and(|p| p.session_id == session_id);
                if !still_active {
                    drop(guard);
                    drop(_op_guard);
                    return Err(ProviderError::OAuth(
                        "OpenAI Codex sign-in session was canceled or superseded before completion"
                            .to_string(),
                    ));
                }

                let save_result = self
                    .inner
                    .auth_store
                    .save_codex(&creds)
                    .map_err(|err| ProviderError::AuthStore(err.to_string()));
                let taken = guard.take();
                drop(guard);
                if let Some(pending) = taken {
                    finish_pending_teardown(pending).await;
                }
                drop(_op_guard);
                save_result?;
                Ok(creds)
            }
            Err(err) => {
                let _op_guard = self.inner.login_op_mutex.lock().await;
                let taken = {
                    let mut guard = self.inner.pending.lock().await;
                    if guard.as_ref().is_some_and(|p| p.session_id == session_id) {
                        guard.take()
                    } else {
                        None
                    }
                };
                if let Some(pending) = taken {
                    finish_pending_teardown(pending).await;
                }
                drop(_op_guard);
                Err(err)
            }
        }
    }

    /// Cancels any pending login listener and clears persisted `auth/codex.json` credentials.
    pub async fn disconnect(&self) -> Result<(), ProviderError> {
        let _op_guard = self.inner.login_op_mutex.lock().await;
        self.inner.disconnect_epoch.fetch_add(1, Ordering::SeqCst);
        self.teardown_pending_login().await;
        self.inner
            .auth_store
            .clear_codex()
            .map_err(|err| ProviderError::AuthStore(err.to_string()))
    }

    /// Returns valid OpenAI Codex credentials, proactively refreshing within the configured
    /// margin or when `force_refresh` is `true` (e.g. after an upstream `401`).
    pub async fn valid_credentials(
        &self,
        force_refresh: bool,
    ) -> Result<CodexCredentials, ProviderError> {
        let initial = self
            .inner
            .auth_store
            .load_codex()
            .map_err(|err| ProviderError::AuthStore(err.to_string()))?
            .ok_or_else(|| ProviderError::NotAuthenticated(CODEX_NOT_SIGNED_IN_MSG.to_string()))?;

        let margin_ms =
            u64::try_from(self.inner.options.refresh_margin.as_millis()).unwrap_or(u64::MAX);
        let now_ms = current_utc_epoch_ms();
        if !force_refresh && now_ms.saturating_add(margin_ms) < initial.expires_at_ms {
            return Ok(initial);
        }

        let stale_access_token = initial.access_token;

        // Serialize in-process first so a concurrent stampede never exhausts the blocking pool.
        let _in_proc_guard = self.inner.refresh_mutex.lock().await;

        // Acquire cross-process `.codex-refresh.lock` off the Tokio worker thread when backed by disk.
        let _cross_proc_lock = if let Some(auth_dir) =
            self.inner.auth_store.auth_dir().map(Path::to_path_buf)
        {
            let lock = tokio::task::spawn_blocking(move || CodexRefreshLock::acquire(&auth_dir))
                .await
                .map_err(|err| {
                    ProviderError::AuthStore(format!("refresh lock task failed: {err}"))
                })?
                .map_err(|err| ProviderError::AuthStore(err.to_string()))?;
            Some(lock)
        } else {
            None
        };

        // Double-checked reload under lock: if another task/process already refreshed, reuse it.
        let current = self
            .inner
            .auth_store
            .load_codex()
            .map_err(|err| ProviderError::AuthStore(err.to_string()))?
            .ok_or_else(|| ProviderError::NotAuthenticated(CODEX_NOT_SIGNED_IN_MSG.to_string()))?;

        let now_ms = current_utc_epoch_ms();
        let fresh_enough = now_ms.saturating_add(margin_ms) < current.expires_at_ms;
        if fresh_enough && (!force_refresh || current.access_token != stale_access_token) {
            return Ok(current);
        }

        let start_epoch = self.inner.disconnect_epoch.load(Ordering::SeqCst);
        let refreshed = self.refresh_access_token(&current).await?;

        // Strict acyclic lock order: refresh_mutex -> .codex-refresh.lock -> login_op_mutex.
        // Never hold login_op_mutex across network I/O; re-verify disconnect_epoch and on-disk
        // credential identity before committing so disconnect() or an account switch cannot be
        // resurrected or clobbered by an in-flight refresh.
        let _op_guard = self.inner.login_op_mutex.lock().await;
        if self.inner.disconnect_epoch.load(Ordering::SeqCst) != start_epoch {
            return Err(ProviderError::NotAuthenticated(
                CODEX_NOT_SIGNED_IN_MSG.to_string(),
            ));
        }
        let latest = self
            .inner
            .auth_store
            .load_codex()
            .map_err(|err| ProviderError::AuthStore(err.to_string()))?;
        let still_same_credentials = latest.as_ref().is_some_and(|c| {
            c.account_id == current.account_id
                && c.refresh_token == current.refresh_token
                && c.access_token == current.access_token
        });
        if !still_same_credentials {
            return Err(ProviderError::NotAuthenticated(
                CODEX_NOT_SIGNED_IN_MSG.to_string(),
            ));
        }

        self.inner
            .auth_store
            .save_codex(&refreshed)
            .map_err(|err| ProviderError::AuthStore(err.to_string()))?;
        Ok(refreshed)
    }

    async fn teardown_pending_login(&self) {
        let taken = {
            let mut guard = self.inner.pending.lock().await;
            guard.take()
        };
        if let Some(pending) = taken {
            finish_pending_teardown(pending).await;
        }
    }

    async fn exchange_authorization_code(
        &self,
        code: &str,
        redirect_uri: &str,
        code_verifier: &str,
    ) -> Result<CodexCredentials, ProviderError> {
        let form_body = form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "authorization_code")
            .append_pair("client_id", CODEX_OAUTH_CLIENT_ID)
            .append_pair("code", code)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("code_verifier", code_verifier)
            .finish();

        let resp = self
            .inner
            .client
            .post(self.inner.token_url.clone())
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(form_body)
            .send()
            .await
            .map_err(|err| {
                ProviderError::OAuth(format!(
                    "OAuth token exchange request failed: {}",
                    err.without_url()
                ))
            })?;

        let status = resp.status();
        if !status.is_success() {
            let _ = read_bounded_body(resp, MAX_OAUTH_ERROR_BODY_BYTES).await;
            return Err(ProviderError::OAuth(format!(
                "OAuth token exchange failed with HTTP {}",
                status.as_u16()
            )));
        }

        let bytes = read_bounded_body(resp, MAX_OAUTH_RESPONSE_BYTES).await?;
        let token_resp: OAuthTokenResponse = serde_json::from_slice(&bytes).map_err(|_| {
            ProviderError::OAuth("OAuth token endpoint returned malformed JSON".to_string())
        })?;

        build_credentials_from_token_response(token_resp, None, current_utc_epoch_ms())
    }

    async fn refresh_access_token(
        &self,
        current: &CodexCredentials,
    ) -> Result<CodexCredentials, ProviderError> {
        let form_body = form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "refresh_token")
            .append_pair("client_id", CODEX_OAUTH_CLIENT_ID)
            .append_pair("refresh_token", current.refresh_token.expose())
            .finish();

        let resp = self
            .inner
            .client
            .post(self.inner.token_url.clone())
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(form_body)
            .send()
            .await
            .map_err(|err| {
                ProviderError::Upstream(format!(
                    "OpenAI OAuth token refresh request failed: {}",
                    err.without_url()
                ))
            })?;

        let status = resp.status();
        if status == StatusCode::UNAUTHORIZED {
            let _ = read_bounded_body(resp, MAX_OAUTH_ERROR_BODY_BYTES).await;
            return Err(ProviderError::AuthExpired(
                CODEX_SESSION_EXPIRED_MSG.to_string(),
            ));
        }
        if status == StatusCode::BAD_REQUEST {
            let body = read_bounded_body(resp, MAX_OAUTH_ERROR_BODY_BYTES)
                .await
                .unwrap_or_default();
            if is_refresh_auth_expired_body(&body) {
                return Err(ProviderError::AuthExpired(
                    CODEX_SESSION_EXPIRED_MSG.to_string(),
                ));
            }
            return Err(ProviderError::Upstream(format!(
                "OpenAI OAuth token refresh failed with HTTP {}",
                status.as_u16()
            )));
        }
        if !status.is_success() {
            let _ = read_bounded_body(resp, MAX_OAUTH_ERROR_BODY_BYTES).await;
            return Err(ProviderError::Upstream(format!(
                "OpenAI OAuth token refresh failed with HTTP {}",
                status.as_u16()
            )));
        }

        let bytes = read_bounded_body(resp, MAX_OAUTH_RESPONSE_BYTES).await?;
        let token_resp: OAuthTokenResponse = serde_json::from_slice(&bytes).map_err(|_| {
            ProviderError::Upstream(
                "OpenAI OAuth token refresh returned malformed JSON".to_string(),
            )
        })?;

        build_credentials_from_token_response(token_resp, Some(current), current_utc_epoch_ms())
    }
}

impl CodexAuth for CodexAuthManager {
    fn valid_credentials(
        &self,
        force_refresh: bool,
    ) -> BoxFuture<'_, Result<CodexCredentials, ProviderError>> {
        Box::pin(CodexAuthManager::valid_credentials(self, force_refresh))
    }
}

#[derive(Clone)]
struct CallbackRouteState {
    inner: Weak<CodexAuthInner>,
    session_id: u64,
    done_tx: Arc<Mutex<Option<oneshot::Sender<()>>>>,
}

impl CallbackRouteState {
    async fn signal_done(&self) {
        let tx = {
            let mut guard = self.done_tx.lock().await;
            guard.take()
        };
        if let Some(tx) = tx {
            let _ = tx.send(());
        }
    }
}

async fn run_callback_listener(
    listener: TcpListener,
    router: Router,
    weak_inner: Weak<CodexAuthInner>,
    session_id: u64,
    ttl: Duration,
    cancel_rx: oneshot::Receiver<()>,
    done_rx: oneshot::Receiver<()>,
) {
    let (graceful_tx, graceful_rx) = oneshot::channel::<()>();
    let mut graceful_tx = Some(graceful_tx);
    let mut serve_fut = Box::pin(std::future::IntoFuture::into_future(
        axum::serve(listener, router).with_graceful_shutdown(async move {
            let _ = graceful_rx.await;
        }),
    ));

    tokio::select! {
        _ = &mut serve_fut => {}
        Ok(()) = cancel_rx => {
            if let Some(tx) = graceful_tx.take() {
                let _ = tx.send(());
            }
            let _ = tokio::time::timeout(CANCEL_TEARDOWN_GRACE, &mut serve_fut).await;
        }
        _ = done_rx => {
            if let Some(tx) = graceful_tx.take() {
                let _ = tx.send(());
            }
            let _ = tokio::time::timeout(CALLBACK_DRAIN_GRACE, &mut serve_fut).await;
        }
        _ = tokio::time::sleep(ttl) => {
            clear_pending_if_matching(&weak_inner, session_id).await;
            if let Some(tx) = graceful_tx.take() {
                let _ = tx.send(());
            }
            let _ = tokio::time::timeout(CALLBACK_DRAIN_GRACE, &mut serve_fut).await;
        }
    }
}

async fn clear_pending_if_matching(weak_inner: &Weak<CodexAuthInner>, session_id: u64) {
    if let Some(inner) = weak_inner.upgrade() {
        let mut guard = inner.pending.lock().await;
        if guard.as_ref().is_some_and(|p| p.session_id == session_id) {
            guard.take();
        }
    }
}

async fn finish_pending_teardown(mut pending: PendingLogin) {
    if let Some(tx) = pending.cancel_tx.take() {
        let _ = tx.send(());
    }
    if let Some(mut handle) = pending.task_handle.take()
        && tokio::time::timeout(CANCEL_TEARDOWN_GRACE, &mut handle)
            .await
            .is_err()
    {
        handle.abort();
        let _ = handle.await;
    }
}

fn static_html_response(status: StatusCode, body: &'static str) -> Response {
    let mut resp = (status, body).into_response();
    let headers = resp.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(CONNECTION, HeaderValue::from_static("close"));
    resp
}

async fn handle_oauth_callback(
    State(route_state): State<CallbackRouteState>,
    RawQuery(raw_query): RawQuery,
) -> Response {
    let Some(inner) = route_state.inner.upgrade() else {
        return static_html_response(StatusCode::BAD_REQUEST, HTML_INVALID_STATE);
    };

    let query_str = raw_query.unwrap_or_default();
    let mut code: Option<String> = None;
    let mut state_param: Option<String> = None;
    let mut error_param: Option<String> = None;

    for (k, v) in form_urlencoded::parse(query_str.as_bytes()) {
        match k.as_ref() {
            "code" if code.is_none() => code = Some(v.into_owned()),
            "state" if state_param.is_none() => state_param = Some(v.into_owned()),
            "error" if error_param.is_none() => error_param = Some(v.into_owned()),
            _ => {}
        }
    }

    let Some(given_state) = state_param.filter(|s| !s.is_empty()) else {
        return static_html_response(StatusCode::BAD_REQUEST, HTML_INVALID_STATE);
    };

    enum CallbackClaim {
        OAuthError(Option<PendingLogin>),
        Exchange {
            code_verifier: SecretString,
            redirect_uri: String,
            start_epoch: u64,
        },
    }

    // Verify CSRF state under lock without destroying an unrelated/valid session on mismatch.
    let claim_result: Option<CallbackClaim> = {
        let mut guard = inner.pending.lock().await;
        match guard.as_mut() {
            Some(pending)
                if pending.session_id == route_state.session_id
                    && !pending.exchanging
                    && Instant::now() < pending.expires_at
                    && constant_time_eq(
                        given_state.as_bytes(),
                        pending.state.expose().as_bytes(),
                    ) =>
            {
                if error_param.is_some() {
                    Some(CallbackClaim::OAuthError(guard.take()))
                } else if code.as_deref().is_some_and(|c| !c.is_empty()) {
                    pending.exchanging = true;
                    let start_epoch = inner.disconnect_epoch.load(Ordering::SeqCst);
                    Some(CallbackClaim::Exchange {
                        code_verifier: pending.code_verifier.clone(),
                        redirect_uri: pending.redirect_uri.clone(),
                        start_epoch,
                    })
                } else {
                    None
                }
            }
            _ => None,
        }
    };

    let (code_verifier, redirect_uri, start_epoch) = match claim_result {
        None => return static_html_response(StatusCode::BAD_REQUEST, HTML_INVALID_STATE),
        Some(CallbackClaim::OAuthError(taken)) => {
            route_state.signal_done().await;
            drop(taken);
            return static_html_response(StatusCode::BAD_REQUEST, HTML_OAUTH_DENIED);
        }
        Some(CallbackClaim::Exchange {
            code_verifier,
            redirect_uri,
            start_epoch,
        }) => (code_verifier, redirect_uri, start_epoch),
    };

    let code = code.expect("checked non-empty above");
    let manager = CodexAuthManager { inner };
    match manager
        .exchange_authorization_code(&code, &redirect_uri, code_verifier.expose())
        .await
    {
        Ok(creds) => {
            let commit_outcome = {
                let _op_guard = manager.inner.login_op_mutex.lock().await;
                let mut guard = manager.inner.pending.lock().await;
                let still_active = manager.inner.disconnect_epoch.load(Ordering::SeqCst)
                    == start_epoch
                    && guard
                        .as_ref()
                        .is_some_and(|p| p.session_id == route_state.session_id);
                if !still_active {
                    None
                } else {
                    let save_res = manager.inner.auth_store.save_codex(&creds);
                    let taken = guard.take();
                    Some((save_res, taken))
                }
            };

            match commit_outcome {
                None => static_html_response(StatusCode::BAD_REQUEST, HTML_OAUTH_DENIED),
                Some((Ok(()), taken)) => {
                    route_state.signal_done().await;
                    drop(taken);
                    static_html_response(StatusCode::OK, HTML_SIGNED_IN)
                }
                Some((Err(err), taken)) => {
                    warn!(error = %err, "Failed to persist OpenAI Codex credentials after OAuth callback");
                    route_state.signal_done().await;
                    drop(taken);
                    static_html_response(StatusCode::INTERNAL_SERVER_ERROR, HTML_EXCHANGE_FAILED)
                }
            }
        }
        Err(err) => {
            warn!(error = %err, "OpenAI Codex OAuth token exchange failed on callback");
            let taken = {
                let mut guard = manager.inner.pending.lock().await;
                if guard
                    .as_ref()
                    .is_some_and(|p| p.session_id == route_state.session_id)
                {
                    guard.take()
                } else {
                    None
                }
            };
            route_state.signal_done().await;
            drop(taken);
            static_html_response(StatusCode::BAD_GATEWAY, HTML_EXCHANGE_FAILED)
        }
    }
}

fn normalize_and_build_issuer_endpoints(raw: &str) -> Result<(Url, String, Url), ProviderError> {
    validate_upstream_url(raw).map_err(|err| ProviderError::InvalidConfig(err.to_string()))?;

    let parsed = Url::parse(raw).map_err(|err| {
        ProviderError::InvalidConfig(format!("malformed OAuth issuer URL: {err}"))
    })?;
    if !matches!(parsed.origin(), Origin::Tuple(..)) {
        return Err(ProviderError::InvalidConfig(
            "OAuth issuer URL must have a tuple origin".to_string(),
        ));
    }

    let origin = parsed.origin().ascii_serialization();
    let base_path = parsed.path().trim_end_matches('/');
    let normalized_str = format!("{origin}{base_path}");
    let normalized = Url::parse(&normalized_str).map_err(|_| {
        ProviderError::InvalidConfig("normalized OAuth issuer URL failed re-parse".to_string())
    })?;

    let expected_path = if base_path.is_empty() { "/" } else { base_path };
    if normalized.scheme() != parsed.scheme()
        || normalized.host_str() != parsed.host_str()
        || normalized.port_or_known_default() != parsed.port_or_known_default()
        || !normalized.username().is_empty()
        || normalized.password().is_some()
        || normalized.query().is_some()
        || normalized.fragment().is_some()
        || normalized.path() != expected_path
    {
        return Err(ProviderError::InvalidConfig(
            "OAuth issuer URL normalization invariant failed".to_string(),
        ));
    }

    let authorize_expected_path = format!("{base_path}/oauth/authorize");
    let authorize_candidate = format!("{normalized_str}/oauth/authorize");
    let authorize_url =
        verify_issuer_endpoint(&normalized, &authorize_candidate, &authorize_expected_path)?;

    let token_expected_path = format!("{base_path}/oauth/token");
    let token_candidate = format!("{normalized_str}/oauth/token");
    let token_url = verify_issuer_endpoint(&normalized, &token_candidate, &token_expected_path)?;

    Ok((normalized, authorize_url.into(), token_url))
}

fn verify_issuer_endpoint(
    issuer: &Url,
    candidate: &str,
    expected_path: &str,
) -> Result<Url, ProviderError> {
    let parsed = Url::parse(candidate).map_err(|_| {
        ProviderError::InvalidConfig("OAuth endpoint URL failed canonical parse".to_string())
    })?;
    if parsed.scheme() != issuer.scheme()
        || parsed.host_str() != issuer.host_str()
        || parsed.port_or_known_default() != issuer.port_or_known_default()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || parsed.path() != expected_path
    {
        return Err(ProviderError::InvalidConfig(
            "OAuth endpoint URL failed post-parse equivalence check".to_string(),
        ));
    }
    Ok(parsed)
}

fn generate_random_base64url_32() -> Result<SecretString, ProviderError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|err| {
        ProviderError::OAuth(format!("CSPRNG failed generating OAuth entropy: {err}"))
    })?;
    Ok(SecretString::new(URL_SAFE_NO_PAD.encode(bytes)))
}

fn compute_pkce_s256_challenge(verifier: &str) -> String {
    let digest = digest::digest(&digest::SHA256, verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest.as_ref())
}

fn generate_pkce_pair() -> Result<(SecretString, String), ProviderError> {
    let verifier = generate_random_base64url_32()?;
    let challenge = compute_pkce_s256_challenge(verifier.expose());
    Ok((verifier, challenge))
}

fn build_authorize_url(
    authorize_endpoint: &str,
    redirect_uri: &str,
    code_challenge: &str,
    state: &str,
) -> String {
    let query = form_urlencoded::Serializer::new(String::new())
        .append_pair("response_type", "code")
        .append_pair("client_id", CODEX_OAUTH_CLIENT_ID)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", CODEX_OAUTH_SCOPES)
        .append_pair("code_challenge", code_challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("originator", CODEX_OAUTH_ORIGINATOR)
        .finish();
    format!("{authorize_endpoint}?{query}")
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

struct ParsedManualInput {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    requires_state: bool,
}

fn has_http_scheme_prefix(value: &str) -> bool {
    value
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("http://"))
        || value
            .get(..8)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"))
}

fn parse_manual_callback_input(input: &str) -> Result<ParsedManualInput, ProviderError> {
    if input.len() > MAX_MANUAL_CALLBACK_INPUT_BYTES {
        return Err(ProviderError::OAuth(format!(
            "callback input exceeds maximum allowed length of {MAX_MANUAL_CALLBACK_INPUT_BYTES} bytes"
        )));
    }
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(ProviderError::OAuth(
            "callback input cannot be empty".to_string(),
        ));
    }
    if trimmed.bytes().any(|b| b < 0x20 || b == 0x7F) {
        return Err(ProviderError::OAuth(
            "callback input contains invalid control characters".to_string(),
        ));
    }

    // Form 1: Full redirect URL (`http://...` or `https://...`, case-insensitive scheme)
    if has_http_scheme_prefix(trimmed) {
        let parsed = Url::parse(trimmed)
            .map_err(|_| ProviderError::OAuth("pasted redirect URL is malformed".to_string()))?;
        let mut code = None;
        let mut state = None;
        let mut error = None;
        for (k, v) in parsed.query_pairs() {
            match k.as_ref() {
                "code" if code.is_none() => code = Some(v.into_owned()),
                "state" if state.is_none() => state = Some(v.into_owned()),
                "error" if error.is_none() => error = Some(v.into_owned()),
                _ => {}
            }
        }
        return validate_structured_callback_fields(code, state, error);
    }

    // Form 2: Query string (`?code=...&state=...`, `code=...&state=...`) or schemeless
    // address-bar URL (`localhost:1455/auth/callback?code=...&state=...`).
    let without_fragment = trimmed.split('#').next().unwrap_or(trimmed);
    let query_part = if !trimmed.contains("://") {
        if let Some((_, query)) = without_fragment.split_once('?') {
            Some(query)
        } else if without_fragment.contains('=')
            && form_urlencoded::parse(without_fragment.as_bytes())
                .any(|(k, _)| matches!(k.as_ref(), "code" | "state" | "error"))
        {
            Some(without_fragment)
        } else {
            None
        }
    } else {
        None
    };
    if let Some(query_part) = query_part {
        let mut code = None;
        let mut state = None;
        let mut error = None;
        for (k, v) in form_urlencoded::parse(query_part.as_bytes()) {
            match k.as_ref() {
                "code" if code.is_none() => code = Some(v.into_owned()),
                "state" if state.is_none() => state = Some(v.into_owned()),
                "error" if error.is_none() => error = Some(v.into_owned()),
                _ => {}
            }
        }
        return validate_structured_callback_fields(code, state, error);
    }

    // Form 3: `code#state` shorthand
    if !trimmed.contains("://") && !trimmed.contains('?') && trimmed.contains('#') {
        let Some((code_part, state_part)) = trimmed.split_once('#') else {
            return Err(ProviderError::OAuth(
                "invalid code#state callback format".to_string(),
            ));
        };
        if code_part.is_empty()
            || state_part.is_empty()
            || !is_header_safe_ascii(code_part)
            || !is_header_safe_ascii(state_part)
        {
            return Err(ProviderError::OAuth(
                "code#state callback input must contain valid code and state values".to_string(),
            ));
        }
        return Ok(ParsedManualInput {
            code: Some(code_part.to_string()),
            state: Some(state_part.to_string()),
            error: None,
            requires_state: true,
        });
    }

    // Form 4: Raw authorization code
    if !trimmed.contains("://")
        && !trimmed.contains(['?', '&', '/', '#', '='])
        && is_header_safe_ascii(trimmed)
    {
        return Ok(ParsedManualInput {
            code: Some(trimmed.to_string()),
            state: None,
            error: None,
            requires_state: false,
        });
    }

    Err(ProviderError::OAuth(
        "unrecognized OAuth callback format — paste the full redirect URL or authorization code"
            .to_string(),
    ))
}

fn validate_structured_callback_fields(
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
) -> Result<ParsedManualInput, ProviderError> {
    let error = error.filter(|s| !s.is_empty());
    let state = state.filter(|s| !s.is_empty());
    if error.is_some() {
        return Ok(ParsedManualInput {
            code: None,
            state,
            error,
            requires_state: false,
        });
    }
    let code = code.filter(|s| !s.is_empty()).ok_or_else(|| {
        ProviderError::OAuth("pasted callback is missing required 'code' parameter".to_string())
    })?;
    if !is_header_safe_ascii(&code) {
        return Err(ProviderError::OAuth(
            "pasted authorization code contains invalid characters".to_string(),
        ));
    }
    let state = state.ok_or_else(|| {
        ProviderError::OAuth("pasted callback is missing required 'state' parameter".to_string())
    })?;
    Ok(ParsedManualInput {
        code: Some(code),
        state: Some(state),
        error: None,
        requires_state: true,
    })
}

#[derive(Deserialize)]
struct OAuthTokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

struct DecodedJwtClaims {
    account_id: Option<String>,
    plan_type: Option<String>,
    email_hint: Option<String>,
    exp_secs: Option<u64>,
}

fn decode_jwt_payload_claims(token: &str) -> Result<DecodedJwtClaims, ProviderError> {
    let mut parts = token.split('.');
    let (Some(header), Some(payload), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(ProviderError::OAuth(
            "OAuth token is not a valid 3-segment JWT".to_string(),
        ));
    };
    if header.is_empty() || payload.is_empty() || signature.is_empty() {
        return Err(ProviderError::OAuth(
            "OAuth token contains an empty JWT segment".to_string(),
        ));
    }

    let max_encoded_len = (MAX_JWT_PAYLOAD_BYTES * 4) / 3 + 4;
    if payload.len() > max_encoded_len {
        return Err(ProviderError::OAuth(
            "OAuth JWT payload exceeds maximum allowed size".to_string(),
        ));
    }

    let decoded = URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .map_err(|_| {
            ProviderError::OAuth("OAuth JWT payload is not valid Base64URL".to_string())
        })?;
    if decoded.len() > MAX_JWT_PAYLOAD_BYTES {
        return Err(ProviderError::OAuth(
            "OAuth JWT payload exceeds maximum decoded size".to_string(),
        ));
    }

    let value: serde_json::Value = serde_json::from_slice(&decoded)
        .map_err(|_| ProviderError::OAuth("OAuth JWT payload is not valid JSON".to_string()))?;
    let Some(obj) = value.as_object() else {
        return Err(ProviderError::OAuth(
            "OAuth JWT payload must be a JSON object".to_string(),
        ));
    };

    let auth_ns = obj
        .get("https://api.openai.com/auth")
        .and_then(serde_json::Value::as_object);
    let profile_ns = obj
        .get("https://api.openai.com/profile")
        .and_then(serde_json::Value::as_object);

    let raw_account_id = auth_ns
        .and_then(|ns| ns.get("chatgpt_account_id"))
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            obj.get("chatgpt_account_id")
                .and_then(serde_json::Value::as_str)
        });
    let account_id = match raw_account_id {
        Some(raw) => Some(validate_account_id(raw)?),
        None => None,
    };

    let raw_plan_type = auth_ns
        .and_then(|ns| ns.get("chatgpt_plan_type"))
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            obj.get("chatgpt_plan_type")
                .and_then(serde_json::Value::as_str)
        });
    let plan_type = raw_plan_type.and_then(sanitize_plan_type);

    let raw_email = obj
        .get("email")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            profile_ns
                .and_then(|ns| ns.get("email"))
                .and_then(serde_json::Value::as_str)
        });
    let email_hint = raw_email.and_then(mask_email_hint);

    let exp_secs = obj.get("exp").and_then(serde_json::Value::as_u64);

    Ok(DecodedJwtClaims {
        account_id,
        plan_type,
        email_hint,
        exp_secs,
    })
}

fn validate_account_id(raw: &str) -> Result<String, ProviderError> {
    if !is_header_safe_ascii(raw) || raw.len() > MAX_ACCOUNT_ID_LEN {
        return Err(ProviderError::OAuth(
            "OAuth JWT chatgpt_account_id is invalid or unsafe for HTTP headers".to_string(),
        ));
    }
    Ok(raw.to_string())
}

fn sanitize_plan_type(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed.len() > MAX_PLAN_TYPE_LEN
        || trimmed.chars().any(char::is_control)
    {
        return None;
    }
    Some(trimmed.to_string())
}

/// Masks an email address to a UTF-8-safe non-identifying hint:
/// - `"jack@example.com"` → `Some("j***@example.com")`
/// - `"a@example.com"` → `Some("***@example.com")`
/// - `"élan@example.com"` → `Some("é***@example.com")`
/// - malformed / control / whitespace → `None`
pub(crate) fn mask_email_hint(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed.len() > MAX_EMAIL_LEN
        || trimmed.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return None;
    }
    let (local, domain) = trimmed.split_once('@')?;
    if local.is_empty() || domain.is_empty() || domain.contains('@') {
        return None;
    }
    let mut chars = local.chars();
    let first = chars.next()?;
    if chars.next().is_none() {
        Some(format!("***@{domain}"))
    } else {
        Some(format!("{first}***@{domain}"))
    }
}

fn build_credentials_from_token_response(
    resp: OAuthTokenResponse,
    previous: Option<&CodexCredentials>,
    now_ms: u64,
) -> Result<CodexCredentials, ProviderError> {
    if !is_header_safe_ascii(&resp.access_token) {
        return Err(ProviderError::OAuth(
            "OAuth token response contained an invalid access_token".to_string(),
        ));
    }

    let access_claims = decode_jwt_payload_claims(&resp.access_token)?;

    let id_token = match resp.id_token {
        Some(tok) if !tok.is_empty() => {
            if !is_header_safe_ascii(&tok) {
                return Err(ProviderError::OAuth(
                    "OAuth token response contained an invalid id_token".to_string(),
                ));
            }
            Some(SecretString::new(tok))
        }
        _ => previous.and_then(|p| p.id_token.clone()),
    };

    let id_claims = match id_token.as_ref() {
        Some(tok) => decode_jwt_payload_claims(tok.expose()).ok(),
        None => None,
    };

    let refresh_token = match resp.refresh_token {
        Some(tok) if !tok.is_empty() => {
            if !is_header_safe_ascii(&tok) {
                return Err(ProviderError::OAuth(
                    "OAuth token response contained an invalid refresh_token".to_string(),
                ));
            }
            SecretString::new(tok)
        }
        _ => previous.map(|p| p.refresh_token.clone()).ok_or_else(|| {
            ProviderError::OAuth("OAuth token response missing required refresh_token".to_string())
        })?,
    };

    let account_id = access_claims
        .account_id
        .or_else(|| id_claims.as_ref().and_then(|c| c.account_id.clone()))
        .or_else(|| previous.map(|p| p.account_id.clone()))
        .ok_or_else(|| {
            ProviderError::OAuth("OAuth JWT missing required chatgpt_account_id claim".to_string())
        })?;

    let plan_type = access_claims
        .plan_type
        .or_else(|| id_claims.as_ref().and_then(|c| c.plan_type.clone()))
        .or_else(|| previous.and_then(|p| p.plan_type.clone()));

    let email_hint = id_claims
        .as_ref()
        .and_then(|c| c.email_hint.clone())
        .or(access_claims.email_hint)
        .or_else(|| previous.and_then(|p| p.email_hint.clone()));

    let expires_at_ms = match access_claims.exp_secs {
        Some(exp) if exp > 0 => exp.checked_mul(1000).ok_or_else(|| {
            ProviderError::OAuth("OAuth JWT exp timestamp overflowed".to_string())
        })?,
        _ => match resp.expires_in {
            Some(secs) if secs > 0 => secs
                .checked_mul(1000)
                .and_then(|delta| now_ms.checked_add(delta))
                .ok_or_else(|| {
                    ProviderError::OAuth("OAuth expires_in timestamp overflowed".to_string())
                })?,
            _ => {
                return Err(ProviderError::OAuth(
                    "OAuth token missing valid exp claim and expires_in".to_string(),
                ));
            }
        },
    };

    let creds = CodexCredentials {
        schema_version: AUTH_SCHEMA_VERSION,
        auth_mode: CodexAuthMode::ChatGptOAuth,
        access_token: SecretString::new(resp.access_token),
        refresh_token,
        id_token,
        account_id,
        expires_at_ms,
        plan_type,
        email_hint,
        last_refresh_utc: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
    };
    creds
        .validate()
        .map_err(|err| ProviderError::OAuth(err.to_string()))?;
    Ok(creds)
}

fn is_refresh_auth_expired_body(body: &[u8]) -> bool {
    let Ok(val) = serde_json::from_slice::<serde_json::Value>(body) else {
        return false;
    };
    let code = val.get("error").and_then(|err| {
        err.as_str().or_else(|| {
            err.as_object()
                .and_then(|obj| obj.get("code"))
                .and_then(serde_json::Value::as_str)
        })
    });
    matches!(
        code,
        Some(
            "invalid_grant"
                | "refresh_token_reused"
                | "refresh_token_expired"
                | "refresh_token_invalidated"
        )
    )
}

async fn read_bounded_body(
    mut resp: reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>, ProviderError> {
    if let Some(len) = resp.content_length()
        && len > max_bytes as u64
    {
        return Err(ProviderError::OAuth(format!(
            "OAuth response body exceeded {max_bytes}-byte cap"
        )));
    }
    let mut buf = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|err| {
        ProviderError::OAuth(format!(
            "failed reading OAuth response body: {}",
            err.without_url()
        ))
    })? {
        if buf.len().saturating_add(chunk.len()) > max_bytes {
            return Err(ProviderError::OAuth(format!(
                "OAuth response body exceeded {max_bytes}-byte cap"
            )));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

fn is_header_safe_ascii(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| (0x21..=0x7E).contains(&b))
}

fn current_utc_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::AtomicUsize;

    use axum::body::Bytes;
    use axum::routing::post;
    use companion_core::{FileAuthStore, MemoryAuthStore};
    use serde_json::json;
    use tempfile::TempDir;

    use super::*;

    fn make_test_jwt(payload: &serde_json::Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"RS256","typ":"JWT"}"#);
        let body = URL_SAFE_NO_PAD.encode(serde_json::to_vec(payload).unwrap());
        format!("{header}.{body}.test_signature")
    }

    fn loopback_http_client() -> reqwest::Client {
        reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()
            .expect("test client")
    }

    async fn spawn_mock_oauth_server(router: Router) -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind mock oauth");
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        tokio::task::yield_now().await;
        (format!("http://127.0.0.1:{port}"), handle)
    }

    fn parse_query_map(url_str: &str) -> HashMap<String, String> {
        let parsed = Url::parse(url_str).expect("valid url");
        parsed
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect()
    }

    #[test]
    fn pkce_s256_authorize_url_and_s3_issuer_validation() {
        let (v1, c1) = generate_pkce_pair().expect("pkce 1");
        let (v2, c2) = generate_pkce_pair().expect("pkce 2");
        assert_eq!(v1.expose().len(), 43);
        assert_eq!(c1.len(), 43);
        assert_ne!(v1.expose(), v2.expose());
        assert_ne!(c1, c2);

        // Verify RFC 7636 Appendix B test vector and S256 math directly
        assert_eq!(
            compute_pkce_s256_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        let expected_c1 = URL_SAFE_NO_PAD
            .encode(digest::digest(&digest::SHA256, v1.expose().as_bytes()).as_ref());
        assert_eq!(c1, expected_c1);

        let url = build_authorize_url(
            "https://auth.openai.com/oauth/authorize",
            "http://localhost:1455/auth/callback",
            &c1,
            "state_abc",
        );
        let q = parse_query_map(&url);
        assert_eq!(q.get("response_type").map(String::as_str), Some("code"));
        assert_eq!(
            q.get("client_id").map(String::as_str),
            Some(CODEX_OAUTH_CLIENT_ID)
        );
        assert_eq!(
            q.get("redirect_uri").map(String::as_str),
            Some("http://localhost:1455/auth/callback")
        );
        assert_eq!(q.get("scope").map(String::as_str), Some(CODEX_OAUTH_SCOPES));
        assert_eq!(
            q.get("code_challenge").map(String::as_str),
            Some(c1.as_str())
        );
        assert_eq!(
            q.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert_eq!(q.get("state").map(String::as_str), Some("state_abc"));
        assert_eq!(
            q.get("id_token_add_organizations").map(String::as_str),
            Some("true")
        );
        assert_eq!(
            q.get("codex_cli_simplified_flow").map(String::as_str),
            Some("true")
        );
        assert_eq!(
            q.get("originator").map(String::as_str),
            Some(CODEX_OAUTH_ORIGINATOR)
        );

        // S3 issuer validation: HTTPS remote and loopback HTTP allowed; remote HTTP/creds/traversal/query rejected
        let store: Arc<dyn AuthStore> = Arc::new(MemoryAuthStore::new());
        for ok_issuer in [
            "https://auth.openai.com",
            "https://auth.openai.com/",
            "http://127.0.0.1:18080",
            "http://localhost:18080/prefix",
        ] {
            let opts = CodexAuthOptions {
                oauth_issuer: ok_issuer.to_string(),
                ..Default::default()
            };
            assert!(
                CodexAuthManager::with_options(store.clone(), opts).is_ok(),
                "expected issuer allowed: {ok_issuer}"
            );
        }
        for bad_issuer in [
            "http://evil.example.com",
            "https://user:pass@auth.openai.com",
            "https://auth.openai.com/a/../b",
            "https://auth.openai.com?foo=bar",
            "https://auth.openai.com#frag",
            "ftp://auth.openai.com",
        ] {
            let opts = CodexAuthOptions {
                oauth_issuer: bad_issuer.to_string(),
                ..Default::default()
            };
            assert!(
                matches!(
                    CodexAuthManager::with_options(store.clone(), opts),
                    Err(ProviderError::InvalidConfig(_))
                ),
                "expected issuer rejected: {bad_issuer}"
            );
        }
    }

    #[test]
    fn jwt_decoder_and_mask_email_hint_table() {
        // Masking table including UTF-8
        assert_eq!(
            mask_email_hint("jack@example.com").as_deref(),
            Some("j***@example.com")
        );
        assert_eq!(
            mask_email_hint("a@example.com").as_deref(),
            Some("***@example.com")
        );
        assert_eq!(
            mask_email_hint("élan@example.com").as_deref(),
            Some("é***@example.com")
        );
        for bad in [
            "no-at",
            "a@b@c",
            "@x.com",
            "a@",
            "a\n@x.com",
            "a b@x.com",
            "",
        ] {
            assert_eq!(mask_email_hint(bad), None, "expected None for '{bad:?}'");
        }

        // Nested OpenAI auth namespace + id_token email fallback
        let access_jwt = make_test_jwt(&json!({
            "exp": 1_900_000_000u64,
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct_nested_9988",
                "chatgpt_plan_type": "pro"
            }
        }));
        let id_jwt = make_test_jwt(&json!({
            "email": "élan@example.com"
        }));
        let creds = build_credentials_from_token_response(
            OAuthTokenResponse {
                access_token: access_jwt,
                refresh_token: Some("rt_initial_secret".to_string()),
                id_token: Some(id_jwt),
                expires_in: Some(3600),
            },
            None,
            1_800_000_000_000,
        )
        .expect("build creds");
        assert_eq!(creds.account_id, "acct_nested_9988");
        assert_eq!(creds.plan_type.as_deref(), Some("pro"));
        assert_eq!(creds.email_hint.as_deref(), Some("é***@example.com"));
        assert_eq!(creds.expires_at_ms, 1_900_000_000_000);

        // Top-level fallback + expires_in fallback when JWT omits exp
        let top_jwt = make_test_jwt(&json!({
            "chatgpt_account_id": "acct_top_1234",
            "chatgpt_plan_type": "plus"
        }));
        let creds2 = build_credentials_from_token_response(
            OAuthTokenResponse {
                access_token: top_jwt,
                refresh_token: Some("rt_2".to_string()),
                id_token: None,
                expires_in: Some(1800),
            },
            None,
            1_800_000_000_000,
        )
        .expect("expires_in fallback");
        assert_eq!(creds2.account_id, "acct_top_1234");
        assert_eq!(creds2.plan_type.as_deref(), Some("plus"));
        assert_eq!(creds2.expires_at_ms, 1_800_001_800_000);

        // Overflowing exp, missing account_id, malformed JWTs fail closed without leaking secrets
        let overflow_jwt = make_test_jwt(&json!({
            "exp": u64::MAX,
            "chatgpt_account_id": "acct_ok"
        }));
        assert!(
            build_credentials_from_token_response(
                OAuthTokenResponse {
                    access_token: overflow_jwt,
                    refresh_token: Some("rt_secret_overflow".to_string()),
                    id_token: None,
                    expires_in: None,
                },
                None,
                1_800_000_000_000,
            )
            .is_err()
        );

        let missing_acct_jwt = make_test_jwt(&json!({
            "exp": 1_900_000_000u64,
            "email": "secret.person@example.com"
        }));
        let err = build_credentials_from_token_response(
            OAuthTokenResponse {
                access_token: missing_acct_jwt,
                refresh_token: Some("rt_secret_missing_acct".to_string()),
                id_token: None,
                expires_in: None,
            },
            None,
            1_800_000_000_000,
        )
        .expect_err("missing account_id must fail");
        assert!(!err.to_string().contains("secret.person"));
        assert!(!err.to_string().contains("rt_secret_missing_acct"));

        for bad_jwt in ["not-a-jwt", "a.b", "a..c", "a.!!!.c", "a.e30=.c.d"] {
            assert!(decode_jwt_payload_claims(bad_jwt).is_err());
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn full_loopback_oauth_happy_path_persists_0600_and_releases_listener() {
        let captured_form = Arc::new(Mutex::new(HashMap::<String, String>::new()));
        let captured_ct = Arc::new(Mutex::new(String::new()));
        let captured_form_clone = captured_form.clone();
        let captured_ct_clone = captured_ct.clone();

        let access_jwt = make_test_jwt(&json!({
            "exp": (current_utc_epoch_ms() / 1000) + 3600,
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct_happy_1234",
                "chatgpt_plan_type": "pro"
            }
        }));
        let id_jwt = make_test_jwt(&json!({ "email": "jack@example.com" }));
        let access_jwt_clone = access_jwt.clone();
        let id_jwt_clone = id_jwt.clone();

        let router = Router::new().route(
            "/oauth/token",
            post(move |headers: axum::http::HeaderMap, body: Bytes| {
                let captured_form = captured_form_clone.clone();
                let captured_ct = captured_ct_clone.clone();
                let access_jwt = access_jwt_clone.clone();
                let id_jwt = id_jwt_clone.clone();
                async move {
                    if let Some(ct) = headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok()) {
                        *captured_ct.lock().await = ct.to_string();
                    }
                    let pairs: HashMap<String, String> = form_urlencoded::parse(&body)
                        .map(|(k, v)| (k.into_owned(), v.into_owned()))
                        .collect();
                    *captured_form.lock().await = pairs;
                    axum::Json(json!({
                        "access_token": access_jwt,
                        "refresh_token": "rt_happy_secret_1",
                        "id_token": id_jwt,
                        "expires_in": 3600
                    }))
                }
            }),
        );
        let (issuer, server_handle) = spawn_mock_oauth_server(router).await;

        let tmp = TempDir::new().unwrap();
        let store = Arc::new(
            FileAuthStore::for_config_path(&tmp.path().join("config.json")).expect("store"),
        );
        let manager = CodexAuthManager::with_options(
            store.clone(),
            CodexAuthOptions {
                oauth_issuer: issuer,
                callback_bind_port: 0,
                login_ttl: Duration::from_secs(30),
                ..Default::default()
            },
        )
        .expect("manager");

        let start = manager.start_login().await.expect("start_login");
        assert!(start.callback_listening);
        assert!(!start.manual_fallback);
        let port = start.callback_port.expect("bound port");
        assert!(manager.is_login_pending().await);

        let auth_q = parse_query_map(&start.authorize_url);
        let state = auth_q.get("state").expect("state").clone();
        let challenge = auth_q.get("code_challenge").expect("challenge").clone();
        let redirect_uri = auth_q.get("redirect_uri").expect("redirect_uri").clone();
        assert_eq!(
            redirect_uri,
            format!("http://localhost:{port}{CODEX_CALLBACK_PATH}")
        );

        let http = loopback_http_client();
        let cb_resp = http
            .get(format!(
                "http://127.0.0.1:{port}{CODEX_CALLBACK_PATH}?code=auth_code_42&state={state}"
            ))
            .send()
            .await
            .expect("callback GET");
        assert_eq!(cb_resp.status(), StatusCode::OK);
        assert_eq!(
            cb_resp
                .headers()
                .get(CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some("no-store")
        );
        assert_eq!(
            cb_resp
                .headers()
                .get("x-content-type-options")
                .and_then(|v| v.to_str().ok()),
            Some("nosniff")
        );
        let html = cb_resp.text().await.unwrap();
        assert!(html.contains("Signed in — return to Antigravity"));

        // Verify /oauth/token received form-urlencoded PKCE exchange matching challenge
        assert_eq!(
            *captured_ct.lock().await,
            "application/x-www-form-urlencoded"
        );
        let form = captured_form.lock().await.clone();
        assert_eq!(
            form.get("grant_type").map(String::as_str),
            Some("authorization_code")
        );
        assert_eq!(
            form.get("client_id").map(String::as_str),
            Some(CODEX_OAUTH_CLIENT_ID)
        );
        assert_eq!(form.get("code").map(String::as_str), Some("auth_code_42"));
        assert_eq!(
            form.get("redirect_uri").map(String::as_str),
            Some(redirect_uri.as_str())
        );
        let verifier = form.get("code_verifier").expect("code_verifier");
        assert_eq!(compute_pkce_s256_challenge(verifier), challenge);

        // Credentials persisted cleanly (0600 on Unix) & pending cleared
        assert!(!manager.is_login_pending().await);
        let saved = store.load_codex().unwrap().expect("saved codex creds");
        assert_eq!(saved.access_token.expose(), access_jwt);
        assert_eq!(saved.refresh_token.expose(), "rt_happy_secret_1");
        assert_eq!(saved.account_id, "acct_happy_1234");
        assert_eq!(saved.plan_type.as_deref(), Some("pro"));
        assert_eq!(saved.email_hint.as_deref(), Some("j***@example.com"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mode = std::fs::metadata(store.codex_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }

        // Listener shuts down promptly after callback completes
        let mut released = false;
        for _ in 0..40 {
            if TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await.is_ok() {
                released = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            released,
            "callback listener port should be released after completion"
        );

        server_handle.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn csrf_state_mismatch_and_oauth_error_do_not_call_token_or_clobber_creds() {
        let token_hits = Arc::new(AtomicUsize::new(0));
        let token_hits_clone = token_hits.clone();
        let router = Router::new().route(
            "/oauth/token",
            post(move || {
                let hits = token_hits_clone.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    StatusCode::INTERNAL_SERVER_ERROR
                }
            }),
        );
        let (issuer, server_handle) = spawn_mock_oauth_server(router).await;

        let existing = CodexCredentials {
            schema_version: AUTH_SCHEMA_VERSION,
            auth_mode: CodexAuthMode::ChatGptOAuth,
            access_token: SecretString::new("existing_access"),
            refresh_token: SecretString::new("existing_refresh"),
            id_token: None,
            account_id: "acct_existing_1111".to_string(),
            expires_at_ms: current_utc_epoch_ms() + 3_600_000,
            plan_type: Some("plus".to_string()),
            email_hint: Some("e***@example.com".to_string()),
            last_refresh_utc: "2026-11-26T22:00:00Z".to_string(),
        };
        let store = Arc::new(MemoryAuthStore::with_codex_credentials(existing.clone()));
        let manager = CodexAuthManager::with_options(
            store.clone(),
            CodexAuthOptions {
                oauth_issuer: issuer,
                callback_bind_port: 0,
                login_ttl: Duration::from_secs(30),
                ..Default::default()
            },
        )
        .unwrap();

        let start = manager.start_login().await.unwrap();
        let port = start.callback_port.unwrap();
        let real_state = parse_query_map(&start.authorize_url)
            .remove("state")
            .unwrap();
        let http = loopback_http_client();

        // Wrong state & missing state return 400, hit /oauth/token 0 times, keep pending & creds intact
        for bad_q in ["code=evil&state=wrong_state", "code=evil"] {
            let resp = http
                .get(format!(
                    "http://127.0.0.1:{port}{CODEX_CALLBACK_PATH}?{bad_q}"
                ))
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
            let body = resp.text().await.unwrap();
            assert!(!body.contains("wrong_state"));
        }
        assert_eq!(token_hits.load(Ordering::SeqCst), 0);
        assert!(manager.is_login_pending().await);
        assert_eq!(store.load_codex().unwrap(), Some(existing.clone()));

        // OAuth error with real state returns 400 static HTML without reflecting error_description
        let err_resp = http
            .get(format!(
                "http://127.0.0.1:{port}{CODEX_CALLBACK_PATH}?error=access_denied&error_description=<script>xss</script>&state={real_state}"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(err_resp.status(), StatusCode::BAD_REQUEST);
        let err_body = err_resp.text().await.unwrap();
        assert!(err_body.contains("Sign-in was canceled or denied"));
        assert!(!err_body.contains("<script>"));
        assert_eq!(token_hits.load(Ordering::SeqCst), 0);
        assert!(!manager.is_login_pending().await);
        assert_eq!(store.load_codex().unwrap(), Some(existing));

        server_handle.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_replace_and_ttl_expiry_release_port_deterministically() {
        let store: Arc<dyn AuthStore> = Arc::new(MemoryAuthStore::new());
        // Pick a free ephemeral port via probe bind, then drop to use as fixed DI port
        let probe = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let fixed_port = probe.local_addr().unwrap().port();
        drop(probe);

        let manager = CodexAuthManager::with_options(
            store,
            CodexAuthOptions {
                oauth_issuer: "http://127.0.0.1:9".to_string(),
                callback_bind_port: fixed_port,
                login_ttl: Duration::from_millis(120),
                ..Default::default()
            },
        )
        .unwrap();

        // 1. start_login -> cancel_login releases port immediately
        let first = manager.start_login().await.unwrap();
        assert_eq!(first.callback_port, Some(fixed_port));
        assert!(manager.is_login_pending().await);
        manager.cancel_login().await;
        assert!(!manager.is_login_pending().await);
        let rebind = TcpListener::bind((Ipv4Addr::LOCALHOST, fixed_port))
            .await
            .expect("port must be released immediately on cancel_login");
        drop(rebind);

        // 2. Repeated start_login tears down previous listener and rebinds same port cleanly
        let second = manager.start_login().await.unwrap();
        assert_eq!(second.callback_port, Some(fixed_port));
        let third = manager.start_login().await.unwrap();
        assert_eq!(third.callback_port, Some(fixed_port));
        assert_ne!(second.authorize_url, third.authorize_url);

        // 3. TTL expiry clears pending and releases port
        tokio::time::sleep(Duration::from_millis(220)).await;
        assert!(!manager.is_login_pending().await);
        let after_ttl = TcpListener::bind((Ipv4Addr::LOCALHOST, fixed_port))
            .await
            .expect("port must be released after login_ttl expires");
        drop(after_ttl);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn addr_in_use_fallback_and_complete_login_manual_four_forms() {
        let token_hits = Arc::new(AtomicUsize::new(0));
        let token_hits_clone = token_hits.clone();
        let router = Router::new().route(
            "/oauth/token",
            post(move |body: Bytes| {
                let hits = token_hits_clone.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    let pairs: HashMap<String, String> = form_urlencoded::parse(&body)
                        .map(|(k, v)| (k.into_owned(), v.into_owned()))
                        .collect();
                    let code = pairs.get("code").cloned().unwrap_or_default();
                    let jwt = make_test_jwt(&json!({
                        "exp": (current_utc_epoch_ms() / 1000) + 3600,
                        "chatgpt_account_id": format!("acct_{code}"),
                        "chatgpt_plan_type": "plus"
                    }));
                    axum::Json(json!({
                        "access_token": jwt,
                        "refresh_token": format!("rt_{code}"),
                        "expires_in": 3600
                    }))
                }
            }),
        );
        let (issuer, server_handle) = spawn_mock_oauth_server(router).await;

        // Hold an ephemeral port so start_login triggers graceful AddrInUse fallback
        let blocker = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let occupied_port = blocker.local_addr().unwrap().port();

        let store = Arc::new(MemoryAuthStore::new());
        let manager = CodexAuthManager::with_options(
            store.clone(),
            CodexAuthOptions {
                oauth_issuer: issuer,
                callback_bind_port: occupied_port,
                login_ttl: Duration::from_secs(30),
                ..Default::default()
            },
        )
        .unwrap();

        let start = manager.start_login().await.unwrap();
        assert!(!start.callback_listening);
        assert!(start.manual_fallback);
        assert_eq!(start.callback_port, None);
        assert!(manager.is_login_pending().await);
        let state = parse_query_map(&start.authorize_url)
            .remove("state")
            .unwrap();

        // Bad / wrong-state / error / oversized inputs fail without consuming pending or hitting /oauth/token
        let oversized_input = format!(
            "http://localhost:{occupied_port}/auth/callback?code={}&state={state}",
            "a".repeat(8 * 1024 + 64)
        );
        for bad_input in [
            "",
            "   ",
            "bad\ninput",
            oversized_input.as_str(),
            &format!("http://localhost:{occupied_port}/auth/callback?code=c1&state=wrong"),
            &format!("http://localhost:{occupied_port}/auth/callback?code=c1"),
            &format!(
                "http://localhost:{occupied_port}/auth/callback?error=access_denied&state={state}"
            ),
            &format!("localhost:{occupied_port}/auth/callback?code=c1&state=wrong"),
            &format!("localhost:{occupied_port}/auth/callback?code=c1"),
            &format!("localhost:{occupied_port}/auth/callback?foo=bar"),
            "?code=c1&state=wrong",
            "code=c1",
            "c1#wrong",
        ] {
            assert!(
                manager.complete_login_manual(bad_input).await.is_err(),
                "expected rejection for '{bad_input}'"
            );
            assert!(
                manager.is_login_pending().await,
                "bad manual input must not destroy pending session: '{bad_input}'"
            );
        }
        assert_eq!(token_hits.load(Ordering::SeqCst), 0);

        // Form 1: Full redirect URL succeeds
        let creds1 = manager
            .complete_login_manual(&format!(
                "http://localhost:{occupied_port}/auth/callback?code=form1&state={state}"
            ))
            .await
            .expect("form 1");
        assert_eq!(creds1.account_id, "acct_form1");
        assert!(!manager.is_login_pending().await);

        // Form 1 (case-insensitive scheme): Uppercase HTTP:// redirect URL succeeds
        let s1_upper = manager.start_login().await.unwrap();
        let st1_upper = parse_query_map(&s1_upper.authorize_url)
            .remove("state")
            .unwrap();
        let creds1_upper = manager
            .complete_login_manual(&format!(
                "HTTP://localhost:{occupied_port}/auth/callback?code=form1upper&state={st1_upper}"
            ))
            .await
            .expect("form 1 uppercase scheme");
        assert_eq!(creds1_upper.account_id, "acct_form1upper");

        // Form 2: Query string (?code=...&state=...) succeeds
        let s2 = manager.start_login().await.unwrap();
        let st2 = parse_query_map(&s2.authorize_url).remove("state").unwrap();
        let creds2 = manager
            .complete_login_manual(&format!("?code=form2&state={st2}"))
            .await
            .expect("form 2");
        assert_eq!(creds2.account_id, "acct_form2");

        // Form 2 (schemeless address-bar URL): localhost:<port>/auth/callback?code=...&state=... succeeds
        let s2_schemeless = manager.start_login().await.unwrap();
        let st2_schemeless = parse_query_map(&s2_schemeless.authorize_url)
            .remove("state")
            .unwrap();
        let creds2_schemeless = manager
            .complete_login_manual(&format!(
                "localhost:{occupied_port}/auth/callback?code=form2schemeless&state={st2_schemeless}"
            ))
            .await
            .expect("form 2 schemeless address-bar URL");
        assert_eq!(creds2_schemeless.account_id, "acct_form2schemeless");

        // Form 3: code#state shorthand succeeds
        let s3 = manager.start_login().await.unwrap();
        let st3 = parse_query_map(&s3.authorize_url).remove("state").unwrap();
        let creds3 = manager
            .complete_login_manual(&format!("form3#{st3}"))
            .await
            .expect("form 3");
        assert_eq!(creds3.account_id, "acct_form3");

        // Form 4: Raw authorization code succeeds
        let _s4 = manager.start_login().await.unwrap();
        let creds4 = manager
            .complete_login_manual("form4raw")
            .await
            .expect("form 4");
        assert_eq!(creds4.account_id, "acct_form4raw");
        assert_eq!(token_hits.load(Ordering::SeqCst), 6);

        drop(blocker);
        server_handle.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refresh_fast_path_concurrent_serialization_401_dedup_invalid_grant_and_disconnect() {
        let refresh_hits = Arc::new(AtomicUsize::new(0));
        let refresh_hits_clone = refresh_hits.clone();
        let mode = Arc::new(Mutex::new("ok".to_string()));
        let mode_clone = mode.clone();

        let router = Router::new().route(
            "/oauth/token",
            post(move |body: Bytes| {
                let hits = refresh_hits_clone.clone();
                let mode = mode_clone.clone();
                async move {
                    let hit_num = hits.fetch_add(1, Ordering::SeqCst) + 1;
                    let pairs: HashMap<String, String> = form_urlencoded::parse(&body)
                        .map(|(k, v)| (k.into_owned(), v.into_owned()))
                        .collect();
                    assert_eq!(
                        pairs.get("grant_type").map(String::as_str),
                        Some("refresh_token")
                    );
                    // Small delay to ensure concurrent waiters pile up on locks
                    tokio::time::sleep(Duration::from_millis(40)).await;
                    let current_mode = mode.lock().await.clone();
                    match current_mode.as_str() {
                        "invalid_grant" => (
                            StatusCode::BAD_REQUEST,
                            axum::Json(json!({"error": "invalid_grant"})),
                        )
                            .into_response(),
                        "reused" => (
                            StatusCode::BAD_REQUEST,
                            axum::Json(json!({"error": {"code": "refresh_token_reused"}})),
                        )
                            .into_response(),
                        "unauth" => StatusCode::UNAUTHORIZED.into_response(),
                        _ => {
                            let jwt = make_test_jwt(&json!({
                                "exp": (current_utc_epoch_ms() / 1000) + 7200,
                                "https://api.openai.com/auth": {
                                    "chatgpt_account_id": "acct_refreshed_1",
                                    "chatgpt_plan_type": "pro"
                                },
                                "jti": format!("rot_{hit_num}")
                            }));
                            axum::Json(json!({
                                "access_token": jwt,
                                "refresh_token": format!("rt_rotated_{hit_num}"),
                                "expires_in": 7200
                            }))
                            .into_response()
                        }
                    }
                }
            }),
        );
        let (issuer, server_handle) = spawn_mock_oauth_server(router).await;

        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("config.json");
        let store1 = Arc::new(FileAuthStore::for_config_path(&cfg_path).unwrap());
        let store2 = Arc::new(FileAuthStore::for_config_path(&cfg_path).unwrap());

        let opts = CodexAuthOptions {
            oauth_issuer: issuer,
            callback_bind_port: 0,
            login_ttl: Duration::from_secs(30),
            refresh_margin: Duration::from_secs(300),
            request_timeout: Duration::from_secs(5),
        };
        let mgr1 = CodexAuthManager::with_options(store1.clone(), opts.clone()).unwrap();
        let mgr2 = CodexAuthManager::with_options(store2.clone(), opts).unwrap();

        // 0. Unauthenticated -> NotAuthenticated
        assert!(matches!(
            mgr1.valid_credentials(false).await,
            Err(ProviderError::NotAuthenticated(_))
        ));

        // 1. Fresh token (> 5m left) takes fast path with 0 HTTP hits
        let fresh_creds = CodexCredentials {
            schema_version: AUTH_SCHEMA_VERSION,
            auth_mode: CodexAuthMode::ChatGptOAuth,
            access_token: SecretString::new("fresh_tok"),
            refresh_token: SecretString::new("rt_fresh"),
            id_token: None,
            account_id: "acct_refreshed_1".to_string(),
            expires_at_ms: current_utc_epoch_ms() + 3_600_000,
            plan_type: Some("pro".to_string()),
            email_hint: Some("j***@example.com".to_string()),
            last_refresh_utc: "2026-11-26T22:00:00Z".to_string(),
        };
        store1.save_codex(&fresh_creds).unwrap();
        let fast = mgr1.valid_credentials(false).await.unwrap();
        assert_eq!(fast.access_token.expose(), "fresh_tok");
        assert_eq!(refresh_hits.load(Ordering::SeqCst), 0);

        // 2. Near-expiry (< 5m) with 6 concurrent tasks across 2 independent CodexAuthManager instances
        // sharing FileAuthStore -> EXACTLY 1 POST /oauth/token under .codex-refresh.lock
        let mut expiring = fresh_creds.clone();
        expiring.access_token = SecretString::new("expiring_tok");
        expiring.expires_at_ms = current_utc_epoch_ms() + 60_000;
        store1.save_codex(&expiring).unwrap();

        let mut handles = Vec::new();
        for i in 0..6 {
            let mgr = if i % 2 == 0 {
                mgr1.clone()
            } else {
                mgr2.clone()
            };
            handles.push(tokio::spawn(async move {
                mgr.valid_credentials(false).await.unwrap()
            }));
        }
        let mut rotated_tokens = Vec::new();
        for h in handles {
            rotated_tokens.push(h.await.unwrap().access_token.expose().to_string());
        }
        assert_eq!(refresh_hits.load(Ordering::SeqCst), 1);
        assert!(rotated_tokens.iter().all(|t| t == &rotated_tokens[0]));
        assert_ne!(rotated_tokens[0], "expiring_tok");
        // Verify email_hint preserved from previous creds when refresh response omitted id_token
        let on_disk = store1.load_codex().unwrap().unwrap();
        assert_eq!(on_disk.email_hint.as_deref(), Some("j***@example.com"));
        assert!(store1.codex_refresh_lock_path().exists());

        // 3. Concurrent force_refresh (401 stampede) across 2 tasks -> EXACTLY 1 additional hit
        let h1 = {
            let m = mgr1.clone();
            tokio::spawn(async move { m.valid_credentials(true).await.unwrap() })
        };
        let h2 = {
            let m = mgr1.clone();
            tokio::spawn(async move { m.valid_credentials(true).await.unwrap() })
        };
        let r1 = h1.await.unwrap();
        let r2 = h2.await.unwrap();
        assert_eq!(r1.access_token, r2.access_token);
        assert_eq!(refresh_hits.load(Ordering::SeqCst), 2);

        // 4. invalid_grant, refresh_token_reused, and 401 -> AuthExpired without deleting codex.json
        for err_mode in ["invalid_grant", "reused", "unauth"] {
            *mode.lock().await = err_mode.to_string();
            let err = mgr1
                .valid_credentials(true)
                .await
                .expect_err("must fail with AuthExpired");
            match err {
                ProviderError::AuthExpired(msg) => {
                    assert_eq!(msg, CODEX_SESSION_EXPIRED_MSG);
                }
                other => panic!("expected AuthExpired for mode '{err_mode}', got {other:?}"),
            }
            assert!(
                store1.codex_path().exists(),
                "auth/codex.json must not be silently deleted on refresh error"
            );
        }

        // 5. disconnect cancels pending login and clears auth/codex.json
        let pending = mgr1.start_login().await.unwrap();
        let port = pending.callback_port.unwrap();
        assert!(mgr1.is_login_pending().await);
        mgr1.disconnect().await.unwrap();
        assert!(!mgr1.is_login_pending().await);
        assert_eq!(store1.load_codex().unwrap(), None);
        assert!(!store1.codex_path().exists());
        let rebind = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
            .await
            .expect("disconnect must release pending callback port");
        drop(rebind);

        server_handle.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delayed_oauth_exchange_preserves_pending_and_prevents_resurrection_and_clobber() {
        use tokio::sync::Notify;

        type ExchangeGate = (Arc<Notify>, Arc<Notify>);

        let gates: Arc<Mutex<HashMap<String, ExchangeGate>>> = Arc::new(Mutex::new(HashMap::new()));
        let token_hits = Arc::new(AtomicUsize::new(0));

        let register_gate = |code: &str| {
            let arrived = Arc::new(Notify::new());
            let release = Arc::new(Notify::new());
            (code.to_string(), arrived, release)
        };

        let (c_happy, happy_arrived, happy_release) = register_gate("slow_happy");
        let (c_disc, disc_arrived, disc_release) = register_gate("slow_disconnect");
        let (c_s1, s1_arrived, s1_release) = register_gate("slow_session1");
        let (c_manual, manual_arrived, manual_release) = register_gate("slow_manual");
        {
            let mut g = gates.lock().await;
            g.insert(c_happy, (happy_arrived.clone(), happy_release.clone()));
            g.insert(c_disc, (disc_arrived.clone(), disc_release.clone()));
            g.insert(c_s1, (s1_arrived.clone(), s1_release.clone()));
            g.insert(c_manual, (manual_arrived.clone(), manual_release.clone()));
        }

        let gates_route = gates.clone();
        let hits_route = token_hits.clone();
        let router = Router::new().route(
            "/oauth/token",
            post(move |body: Bytes| {
                let gates = gates_route.clone();
                let hits = hits_route.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    let pairs: HashMap<String, String> = form_urlencoded::parse(&body)
                        .map(|(k, v)| (k.into_owned(), v.into_owned()))
                        .collect();
                    let code = pairs.get("code").cloned().unwrap_or_default();
                    let gate = gates.lock().await.get(&code).cloned();
                    if let Some((arrived, release)) = gate {
                        arrived.notify_one();
                        release.notified().await;
                    }
                    let jwt = make_test_jwt(&json!({
                        "exp": (current_utc_epoch_ms() / 1000) + 3600,
                        "https://api.openai.com/auth": {
                            "chatgpt_account_id": format!("acct_{code}"),
                            "chatgpt_plan_type": "pro"
                        }
                    }));
                    axum::Json(json!({
                        "access_token": jwt,
                        "refresh_token": format!("rt_{code}"),
                        "expires_in": 3600
                    }))
                }
            }),
        );
        let (issuer, server_handle) = spawn_mock_oauth_server(router).await;

        let tmp = TempDir::new().unwrap();
        let store = Arc::new(
            FileAuthStore::for_config_path(&tmp.path().join("config.json")).expect("store"),
        );
        let manager = CodexAuthManager::with_options(
            store.clone(),
            CodexAuthOptions {
                oauth_issuer: issuer,
                callback_bind_port: 0,
                login_ttl: Duration::from_secs(30),
                request_timeout: Duration::from_secs(10),
                ..Default::default()
            },
        )
        .expect("manager");
        let http = loopback_http_client();

        // 1. Slow /oauth/token (> CANCEL_TEARDOWN_GRACE = 50ms):
        //    is_login_pending() stays true, listener stays bound until done_rx, duplicate/manual rejected,
        //    and is_login_pending() becomes false only after auth/codex.json is persisted.
        let s_happy = manager.start_login().await.unwrap();
        let port_happy = s_happy.callback_port.unwrap();
        let state_happy = parse_query_map(&s_happy.authorize_url)
            .remove("state")
            .unwrap();

        let http_cb = http.clone();
        let state_happy_cb = state_happy.clone();
        let cb_task = tokio::spawn(async move {
            http_cb
                .get(format!(
                    "http://127.0.0.1:{port_happy}{CODEX_CALLBACK_PATH}?code=slow_happy&state={state_happy_cb}"
                ))
                .send()
                .await
        });

        tokio::time::timeout(Duration::from_secs(2), happy_arrived.notified())
            .await
            .expect("slow_happy must reach /oauth/token");
        // Sleep 200ms (> CANCEL_TEARDOWN_GRACE = 50ms) while /oauth/token is still blocked.
        tokio::time::sleep(Duration::from_millis(200)).await;

        assert!(
            manager.is_login_pending().await,
            "is_login_pending() must remain true while /oauth/token is in-flight"
        );
        assert_eq!(store.load_codex().unwrap(), None);
        assert!(
            TcpListener::bind((Ipv4Addr::LOCALHOST, port_happy))
                .await
                .is_err(),
            "callback listener must stay bound past CANCEL_TEARDOWN_GRACE until done_rx"
        );
        // Duplicate callback and manual completion must both be rejected while exchanging.
        let dup_resp = http
            .get(format!(
                "http://127.0.0.1:{port_happy}{CODEX_CALLBACK_PATH}?code=slow_happy&state={state_happy}"
            ))
            .send()
            .await
            .expect("duplicate callback should reach live listener");
        assert_eq!(dup_resp.status(), StatusCode::BAD_REQUEST);
        assert!(
            matches!(
                manager
                    .complete_login_manual(&format!("slow_happy#{state_happy}"))
                    .await,
                Err(ProviderError::OAuth(_))
            ),
            "manual completion must fail while callback exchange is in-flight"
        );
        assert_eq!(token_hits.load(Ordering::SeqCst), 1);

        happy_release.notify_one();
        let cb_resp = cb_task
            .await
            .expect("callback task join")
            .expect("callback HTTP response");
        assert_eq!(cb_resp.status(), StatusCode::OK);
        assert!(!manager.is_login_pending().await);
        let saved_happy = store
            .load_codex()
            .unwrap()
            .expect("auth/codex.json persisted");
        assert_eq!(saved_happy.account_id, "acct_slow_happy");

        let mut released = false;
        for _ in 0..40 {
            if TcpListener::bind((Ipv4Addr::LOCALHOST, port_happy))
                .await
                .is_ok()
            {
                released = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            released,
            "listener must release port via done_rx after save"
        );

        // 2. disconnect() mid-exchange prevents credential resurrection even after slow /oauth/token finishes.
        manager.disconnect().await.unwrap();
        let s_disc = manager.start_login().await.unwrap();
        let port_disc = s_disc.callback_port.unwrap();
        let state_disc = parse_query_map(&s_disc.authorize_url)
            .remove("state")
            .unwrap();

        let http_disc = http.clone();
        let cb_disc_task = tokio::spawn(async move {
            http_disc
                .get(format!(
                    "http://127.0.0.1:{port_disc}{CODEX_CALLBACK_PATH}?code=slow_disconnect&state={state_disc}"
                ))
                .send()
                .await
        });

        tokio::time::timeout(Duration::from_secs(2), disc_arrived.notified())
            .await
            .expect("slow_disconnect must reach /oauth/token");
        manager.disconnect().await.unwrap();
        assert!(!manager.is_login_pending().await);
        assert_eq!(store.load_codex().unwrap(), None);

        disc_release.notify_one();
        let _ = cb_disc_task.await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            store.load_codex().unwrap(),
            None,
            "slow callback must never resurrect auth/codex.json after disconnect()"
        );

        // 3. start_login() Session 2 mid-exchange cancels Session 1 and Session 1 CANNOT overwrite Session 2.
        let s1 = manager.start_login().await.unwrap();
        let port_s1 = s1.callback_port.unwrap();
        let state_s1 = parse_query_map(&s1.authorize_url).remove("state").unwrap();

        let http_s1 = http.clone();
        let cb_s1_task = tokio::spawn(async move {
            http_s1
                .get(format!(
                    "http://127.0.0.1:{port_s1}{CODEX_CALLBACK_PATH}?code=slow_session1&state={state_s1}"
                ))
                .send()
                .await
        });

        tokio::time::timeout(Duration::from_secs(2), s1_arrived.notified())
            .await
            .expect("slow_session1 must reach /oauth/token");

        let s2 = manager.start_login().await.unwrap();
        let rebind_s1 = TcpListener::bind((Ipv4Addr::LOCALHOST, port_s1))
            .await
            .expect("starting Session 2 must deterministically release Session 1 callback port");
        drop(rebind_s1);
        let port_s2 = s2.callback_port.unwrap();
        let state_s2 = parse_query_map(&s2.authorize_url).remove("state").unwrap();
        let cb_s2_resp = http
            .get(format!(
                "http://127.0.0.1:{port_s2}{CODEX_CALLBACK_PATH}?code=fast_session2&state={state_s2}"
            ))
            .send()
            .await
            .expect("session 2 callback");
        assert_eq!(cb_s2_resp.status(), StatusCode::OK);
        assert_eq!(
            store.load_codex().unwrap().unwrap().account_id,
            "acct_fast_session2"
        );

        s1_release.notify_one();
        let _ = cb_s1_task.await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            store.load_codex().unwrap().unwrap().account_id,
            "acct_fast_session2",
            "stale Session 1 callback must never overwrite Session 2 credentials"
        );

        // 4. disconnect() during in-flight complete_login_manual prevents credential resurrection.
        manager.disconnect().await.unwrap();
        let s_manual = manager.start_login().await.unwrap();
        let state_manual = parse_query_map(&s_manual.authorize_url)
            .remove("state")
            .unwrap();
        let mgr_manual = manager.clone();
        let manual_task = tokio::spawn(async move {
            mgr_manual
                .complete_login_manual(&format!("slow_manual#{state_manual}"))
                .await
        });

        tokio::time::timeout(Duration::from_secs(2), manual_arrived.notified())
            .await
            .expect("slow_manual must reach /oauth/token");
        assert!(manager.is_login_pending().await);

        manager.disconnect().await.unwrap();
        assert!(!manager.is_login_pending().await);

        manual_release.notify_one();
        let manual_res = manual_task.await.expect("manual task join");
        assert!(
            matches!(manual_res, Err(ProviderError::OAuth(_))),
            "disconnected in-flight manual exchange must fail closed, got {manual_res:?}"
        );
        assert_eq!(
            store.load_codex().unwrap(),
            None,
            "disconnected in-flight manual exchange must never resurrect auth/codex.json"
        );

        server_handle.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refresh_commit_guarded_against_concurrent_disconnect_and_account_switch() {
        use tokio::sync::Notify;

        type RefreshGate = (Arc<Notify>, Arc<Notify>);

        let gates: Arc<Mutex<HashMap<String, RefreshGate>>> = Arc::new(Mutex::new(HashMap::new()));
        let make_gate = |rt: &str| {
            (
                rt.to_string(),
                Arc::new(Notify::new()),
                Arc::new(Notify::new()),
            )
        };
        let (rt_disc, disc_arrived, disc_release) = make_gate("rt_acct_a_disc");
        let (rt_switch, switch_arrived, switch_release) = make_gate("rt_acct_a_switch");
        let (rt_xproc, xproc_arrived, xproc_release) = make_gate("rt_acct_a_xproc");
        {
            let mut g = gates.lock().await;
            g.insert(rt_disc, (disc_arrived.clone(), disc_release.clone()));
            g.insert(rt_switch, (switch_arrived.clone(), switch_release.clone()));
            g.insert(rt_xproc, (xproc_arrived.clone(), xproc_release.clone()));
        }

        let gates_route = gates.clone();
        let router = Router::new().route(
            "/oauth/token",
            post(move |body: Bytes| {
                let gates = gates_route.clone();
                async move {
                    let pairs: HashMap<String, String> = form_urlencoded::parse(&body)
                        .map(|(k, v)| (k.into_owned(), v.into_owned()))
                        .collect();
                    let grant_type = pairs.get("grant_type").map(String::as_str).unwrap_or("");
                    match grant_type {
                        "refresh_token" => {
                            let rt = pairs.get("refresh_token").cloned().unwrap_or_default();
                            let gate = gates.lock().await.get(&rt).cloned();
                            if let Some((arrived, release)) = gate {
                                arrived.notify_one();
                                release.notified().await;
                            }
                            let jwt = make_test_jwt(&json!({
                                "exp": (current_utc_epoch_ms() / 1000) + 7200,
                                "https://api.openai.com/auth": {
                                    "chatgpt_account_id": "acct_a_stale_refreshed",
                                    "chatgpt_plan_type": "pro"
                                }
                            }));
                            axum::Json(json!({
                                "access_token": jwt,
                                "refresh_token": format!("{rt}_rotated"),
                                "expires_in": 7200
                            }))
                        }
                        "authorization_code" => {
                            let jwt = make_test_jwt(&json!({
                                "exp": (current_utc_epoch_ms() / 1000) + 7200,
                                "https://api.openai.com/auth": {
                                    "chatgpt_account_id": "acct_b_new",
                                    "chatgpt_plan_type": "plus"
                                }
                            }));
                            axum::Json(json!({
                                "access_token": jwt,
                                "refresh_token": "rt_acct_b_new",
                                "expires_in": 7200
                            }))
                        }
                        other => panic!("unexpected grant_type: {other}"),
                    }
                }
            }),
        );
        let (issuer, server_handle) = spawn_mock_oauth_server(router).await;

        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("config.json");
        let store = Arc::new(FileAuthStore::for_config_path(&cfg_path).expect("store"));
        let external_store =
            Arc::new(FileAuthStore::for_config_path(&cfg_path).expect("external store"));
        let manager = CodexAuthManager::with_options(
            store.clone(),
            CodexAuthOptions {
                oauth_issuer: issuer,
                callback_bind_port: 0,
                login_ttl: Duration::from_secs(30),
                refresh_margin: Duration::from_secs(300),
                request_timeout: Duration::from_secs(10),
            },
        )
        .expect("manager");

        let make_acct_a = |access: &str, refresh: &str| CodexCredentials {
            schema_version: AUTH_SCHEMA_VERSION,
            auth_mode: CodexAuthMode::ChatGptOAuth,
            access_token: SecretString::new(access),
            refresh_token: SecretString::new(refresh),
            id_token: None,
            account_id: "acct_a_initial".to_string(),
            expires_at_ms: current_utc_epoch_ms() + 3_600_000,
            plan_type: Some("pro".to_string()),
            email_hint: Some("a***@example.com".to_string()),
            last_refresh_utc: "2026-11-26T22:00:00Z".to_string(),
        };

        // (a) disconnect() called mid-refresh returns promptly without deadlock,
        //     valid_credentials(true) returns NotAuthenticated, and auth/codex.json stays None.
        store
            .save_codex(&make_acct_a("tok_a_disc", "rt_acct_a_disc"))
            .unwrap();
        let mgr_a = manager.clone();
        let refresh_a = tokio::spawn(async move { mgr_a.valid_credentials(true).await });

        tokio::time::timeout(Duration::from_secs(2), disc_arrived.notified())
            .await
            .expect("refresh (a) must reach /oauth/token");

        tokio::time::timeout(Duration::from_secs(2), manager.disconnect())
            .await
            .expect("disconnect() must not deadlock while refresh is in-flight")
            .expect("disconnect() succeeds");
        assert_eq!(store.load_codex().unwrap(), None);

        disc_release.notify_one();
        let res_a = refresh_a.await.expect("refresh (a) join");
        assert!(
            matches!(res_a, Err(ProviderError::NotAuthenticated(_))),
            "stale refresh after disconnect() must fail with NotAuthenticated, got {res_a:?}"
        );
        assert_eq!(
            store.load_codex().unwrap(),
            None,
            "stale refresh must never resurrect auth/codex.json after disconnect()"
        );
        assert!(!store.codex_path().exists());

        // (b) disconnect() + signing into Account B via complete_login_manual mid-Account-A-refresh
        //     leaves store holding Account B while Account A's stale refresh returns NotAuthenticated.
        store
            .save_codex(&make_acct_a("tok_a_switch", "rt_acct_a_switch"))
            .unwrap();
        let mgr_b = manager.clone();
        let refresh_b = tokio::spawn(async move { mgr_b.valid_credentials(true).await });

        tokio::time::timeout(Duration::from_secs(2), switch_arrived.notified())
            .await
            .expect("refresh (b) must reach /oauth/token");

        tokio::time::timeout(Duration::from_secs(2), manager.disconnect())
            .await
            .expect("disconnect() in (b) must not deadlock")
            .expect("disconnect() succeeds");
        let login_b = manager.start_login().await.expect("start Account B login");
        let state_b = parse_query_map(&login_b.authorize_url)
            .remove("state")
            .unwrap();
        let creds_b = manager
            .complete_login_manual(&format!("code_acct_b#{state_b}"))
            .await
            .expect("complete Account B manual login");
        assert_eq!(creds_b.account_id, "acct_b_new");
        assert_eq!(
            store.load_codex().unwrap().unwrap().account_id,
            "acct_b_new"
        );

        switch_release.notify_one();
        let res_b = refresh_b.await.expect("refresh (b) join");
        assert!(
            matches!(res_b, Err(ProviderError::NotAuthenticated(_))),
            "stale Account A refresh after Account B sign-in must return NotAuthenticated, got {res_b:?}"
        );
        let stored_after_b = store
            .load_codex()
            .unwrap()
            .expect("Account B credentials must remain persisted");
        assert_eq!(
            stored_after_b.account_id, "acct_b_new",
            "stale Account A refresh must never clobber Account B"
        );
        assert_eq!(stored_after_b.refresh_token.expose(), "rt_acct_b_new");

        // (c) Cross-process disconnect (external_store.clear_codex() without touching manager's
        //     in-process disconnect_epoch) mid-refresh is also caught by pre-save load_codex() check.
        store
            .save_codex(&make_acct_a("tok_a_xproc", "rt_acct_a_xproc"))
            .unwrap();
        let mgr_c = manager.clone();
        let refresh_c = tokio::spawn(async move { mgr_c.valid_credentials(true).await });

        tokio::time::timeout(Duration::from_secs(2), xproc_arrived.notified())
            .await
            .expect("refresh (c) must reach /oauth/token");

        external_store.clear_codex().expect("cross-process clear");
        xproc_release.notify_one();
        let res_c = refresh_c.await.expect("refresh (c) join");
        assert!(
            matches!(res_c, Err(ProviderError::NotAuthenticated(_))),
            "stale refresh after cross-process clear must return NotAuthenticated, got {res_c:?}"
        );
        assert_eq!(
            store.load_codex().unwrap(),
            None,
            "stale refresh must never resurrect auth/codex.json after cross-process clear"
        );

        server_handle.abort();
    }
}
