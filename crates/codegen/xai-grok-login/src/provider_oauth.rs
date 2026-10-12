//! Built-in OAuth2/OIDC login for catalog model providers.
//!
//! Coverage mirrors pi's `packages/ai/src/auth/oauth`: `openai-codex`,
//! `anthropic`, `github-copilot`, `kimi-coding`, `meta`, and `openrouter`.
//! (xAI itself is covered by the Grok.com login; `radius` is enterprise-only
//! and intentionally not ported.)
//!
//! Two flow shapes:
//! - `PkceLoopback` — localhost HTTP callback + PKCE, with a manual-paste
//!   fallback for remote/headless setups (`deliver_manual_input`).
//! - `DeviceCode` — RFC 8628 device authorization; includes the OpenAI Codex
//!   JSON variant (deviceauth endpoints) and standard form-polling servers.
//!
//! Credentials persist in `<grok_home>/provider-auth.json` (separate from
//! `auth.json`, which is xAI-shaped). Provider models consume them through the
//! `[model_providers.<id>.auth]`/`[auth_provider]` machinery: `oauth = "<id>"`
//! selects [`mint`] as the token source. `openrouter` and `meta` mint
//! long-lived or renewable API keys; openrouter stores the key directly on the
//! provider (`api_key`), meta keeps the identity token here and re-mints on
//! expiry via [`mint`].

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Mutex, OnceLock};
use tokio::sync::mpsc;

/// Poll loop cancels/hangs bound: OAuth device codes typically live 15 min.
const LOGIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15 * 60);
/// Single HTTP request bound inside a flow (pi uses 30s).
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// RFC 8628 §3.2 default poll interval when the server omits `interval`.
const DEFAULT_POLL_INTERVAL_SECS: u64 = 5;
const SLOW_DOWN_INCREMENT_SECS: u64 = 5;
/// Minimum poll interval; a malicious/buggy `interval=0` must not busy-loop.
const MIN_POLL_INTERVAL_SECS: u64 = 1;

// ---------------------------------------------------------------------------
// Spec table
// ---------------------------------------------------------------------------

/// How the provider's OAuth dance begins.
#[derive(Debug, Clone, Copy)]
pub enum OAuthFlow {
    /// Browser authorize URL + local loopback listener + PKCE.
    PkceLoopback(&'static PkceSpec),
    /// RFC 8628 device authorization (or the Codex JSON variant).
    DeviceCode(&'static DeviceSpec),
}

#[derive(Debug, Clone, Copy)]
pub struct DeviceSpec {
    pub device_code_url: &'static str,
    pub token_url: &'static str,
    pub client_id: &'static str,
    /// `scope` in the device-authorization request (GitHub only today).
    pub scope: Option<&'static str>,
    pub style: DeviceStyle,
    /// `verification_uri` must match this suffix, else the response is
    /// untrusted (pi checks scheme; we additionally pin the host suffix).
    pub expected_verify_host: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceStyle {
    /// Form-encoded request and poll; errors arrive as `{error, interval?}`.
    /// GitHub (`/login/device/code`, `/login/oauth/access_token`).
    Github,
    /// Form request, JSON poll shape — Kimi (`/api/oauth/*`) and Meta
    /// (`/oidc/device/*`) both poll a form body against a JSON response.
    Rfc8628Json,
    /// OpenAI Codex device auth: JSON `{client_id}` → `{device_auth_id,
    /// user_code, interval}`, JSON poll → `{authorization_code,
    /// code_verifier}` which then goes through the normal PKCE exchange with
    /// `DEVICE_REDIRECT_URI`.
    OpenAiCodex,
}

#[derive(Debug, Clone, Copy)]
pub struct PkceSpec {
    pub authorize_url: &'static str,
    pub token_url: &'static str,
    /// `None` for OpenRouter, whose PKCE flow has no client id.
    pub client_id: Option<&'static str>,
    pub scope: Option<&'static str>,
    /// Fixed port (`Some`) or ephemeral (`None`, OpenRouter-style).
    pub callback_port: Option<u16>,
    pub callback_path: &'static str,
    /// Extra authorize query pairs (e.g. Codex's simplified-flow flags).
    pub extra_authorize_params: &'static [(&'static str, &'static str)],
    pub authorize_style: AuthorizeStyle,
    pub exchange_style: ExchangeStyle,
    /// Anthropic uses the PKCE verifier itself as `state`.
    pub state_is_verifier: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorizeStyle {
    /// `response_type=code&client_id&redirect_uri&scope&code_challenge&...&state`
    Standard,
    /// Anthropic: `code=true` plus standard params.
    Anthropic,
    /// OpenRouter: `callback_url&code_challenge&code_challenge_method` only.
    OpenRouter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExchangeStyle {
    /// Form-encoded `authorization_code` grant → `{access_token, refresh_token, expires_in}`.
    StandardForm,
    /// JSON `{grant_type, client_id, code, state, redirect_uri, code_verifier}`.
    AnthropicJson,
    /// JSON `{code, code_verifier, code_challenge_method}` → `{key}` (permanent API key).
    OpenRouterKey,
}

/// How a stored credential produces a fresh bearer for requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshStyle {
    /// `grant_type=refresh_token` form post.
    StandardForm,
    /// `grant_type=refresh_token` JSON post (Anthropic).
    AnthropicJson,
    /// GET `{token_url}` with `Bearer <identity>` → `{token, expires_at}`
    /// (GitHub Copilot session token).
    CopilotSession,
    /// POST `{token_url}` JSON `{access_token: <identity>}` → `{api_key}`
    /// (Meta Muse key mint; identity itself is not renewable).
    MetaKey,
    /// The credential IS a long-lived key; no slot mint needed (OpenRouter).
    ApiKey,
}

pub struct OAuthSpec {
    /// Provider id in the catalog / `provider-auth.json` key.
    pub id: &'static str,
    pub display_name: &'static str,
    /// Primary flow.
    pub flow: OAuthFlow,
    /// Optional secondary device flow (Codex offers browser OR device).
    pub device_flow: Option<&'static DeviceSpec>,
    pub refresh: RefreshStyle,
    /// URL hit by `RefreshStyle::{CopilotSession,MetaKey}` mints.
    pub mint_url: Option<&'static str>,
    /// OAuth tokens are Bearer credentials — the login writes
    /// `auth_scheme = "bearer"` so the minted token does not go through the
    /// provider's default `x-api-key` header (Anthropic, Kimi).
    pub bearer: bool,
}

const CODEX_DEVICE: DeviceSpec = DeviceSpec {
    device_code_url: "https://auth.openai.com/api/accounts/deviceauth/usercode",
    token_url: "https://auth.openai.com/api/accounts/deviceauth/token",
    client_id: "app_EMoamEEZ73f0CkXaXp7hrann",
    scope: None,
    style: DeviceStyle::OpenAiCodex,
    expected_verify_host: Some("openai.com"),
};

const SPECS: &[OAuthSpec] = &[
    OAuthSpec {
        id: "openai-codex",
        display_name: "OpenAI Codex (ChatGPT Plus/Pro)",
        flow: OAuthFlow::PkceLoopback(&PkceSpec {
            authorize_url: "https://auth.openai.com/oauth/authorize",
            token_url: "https://auth.openai.com/oauth/token",
            client_id: Some("app_EMoamEEZ73f0CkXaXp7hrann"),
            scope: Some("openid profile email offline_access"),
            callback_port: Some(1455),
            callback_path: "/auth/callback",
            extra_authorize_params: &[
                ("id_token_add_organizations", "true"),
                ("codex_cli_simplified_flow", "true"),
                ("originator", "grok"),
            ],
            authorize_style: AuthorizeStyle::Standard,
            exchange_style: ExchangeStyle::StandardForm,
            state_is_verifier: false,
        }),
        device_flow: Some(&CODEX_DEVICE),
        refresh: RefreshStyle::StandardForm,
        mint_url: None,
        bearer: false,
    },
    OAuthSpec {
        id: "anthropic",
        display_name: "Anthropic (Claude Pro/Max)",
        flow: OAuthFlow::PkceLoopback(&PkceSpec {
            authorize_url: "https://claude.ai/oauth/authorize",
            token_url: "https://platform.claude.com/v1/oauth/token",
            client_id: Some("9d1c250a-e61b-44d9-88ed-5944d1962f5e"),
            scope: Some(
                "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload",
            ),
            callback_port: Some(53692),
            callback_path: "/callback",
            extra_authorize_params: &[],
            authorize_style: AuthorizeStyle::Anthropic,
            exchange_style: ExchangeStyle::AnthropicJson,
            state_is_verifier: true,
        }),
        device_flow: None,
        refresh: RefreshStyle::AnthropicJson,
        mint_url: None,
        bearer: true,
    },
    OAuthSpec {
        id: "github-copilot",
        display_name: "GitHub Copilot",
        flow: OAuthFlow::DeviceCode(&DeviceSpec {
            device_code_url: "https://github.com/login/device/code",
            token_url: "https://github.com/login/oauth/access_token",
            client_id: "Iv1.b507a08c87ecfe98",
            scope: Some("read:user"),
            style: DeviceStyle::Github,
            expected_verify_host: Some("github.com"),
        }),
        device_flow: None,
        refresh: RefreshStyle::CopilotSession,
        mint_url: Some("https://api.github.com/copilot_internal/v2/token"),
        bearer: false,
    },
    OAuthSpec {
        id: "kimi-coding",
        display_name: "Kimi For Coding",
        flow: OAuthFlow::DeviceCode(&DeviceSpec {
            device_code_url: "https://auth.kimi.com/api/oauth/device_authorization",
            token_url: "https://auth.kimi.com/api/oauth/token",
            client_id: "17e5f671-d194-4dfb-9706-5516cb48c098",
            scope: None,
            style: DeviceStyle::Rfc8628Json,
            expected_verify_host: Some("kimi.com"),
        }),
        device_flow: None,
        refresh: RefreshStyle::StandardForm,
        mint_url: None,
        bearer: true,
    },
    OAuthSpec {
        id: "meta",
        display_name: "Meta (Muse)",
        flow: OAuthFlow::DeviceCode(&DeviceSpec {
            device_code_url: "https://auth.meta.com/oidc/device/authorization/",
            token_url: "https://auth.meta.com/oidc/device/token/",
            client_id: "1031625952748946",
            scope: None,
            style: DeviceStyle::Rfc8628Json,
            expected_verify_host: Some("meta.com"),
        }),
        device_flow: None,
        refresh: RefreshStyle::MetaKey,
        mint_url: Some("https://api.meta.ai/muse-code/key"),
        bearer: false,
    },
    OAuthSpec {
        id: "openrouter",
        display_name: "OpenRouter",
        flow: OAuthFlow::PkceLoopback(&PkceSpec {
            authorize_url: "https://openrouter.ai/auth",
            token_url: "https://openrouter.ai/api/v1/auth/keys",
            client_id: None,
            scope: None,
            callback_port: None,
            callback_path: "/callback",
            extra_authorize_params: &[],
            authorize_style: AuthorizeStyle::OpenRouter,
            exchange_style: ExchangeStyle::OpenRouterKey,
            state_is_verifier: false,
        }),
        device_flow: None,
        refresh: RefreshStyle::ApiKey,
        mint_url: None,
        bearer: false,
    },
];

/// The OAuth spec for a provider id (None when the provider has no built-in flow).
pub fn spec_for(provider_id: &str) -> Option<&'static OAuthSpec> {
    SPECS.iter().find(|s| s.id == provider_id)
}

pub fn oauth_capable(provider_id: &str) -> bool {
    spec_for(provider_id).is_some()
}

/// All ids with a built-in OAuth flow (for error/usage text).
pub fn oauth_ids() -> Vec<&'static str> {
    SPECS.iter().map(|s| s.id).collect()
}

// ---------------------------------------------------------------------------
// Credential store — <grok_home>/provider-auth.json
// ---------------------------------------------------------------------------

fn creds_path() -> std::path::PathBuf {
    xai_grok_shell_base::util::grok_home::grok_home().join("provider-auth.json")
}

fn creds_lock_path() -> std::path::PathBuf {
    xai_grok_shell_base::util::grok_home::grok_home().join("provider-auth.lock")
}

/// One provider's stored credential. Which fields are set depends on the
/// spec's refresh style:
/// - StandardForm/AnthropicJson: `access_token` + `refresh_token` + `expires_at`.
/// - CopilotSession/MetaKey: `access_token` = durable identity; minted
///   session token / API key goes to `api_key` + `expires_at`.
/// - ApiKey (OpenRouter): `api_key` only; never enters the slot machinery.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct StoredCredential {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Codex `chatgpt-account-id` claim, captured at login.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct CredentialStore {
    #[serde(flatten)]
    providers: HashMap<String, StoredCredential>,
}

/// Locked read-modify-write across the whole file, mirroring auth.json's
/// `AuthFileLock` discipline (fs2 exclusive lock on a sibling .lock file).
fn with_credential_store<R>(f: impl FnOnce(&mut CredentialStore) -> Result<R>) -> Result<R> {
    use fs2::FileExt as _;
    let dir = xai_grok_shell_base::util::grok_home::grok_home();
    std::fs::create_dir_all(&dir).context("create grok home for provider-auth.json")?;
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(creds_lock_path())
        .context("open provider-auth.lock")?;
    lock_file
        .lock_exclusive()
        .context("lock provider-auth.lock")?;
    let result = (|| {
        let path = creds_path();
        let mut store: CredentialStore = match std::fs::read_to_string(&path) {
            Ok(text) => {
                serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => CredentialStore::default(),
            Err(e) => return Err(e).context("read provider-auth.json"),
        };
        let out = f(&mut store)?;
        let text = serde_json::to_string_pretty(&store)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text).context("write provider-auth.json.tmp")?;
        std::fs::rename(&tmp, &path).context("rename provider-auth.json")?;
        Ok(out)
    })();
    let _ = lock_file.unlock();
    result
}

/// Stored credential for `provider_id`, if a login completed before.
pub fn load_credentials(provider_id: &str) -> Option<StoredCredential> {
    let path = creds_path();
    let text = std::fs::read_to_string(path).ok()?;
    let store: CredentialStore = serde_json::from_str(&text).ok()?;
    store.providers.get(provider_id).cloned()
}

/// Persist (or overwrite) the credential for `provider_id`.
pub fn save_credentials(provider_id: &str, creds: StoredCredential) -> Result<()> {
    with_credential_store(|store| {
        store.providers.insert(provider_id.to_owned(), creds);
        Ok(())
    })
}

/// Drop the stored credential (logout).
pub fn clear_credentials(provider_id: &str) -> Result<()> {
    with_credential_store(|store| {
        store.providers.remove(provider_id);
        Ok(())
    })
}

// ---------------------------------------------------------------------------
// Notices to the UI (device code / authorize URL)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum LoginNotice {
    /// Device flow: show the URL + code; poll runs in the background.
    DeviceCode {
        verification_uri: String,
        verification_uri_complete: Option<String>,
        user_code: String,
        expires_secs: u64,
    },
    /// Browser flow: authorize URL opened/shown; paste fallback active.
    AuthUrl {
        url: String,
        /// Localhost callback address, for the paste hint.
        callback_hint: String,
    },
}

/// Sink the flow reports through (session layer renders it as agent output).
/// `?Send`: a slash-command UI sink may borrow `RefCell`-carrying state.
#[async_trait::async_trait(?Send)]
pub trait LoginUi {
    async fn notice(&mut self, notice: LoginNotice);
}

/// Which flow to start when a provider offers both (Codex).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LoginMethod {
    #[default]
    Preferred,
    Browser,
    Device,
}

// ---------------------------------------------------------------------------
// Manual paste delivery: `/provider oauth <id> code <input>`
// ---------------------------------------------------------------------------

type PendingMap = HashMap<String, mpsc::UnboundedSender<String>>;

static PENDING_LOGINS: OnceLock<Mutex<PendingMap>> = OnceLock::new();

fn pending() -> &'static Mutex<PendingMap> {
    PENDING_LOGINS.get_or_init(Default::default)
}

/// Feed a manually pasted code / redirect URL into an in-flight login.
/// Called by the `/provider oauth <id> code <input>` slash path.
pub fn deliver_manual_input(provider_id: &str, input: String) -> Result<()> {
    let tx = pending()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(provider_id)
        .cloned();
    match tx {
        Some(tx) => tx
            .send(input)
            .map_err(|_| anyhow::anyhow!("login for {provider_id} already finished")),
        None => bail!("no {provider_id} login is waiting for a pasted code"),
    }
}

fn register_pending(provider_id: &str) -> mpsc::UnboundedReceiver<String> {
    let (tx, rx) = mpsc::unbounded_channel();
    pending()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(provider_id.to_owned(), tx);
    rx
}

fn unregister_pending(provider_id: &str) {
    pending()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(provider_id);
}

/// Parse pasted input into `{code, state}` — accepts a raw code, a full
/// redirect URL, `code=...&state=...`, or the `code#state` split pi supports.
fn parse_pasted_input(input: &str) -> (Option<String>, Option<String>) {
    let value = input.trim();
    if value.is_empty() {
        return (None, None);
    }
    if let Ok(url) = url::Url::parse(value) {
        let code = url
            .query_pairs()
            .find(|(k, _)| k == "code")
            .map(|(_, v)| v.into_owned());
        let state = url
            .query_pairs()
            .find(|(k, _)| k == "state")
            .map(|(_, v)| v.into_owned());
        if code.is_some() {
            return (code, state);
        }
    }
    if let Some((code, state)) = value.split_once('#') {
        return (Some(code.to_string()), Some(state.to_string()));
    }
    if value.contains("code=") {
        let pairs: HashMap<String, String> = url::form_urlencoded::parse(value.as_bytes())
            .into_owned()
            .collect();
        return (pairs.get("code").cloned(), pairs.get("state").cloned());
    }
    (Some(value.to_string()), None)
}

// ---------------------------------------------------------------------------
// PKCE + loopback listener
// ---------------------------------------------------------------------------

fn pkce_pair() -> (String, String) {
    use base64::Engine as _;
    use sha2::Digest as _;
    let bytes: [u8; 32] = rand::random();
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(sha2::Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

fn random_state() -> String {
    let bytes: [u8; 16] = rand::random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Build the authorize URL per spec.
fn authorize_url(spec: &PkceSpec, challenge: &str, state: &str, redirect_uri: &str) -> String {
    let mut url = url::Url::parse(spec.authorize_url).expect("static authorize URL");
    {
        let mut q = url.query_pairs_mut();
        match spec.authorize_style {
            AuthorizeStyle::Standard => {
                q.append_pair("response_type", "code");
                if let Some(client_id) = spec.client_id {
                    q.append_pair("client_id", client_id);
                }
                q.append_pair("redirect_uri", redirect_uri);
                if let Some(scope) = spec.scope {
                    q.append_pair("scope", scope);
                }
                q.append_pair("code_challenge", challenge);
                q.append_pair("code_challenge_method", "S256");
                q.append_pair("state", state);
            }
            AuthorizeStyle::Anthropic => {
                q.append_pair("code", "true");
                if let Some(client_id) = spec.client_id {
                    q.append_pair("client_id", client_id);
                }
                q.append_pair("response_type", "code");
                q.append_pair("redirect_uri", redirect_uri);
                if let Some(scope) = spec.scope {
                    q.append_pair("scope", scope);
                }
                q.append_pair("code_challenge", challenge);
                q.append_pair("code_challenge_method", "S256");
                q.append_pair("state", state);
            }
            AuthorizeStyle::OpenRouter => {
                q.append_pair("callback_url", redirect_uri);
                q.append_pair("code_challenge", challenge);
                q.append_pair("code_challenge_method", "S256");
            }
        }
        for (k, v) in spec.extra_authorize_params {
            q.append_pair(k, v);
        }
    }
    url.to_string()
}

const SUCCESS_PAGE: &str =
    "<!doctype html><html><body><h2>Login complete — you can close this tab.</h2></body></html>";
const ERROR_PAGE: &str = "<!doctype html><html><body><h2>Login failed.</h2></body></html>";

/// Accept loop on one listener until a request carrying `code` arrives that
/// matches `expected_state` (None = ignore state). Returns the code.
async fn wait_for_callback(
    listener: tokio::net::TcpListener,
    path: &str,
    expected_state: Option<&str>,
    mut manual: mpsc::UnboundedReceiver<String>,
) -> Result<String> {
    let timeout = tokio::time::sleep(LOGIN_TIMEOUT);
    tokio::pin!(timeout);
    loop {
        tokio::select! {
            _ = &mut timeout => bail!("login timed out waiting for the browser callback"),
            pasted = manual.recv() => {
                let Some(input) = pasted else { continue };
                let (code, state) = parse_pasted_input(&input);
                if let Some(expected) = expected_state
                    && let Some(got) = state.as_deref()
                    && got != expected
                {
                    bail!("pasted redirect URL has a mismatched state parameter");
                }
                if let Some(code) = code {
                    return Ok(code);
                }
                bail!("pasted input did not contain an authorization code");
            }
            accepted = listener.accept() => {
                let (mut stream, _) = accepted.context("callback accept failed")?;
                use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
                let mut buf = vec![0u8; 8192];
                let n = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    stream.read(&mut buf),
                )
                .await
                .context("callback read timed out")??;
                if n == 0 {
                    continue;
                }
                let request = String::from_utf8_lossy(buf.get(..n).unwrap_or(&[]));
                let request_target = request
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/");
                let parsed = url::Url::parse(&format!("http://localhost{request_target}"))
                    .unwrap_or_else(|_| url::Url::parse("http://localhost/").expect("fallback URL"));
                let is_callback = parsed.path() == path;
                let pairs: HashMap<String, String> = parsed
                    .query_pairs()
                    .into_owned()
                    .collect();
                let code = pairs.get("code").cloned();
                let state_ok = match expected_state {
                    Some(expected) => pairs.get("state").map(String::as_str) == Some(expected),
                    None => true,
                };
                let (status, body) = if !is_callback {
                    ("404 Not Found", ERROR_PAGE)
                } else if code.is_some() && state_ok {
                    ("200 OK", SUCCESS_PAGE)
                } else {
                    ("400 Bad Request", ERROR_PAGE)
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
                if is_callback && state_ok
                    && let Some(code) = code
                {
                    return Ok(code);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Token exchange + refresh
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct StandardTokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
}

fn expires_from_now(expires_in: Option<i64>) -> Option<chrono::DateTime<chrono::Utc>> {
    expires_in.map(|s| chrono::Utc::now() + chrono::Duration::seconds(s))
}

fn client() -> reqwest::Client {
    xai_grok_http::shared_client()
}

/// Exchange `code` for tokens per the spec's exchange style.
async fn exchange_code(
    spec: &PkceSpec,
    code: &str,
    verifier: &str,
    state: Option<&str>,
    redirect_uri: &str,
) -> Result<StoredCredential> {
    match spec.exchange_style {
        ExchangeStyle::StandardForm => {
            let mut form: Vec<(&str, &str)> = vec![
                ("grant_type", "authorization_code"),
                ("code", code),
                ("code_verifier", verifier),
                ("redirect_uri", redirect_uri),
            ];
            if let Some(client_id) = spec.client_id {
                form.push(("client_id", client_id));
            }
            let resp = client()
                .post(spec.token_url)
                .form(&form)
                .timeout(REQUEST_TIMEOUT)
                .send()
                .await
                .context("token exchange request")?;
            let creds = standard_token_from_response(resp, "token exchange").await?;
            Ok(creds)
        }
        ExchangeStyle::AnthropicJson => {
            let body = serde_json::json!({
                "grant_type": "authorization_code",
                "client_id": spec.client_id.unwrap_or_default(),
                "code": code,
                "state": state.unwrap_or_default(),
                "redirect_uri": redirect_uri,
                "code_verifier": verifier,
            });
            let resp = client()
                .post(spec.token_url)
                .json(&body)
                .timeout(REQUEST_TIMEOUT)
                .send()
                .await
                .context("token exchange request")?;
            standard_token_from_response(resp, "token exchange").await
        }
        ExchangeStyle::OpenRouterKey => {
            let body = serde_json::json!({
                "code": code,
                "code_verifier": verifier,
                "code_challenge_method": "S256",
            });
            let resp = client()
                .post(spec.token_url)
                .json(&body)
                .timeout(REQUEST_TIMEOUT)
                .send()
                .await
                .context("OpenRouter key exchange")?;
            let status = resp.status();
            let json: serde_json::Value = resp
                .json()
                .await
                .context("OpenRouter key exchange response")?;
            let key = json.get("key").and_then(|k| k.as_str()).map(str::to_owned);
            match (status.is_success(), key) {
                (true, Some(key)) if !key.is_empty() => Ok(StoredCredential {
                    api_key: Some(key),
                    ..Default::default()
                }),
                _ => bail!("OpenRouter key exchange failed (HTTP {status}): {json}"),
            }
        }
    }
}

async fn standard_token_from_response(
    resp: reqwest::Response,
    op: &str,
) -> Result<StoredCredential> {
    let status = resp.status();
    let json: StandardTokenResponse = resp
        .json()
        .await
        .with_context(|| format!("{op} returned non-JSON"))?;
    let Some(access) = json.access_token.filter(|a| !a.is_empty()) else {
        bail!("{op} failed (HTTP {status}): no access_token in response");
    };
    Ok(StoredCredential {
        access_token: Some(access),
        refresh_token: json.refresh_token,
        expires_at: expires_from_now(json.expires_in),
        ..Default::default()
    })
}

/// Decode a JWT payload (no signature check — the token arrived over direct
/// HTTPS and is only mined for claims).
fn jwt_claim(token: &str, path: &str, key: &str) -> Option<String> {
    use base64::Engine as _;
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload))
        .ok()?;
    let json: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    json.get(path)?.get(key)?.as_str().map(str::to_owned)
}

// ---------------------------------------------------------------------------
// Device-code flows
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct DeviceAuthorization {
    device_code: Option<String>,
    user_code: Option<String>,
    verification_uri: Option<String>,
    verification_uri_complete: Option<String>,
    interval: Option<serde_json::Value>,
    expires_in: Option<i64>,
}

fn parse_interval(v: Option<&serde_json::Value>) -> Option<u64> {
    match v? {
        serde_json::Value::Number(n) => n.as_u64(),
        serde_json::Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn trusted_verify_uri(uri: &str, expected_host: Option<&'static str>) -> bool {
    let Ok(url) = url::Url::parse(uri) else {
        return false;
    };
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    match (expected_host, url.host_str()) {
        (Some(expected), Some(host)) => host == expected || host.ends_with(&format!(".{expected}")),
        (None, _) => true,
        (Some(_), None) => false,
    }
}

async fn run_device_flow(
    provider_id: &str,
    spec: &DeviceSpec,
    parent: &'static OAuthSpec,
    ui: &mut dyn LoginUi,
) -> Result<StoredCredential> {
    if spec.style == DeviceStyle::OpenAiCodex {
        return run_codex_device_flow(provider_id, spec, parent, ui).await;
    }
    let mut body: Vec<(&str, &str)> = vec![("client_id", spec.client_id)];
    if let Some(scope) = spec.scope {
        body.push(("scope", scope));
    }
    let resp = client()
        .post(spec.device_code_url)
        .header("accept", "application/json")
        .form(&body)
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .context("device authorization request")?;
    let status = resp.status();
    let auth: DeviceAuthorization = resp
        .json()
        .await
        .context("device authorization returned non-JSON")?;
    let device_code = auth.device_code.filter(|c| !c.is_empty());
    let user_code = auth.user_code.filter(|c| !c.is_empty());
    let verification_uri = auth.verification_uri.filter(|u| !u.is_empty());
    let (Some(device_code), Some(user_code), Some(verification_uri)) =
        (device_code, user_code, verification_uri)
    else {
        bail!("device authorization failed (HTTP {status})");
    };
    for uri in [
        Some(&verification_uri),
        auth.verification_uri_complete.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        if !trusted_verify_uri(uri, spec.expected_verify_host) {
            bail!("untrusted verification_uri in device authorization response: {uri}");
        }
    }
    ui.notice(LoginNotice::DeviceCode {
        verification_uri: verification_uri.clone(),
        verification_uri_complete: auth.verification_uri_complete.clone(),
        user_code,
        expires_secs: auth.expires_in.unwrap_or(900) as u64,
    })
    .await;
    let open_url = auth
        .verification_uri_complete
        .clone()
        .unwrap_or(verification_uri);
    let _ = open_browser_detached(&open_url).await;

    let mut interval_secs = parse_interval(auth.interval.as_ref())
        .unwrap_or(DEFAULT_POLL_INTERVAL_SECS)
        .max(MIN_POLL_INTERVAL_SECS);
    let deadline = std::time::Instant::now() + LOGIN_TIMEOUT;
    loop {
        if std::time::Instant::now() >= deadline {
            bail!("device login timed out");
        }
        tokio::time::sleep(std::time::Duration::from_secs(interval_secs)).await;
        let resp = client()
            .post(spec.token_url)
            .header("accept", "application/json")
            .form(&[
                ("client_id", spec.client_id),
                ("device_code", device_code.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await;
        let Ok(resp) = resp else { continue };
        let status = resp.status();
        let json: serde_json::Value = resp.json().await.unwrap_or_default();
        if status.is_success() {
            if let Some(access) = json
                .get("access_token")
                .and_then(|t| t.as_str())
                .filter(|t| !t.is_empty())
            {
                return finish_device_login(
                    spec,
                    StoredCredential {
                        access_token: Some(access.to_owned()),
                        refresh_token: json
                            .get("refresh_token")
                            .and_then(|t| t.as_str())
                            .map(str::to_owned),
                        expires_at: expires_from_now(
                            json.get("expires_in").and_then(|e| e.as_i64()),
                        ),
                        ..Default::default()
                    },
                )
                .await;
            }
            bail!("device token response missing access_token");
        }
        let error = json.get("error").and_then(|e| e.as_str()).unwrap_or("");
        match error {
            "authorization_pending" | "deviceauth_authorization_pending" => {}
            "slow_down" => {
                interval_secs = parse_interval(json.get("interval"))
                    .map(|i| i.max(MIN_POLL_INTERVAL_SECS))
                    .unwrap_or(interval_secs + SLOW_DOWN_INCREMENT_SECS);
            }
            "expired_token" | "expired_device_code" => {
                bail!("device code expired — restart login");
            }
            "access_denied" => bail!("device login was denied"),
            other if other.is_empty() && status.is_server_error() => {}
            other => {
                let desc = json
                    .get("error_description")
                    .or_else(|| json.get("detail"))
                    .or_else(|| json.get("message"))
                    .and_then(|d| d.as_str())
                    .unwrap_or(other);
                bail!("device login failed: {desc}");
            }
        }
    }
}

/// Device specs whose stored value is not itself a request token
/// (Copilot stores the GitHub OAuth token; Meta the identity token) still land
/// in `access_token`; per-mint exchange happens in [`mint`].
async fn finish_device_login(
    _spec: &DeviceSpec,
    creds: StoredCredential,
) -> Result<StoredCredential> {
    Ok(creds)
}

/// Codex device variant: JSON endpoints return an authorization code +
/// code_verifier which then complete the standard PKCE exchange against
/// `DEVICE_REDIRECT_URI` (pi parity).
async fn run_codex_device_flow(
    provider_id: &str,
    spec: &DeviceSpec,
    parent: &'static OAuthSpec,
    ui: &mut dyn LoginUi,
) -> Result<StoredCredential> {
    const DEVICE_VERIFICATION_URI: &str = "https://auth.openai.com/codex/device";
    const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
    let _ = provider_id;
    let resp = client()
        .post(spec.device_code_url)
        .json(&serde_json::json!({ "client_id": spec.client_id }))
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .context("codex device authorization request")?;
    let status = resp.status();
    let json: serde_json::Value = resp.json().await.unwrap_or_default();
    let device_auth_id = json.get("device_auth_id").and_then(|v| v.as_str());
    let user_code = json.get("user_code").and_then(|v| v.as_str());
    let interval = parse_interval(json.get("interval")).unwrap_or(DEFAULT_POLL_INTERVAL_SECS);
    let (Some(device_auth_id), Some(user_code)) = (device_auth_id, user_code) else {
        bail!("codex device authorization failed (HTTP {status})");
    };
    ui.notice(LoginNotice::DeviceCode {
        verification_uri: DEVICE_VERIFICATION_URI.to_owned(),
        verification_uri_complete: None,
        user_code: user_code.to_owned(),
        expires_secs: 15 * 60,
    })
    .await;
    let _ = open_browser_detached(DEVICE_VERIFICATION_URI).await;

    let mut interval_secs = interval.max(MIN_POLL_INTERVAL_SECS);
    let deadline = std::time::Instant::now() + LOGIN_TIMEOUT;
    let (authorization_code, code_verifier) = loop {
        if std::time::Instant::now() >= deadline {
            bail!("codex device login timed out");
        }
        tokio::time::sleep(std::time::Duration::from_secs(interval_secs)).await;
        let resp = client()
            .post(spec.token_url)
            .json(&serde_json::json!({
                "device_auth_id": device_auth_id,
                "user_code": user_code,
            }))
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await;
        let Ok(resp) = resp else { continue };
        if resp.status().is_success() {
            let json: serde_json::Value = resp.json().await.unwrap_or_default();
            let code = json.get("authorization_code").and_then(|v| v.as_str());
            let verifier = json.get("code_verifier").and_then(|v| v.as_str());
            if let (Some(code), Some(verifier)) = (code, verifier) {
                break (code.to_owned(), verifier.to_owned());
            }
            bail!("codex device token response missing fields");
        }
        match resp.status().as_u16() {
            403 | 404 => {}
            _ => {
                let json: serde_json::Value = resp.json().await.unwrap_or_default();
                let code = json
                    .get("error")
                    .and_then(|e| {
                        e.get("code")
                            .and_then(|c| c.as_str())
                            .or_else(|| e.as_str())
                    })
                    .unwrap_or("");
                match code {
                    "deviceauth_authorization_pending" => {}
                    "slow_down" => interval_secs += SLOW_DOWN_INCREMENT_SECS,
                    _ => bail!("codex device login failed: {json}"),
                }
            }
        }
    };
    let OAuthFlow::PkceLoopback(pkce) = parent.flow else {
        bail!("codex device flow requires the pkce spec for exchange");
    };
    let mut creds = exchange_code(
        pkce,
        &authorization_code,
        &code_verifier,
        None,
        DEVICE_REDIRECT_URI,
    )
    .await?;
    if creds.account_id.is_none()
        && let Some(access) = creds.access_token.as_deref()
    {
        creds.account_id = jwt_claim(access, "https://api.openai.com/auth", "chatgpt_account_id");
    }
    Ok(creds)
}

// ---------------------------------------------------------------------------
// Browser (PKCE loopback) flow
// ---------------------------------------------------------------------------

async fn run_pkce_flow(
    provider_id: &str,
    spec: &PkceSpec,
    ui: &mut dyn LoginUi,
) -> Result<StoredCredential> {
    let (verifier, challenge) = pkce_pair();
    let state = if spec.state_is_verifier {
        verifier.clone()
    } else {
        random_state()
    };

    let listener = match spec.callback_port {
        Some(port) => {
            let addr = SocketAddr::from(([127, 0, 0, 1], port));
            match tokio::net::TcpListener::bind(addr).await {
                Ok(l) => Some(l),
                Err(e) => {
                    tracing::warn!(error = %e, port, "oauth callback port busy; paste fallback only");
                    None
                }
            }
        }
        None => {
            let addr = SocketAddr::from(([127, 0, 0, 1], 0));
            Some(
                tokio::net::TcpListener::bind(addr)
                    .await
                    .context("bind callback listener")?,
            )
        }
    };
    let port = listener
        .as_ref()
        .and_then(|l| l.local_addr().ok())
        .map(|a| a.port())
        .or(spec.callback_port);
    let Some(port) = port else {
        bail!("could not bind an OAuth callback port and no fixed port exists");
    };
    let redirect_uri = format!("http://localhost:{port}{}", spec.callback_path);
    let url = authorize_url(spec, &challenge, &state, &redirect_uri);
    ui.notice(LoginNotice::AuthUrl {
        url: url.clone(),
        callback_hint: redirect_uri.clone(),
    })
    .await;
    let _ = open_browser_detached(&url).await;

    let manual = register_pending(provider_id);
    let result = match listener {
        Some(l) => wait_for_callback(l, spec.callback_path, Some(&state), manual).await,
        None => wait_manual_only(provider_id, Some(&state), manual).await,
    };
    unregister_pending(provider_id);
    let code = result?;

    let mut creds = exchange_code(spec, &code, &verifier, Some(&state), &redirect_uri).await?;
    if provider_id == "openai-codex" {
        creds.account_id = creds
            .access_token
            .as_deref()
            .and_then(|t| jwt_claim(t, "https://api.openai.com/auth", "chatgpt_account_id"));
        if creds.account_id.is_none() {
            tracing::warn!("openai-codex login: token carried no chatgpt_account_id claim");
        }
    }
    Ok(creds)
}

/// No listener (fixed port busy): only the paste path can finish the login.
async fn wait_manual_only(
    provider_id: &str,
    expected_state: Option<&str>,
    mut manual: mpsc::UnboundedReceiver<String>,
) -> Result<String> {
    let _ = provider_id;
    let timeout = tokio::time::sleep(LOGIN_TIMEOUT);
    tokio::pin!(timeout);
    loop {
        tokio::select! {
            _ = &mut timeout => bail!("login timed out waiting for a pasted redirect URL"),
            pasted = manual.recv() => {
                let Some(input) = pasted else { continue };
                let (code, state) = parse_pasted_input(&input);
                if let (Some(expected), Some(got)) = (expected_state, state.as_deref())
                    && got != expected
                {
                    bail!("pasted redirect URL has a mismatched state parameter");
                }
                if let Some(code) = code {
                    return Ok(code);
                }
                bail!("pasted input did not contain an authorization code");
            }
        }
    }
}

/// Open `url` in the system browser off-thread (termux-open-url on Android,
/// webbrowser elsewhere). Mirrors `device_code::open_browser_detached`.
async fn open_browser_detached(url: &str) -> bool {
    let url = url.to_owned();
    match tokio::task::spawn_blocking(move || {
        #[cfg(target_os = "android")]
        {
            std::process::Command::new("termux-open-url")
                .arg(&url)
                .status()
                .and_then(|status| {
                    if status.success() {
                        Ok(())
                    } else {
                        Err(std::io::Error::other("termux-open-url failed"))
                    }
                })
        }
        #[cfg(not(target_os = "android"))]
        {
            webbrowser::open(&url)
        }
    })
    .await
    {
        Ok(Ok(())) => true,
        result => {
            let error = match result {
                Ok(Err(e)) => e.to_string(),
                Err(e) => e.to_string(),
                _ => unreachable!(),
            };
            tracing::info!(error, "provider oauth: browser open failed");
            false
        }
    }
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Run the login flow for a provider. `ui` receives the URL/device-code
/// notices to render. On success the credential is saved to
/// `provider-auth.json` and returned.
pub async fn run_login(
    provider_id: &str,
    method: LoginMethod,
    ui: &mut dyn LoginUi,
) -> Result<StoredCredential> {
    let spec = spec_for(provider_id)
        .with_context(|| format!("no built-in OAuth flow for provider \"{provider_id}\""))?;
    let creds = match (method, spec.flow, spec.device_flow) {
        (LoginMethod::Device, _, Some(device))
        | (LoginMethod::Device, OAuthFlow::DeviceCode(device), _)
        | (LoginMethod::Preferred, OAuthFlow::DeviceCode(device), _) => {
            run_device_flow(provider_id, device, spec, ui).await?
        }
        (LoginMethod::Device, OAuthFlow::PkceLoopback(_), None) => {
            bail!("{provider_id} has no device-code login")
        }
        (LoginMethod::Browser, OAuthFlow::DeviceCode(_), _) => {
            bail!("{provider_id} only supports device-code login")
        }
        (_, OAuthFlow::PkceLoopback(pkce), _) => run_pkce_flow(provider_id, pkce, ui).await?,
    };
    save_credentials(provider_id, creds.clone())?;
    Ok(creds)
}

/// Minted request credential for the auth-provider slot path.
pub struct MintedCredential {
    pub token: String,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Produce a usable token for `provider_id` from the stored credential,
/// refreshing/exchanging as needed. Called from `auth_provider` mint when
/// `[auth_provider.*].oauth = "<provider_id>"`.
pub async fn mint(provider_id: &str) -> Result<MintedCredential> {
    let spec = spec_for(provider_id)
        .with_context(|| format!("unknown built-in OAuth provider \"{provider_id}\""))?;
    let creds = load_credentials(provider_id).with_context(|| {
        format!("{provider_id} is not signed in — run /provider oauth {provider_id}")
    })?;
    match spec.refresh {
        RefreshStyle::ApiKey => {
            let key = creds
                .api_key
                .filter(|k| !k.is_empty())
                .context("stored credential carries no api_key")?;
            Ok(MintedCredential {
                token: key,
                expires_at: creds.expires_at,
            })
        }
        RefreshStyle::StandardForm | RefreshStyle::AnthropicJson => {
            let access = creds
                .access_token
                .clone()
                .filter(|t| !t.is_empty())
                .context("stored credential carries no access token")?;
            let fresh = creds
                .expires_at
                .is_none_or(|at| chrono::Utc::now() + chrono::Duration::seconds(60) < at);
            if fresh {
                return Ok(MintedCredential {
                    token: access,
                    expires_at: creds.expires_at,
                });
            }
            let refresh = creds
                .refresh_token
                .clone()
                .filter(|t| !t.is_empty())
                .context("token expired and no refresh token stored — log in again")?;
            let (token_url, client_id) = match spec.flow {
                OAuthFlow::PkceLoopback(pkce) => (pkce.token_url, pkce.client_id),
                OAuthFlow::DeviceCode(device) => (device.token_url, Some(device.client_id)),
            };
            refresh_form_or_json(provider_id, spec, token_url, client_id, &refresh).await
        }
        RefreshStyle::CopilotSession => {
            let identity = creds
                .access_token
                .clone()
                .filter(|t| !t.is_empty())
                .context("stored credential carries no GitHub OAuth token")?;
            let still_valid = creds
                .expires_at
                .is_none_or(|at| chrono::Utc::now() + chrono::Duration::seconds(60) < at)
                && creds.api_key.is_some();
            if still_valid && let Some(token) = creds.api_key.clone() {
                return Ok(MintedCredential {
                    token,
                    expires_at: creds.expires_at,
                });
            }
            exchange_copilot_session(provider_id, spec, &identity).await
        }
        RefreshStyle::MetaKey => {
            let identity = creds
                .access_token
                .clone()
                .filter(|t| !t.is_empty())
                .context("stored credential carries no Meta identity token")?;
            let still_valid = creds
                .expires_at
                .is_none_or(|at| chrono::Utc::now() + chrono::Duration::seconds(60) < at)
                && creds.api_key.is_some();
            if still_valid && let Some(token) = creds.api_key.clone() {
                return Ok(MintedCredential {
                    token,
                    expires_at: creds.expires_at,
                });
            }
            exchange_meta_key(provider_id, spec, &identity).await
        }
    }
}

async fn refresh_form_or_json(
    provider_id: &str,
    spec: &OAuthSpec,
    token_url: &str,
    client_id: Option<&'static str>,
    refresh: &str,
) -> Result<MintedCredential> {
    let resp = if spec.refresh == RefreshStyle::AnthropicJson {
        client()
            .post(token_url)
            .json(&serde_json::json!({
                "grant_type": "refresh_token",
                "client_id": client_id.unwrap_or_default(),
                "refresh_token": refresh,
            }))
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .context("token refresh request")?
    } else {
        let mut form: Vec<(&str, &str)> =
            vec![("grant_type", "refresh_token"), ("refresh_token", refresh)];
        if let Some(client_id) = client_id {
            form.push(("client_id", client_id));
        }
        client()
            .post(token_url)
            .form(&form)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .context("token refresh request")?
    };
    let mut next = standard_token_from_response(resp, "token refresh").await?;
    // Servers may rotate or omit refresh_token on refresh; keep the old one.
    if next.refresh_token.is_none() {
        next.refresh_token = Some(refresh.to_owned());
    }
    if provider_id == "openai-codex" && next.account_id.is_none() {
        next.account_id = next
            .access_token
            .as_deref()
            .and_then(|t| jwt_claim(t, "https://api.openai.com/auth", "chatgpt_account_id"));
    }
    let token = next
        .access_token
        .clone()
        .context("refresh returned no access token")?;
    let expires_at = next.expires_at;
    save_credentials(provider_id, next)?;
    Ok(MintedCredential { token, expires_at })
}

const COPILOT_HEADERS: &[(&str, &str)] = &[
    ("user-agent", "GitHubCopilotChat/0.35.0"),
    ("editor-version", "vscode/1.107.0"),
    ("editor-plugin-version", "copilot-chat/0.35.0"),
    ("copilot-integration-id", "vscode-chat"),
];

async fn exchange_copilot_session(
    provider_id: &str,
    spec: &OAuthSpec,
    identity: &str,
) -> Result<MintedCredential> {
    let mint_url = spec.mint_url.context("copilot spec missing mint_url")?;
    let mut req = client()
        .get(mint_url)
        .header("accept", "application/json")
        .bearer_auth(identity)
        .timeout(REQUEST_TIMEOUT);
    for (k, v) in COPILOT_HEADERS {
        req = req.header(*k, *v);
    }
    let resp = req.send().await.context("copilot session exchange")?;
    let status = resp.status();
    let json: serde_json::Value = resp.json().await.unwrap_or_default();
    let token = json
        .get("token")
        .and_then(|t| t.as_str())
        .map(str::to_owned);
    let expires_at = json
        .get("expires_at")
        .and_then(|e| e.as_i64())
        .and_then(|secs| chrono::DateTime::from_timestamp(secs, 0));
    let Some(token) = token.filter(|t| !t.is_empty()) else {
        bail!("copilot session exchange failed (HTTP {status}): {json}");
    };
    let mut creds = load_credentials(provider_id).unwrap_or_default();
    creds.api_key = Some(token.clone());
    creds.expires_at = expires_at;
    save_credentials(provider_id, creds)?;
    Ok(MintedCredential { token, expires_at })
}

async fn exchange_meta_key(
    provider_id: &str,
    spec: &OAuthSpec,
    identity: &str,
) -> Result<MintedCredential> {
    let mint_url = spec.mint_url.context("meta spec missing mint_url")?;
    let resp = client()
        .post(mint_url)
        .json(&serde_json::json!({ "access_token": identity }))
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .context("meta key mint")?;
    let status = resp.status();
    let json: serde_json::Value = resp.json().await.unwrap_or_default();
    let key = json
        .get("api_key")
        .and_then(|k| k.as_str())
        .map(str::to_owned);
    let Some(key) = key.filter(|k| !k.is_empty()) else {
        let action = json
            .get("action_url")
            .and_then(|a| a.as_str())
            .unwrap_or("");
        bail!("meta key mint failed (HTTP {status}): {json} {action}");
    };
    // Minted keys live ~24h (pi parity).
    let expires_at = Some(chrono::Utc::now() + chrono::Duration::hours(24));
    let mut creds = load_credentials(provider_id).unwrap_or_default();
    creds.api_key = Some(key.clone());
    creds.expires_at = expires_at;
    save_credentials(provider_id, creds)?;
    Ok(MintedCredential {
        token: key,
        expires_at,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_spec_resolves_by_id() {
        for id in [
            "openai-codex",
            "anthropic",
            "github-copilot",
            "kimi-coding",
            "meta",
            "openrouter",
        ] {
            assert!(spec_for(id).is_some(), "missing spec for {id}");
        }
        assert!(spec_for("openai").is_none());
    }

    #[test]
    fn paste_parses_url_code_state() {
        let (code, state) =
            parse_pasted_input("http://localhost:1455/auth/callback?code=abc123&state=st-9");
        assert_eq!(code.as_deref(), Some("abc123"));
        assert_eq!(state.as_deref(), Some("st-9"));

        let (code, state) = parse_pasted_input("code=xyz&state=q");
        assert_eq!(code.as_deref(), Some("xyz"));
        assert_eq!(state.as_deref(), Some("q"));

        let (code, state) = parse_pasted_input("rawcode#st8");
        assert_eq!(code.as_deref(), Some("rawcode"));
        assert_eq!(state.as_deref(), Some("st8"));

        let (code, state) = parse_pasted_input("just-a-code");
        assert_eq!(code.as_deref(), Some("just-a-code"));
        assert!(state.is_none());
    }

    #[test]
    fn authorize_url_codex_shape() {
        let spec = match spec_for("openai-codex").map(|s| s.flow) {
            Some(OAuthFlow::PkceLoopback(p)) => p,
            _ => panic!(),
        };
        let url = authorize_url(spec, "CH", "ST", "http://localhost:1455/auth/callback");
        for needle in [
            "response_type=code",
            "client_id=app_EMoamEEZ73f0CkXaXp7hrann",
            "redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback",
            "scope=openid+profile+email+offline_access",
            "code_challenge=CH",
            "code_challenge_method=S256",
            "state=ST",
            "id_token_add_organizations=true",
            "codex_cli_simplified_flow=true",
            "originator=grok",
        ] {
            assert!(url.contains(needle), "missing {needle} in {url}");
        }
    }

    #[test]
    fn authorize_url_anthropic_uses_verifier_state_and_code_flag() {
        let spec = match spec_for("anthropic").map(|s| s.flow) {
            Some(OAuthFlow::PkceLoopback(p)) => p,
            _ => panic!(),
        };
        let url = authorize_url(spec, "CH", "VERIFIER", "http://localhost:53692/callback");
        assert!(url.contains("code=true"));
        assert!(url.contains("state=VERIFIER"));
        assert!(url.contains("client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e"));
    }

    #[test]
    fn authorize_url_openrouter_shape() {
        let spec = match spec_for("openrouter").map(|s| s.flow) {
            Some(OAuthFlow::PkceLoopback(p)) => p,
            _ => panic!(),
        };
        let url = authorize_url(spec, "CH", "ST", "http://localhost:12345/callback");
        assert!(url.contains("callback_url=http%3A%2F%2Flocalhost%3A12345%2Fcallback"));
        assert!(url.contains("code_challenge=CH"));
        assert!(!url.contains("client_id"));
    }

    #[test]
    fn jwt_claim_extracts_nested() {
        use base64::Engine as _;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acc-1"}}"#);
        let token = format!("h.{payload}.s");
        assert_eq!(
            jwt_claim(&token, "https://api.openai.com/auth", "chatgpt_account_id").as_deref(),
            Some("acc-1")
        );
    }

    #[test]
    fn verify_uri_trust_check() {
        assert!(trusted_verify_uri(
            "https://github.com/login/device",
            Some("github.com")
        ));
        assert!(trusted_verify_uri(
            "https://foo.github.com/x",
            Some("github.com")
        ));
        assert!(!trusted_verify_uri(
            "https://github.com.evil/x",
            Some("github.com")
        ));
        assert!(!trusted_verify_uri(
            "javascript:alert(1)",
            Some("github.com")
        ));
        assert!(!trusted_verify_uri("not a url", None));
    }
}
