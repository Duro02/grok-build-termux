//! `/provider` slash command: pi-style in-session management of model providers.
//!
//! Lists built-in and user providers, and registers/extends them by writing
//! `[model_providers.<id>]` / `[model."<id>/<model>"]` blocks into the user
//! `config.toml`. The config watcher diff sees the `model_providers`/`model`
//! tables change and injects `InternalMethod::ReloadModels`, so new models are
//! usable in the same session — no restart, no hand-editing TOML.
//!
//! Credential notes: prefer `--env-key` over `--key`. Slash-command args land in
//! the session transcript, so an inline key is exposed there; `env_key` stores
//! only the variable name.

use crate::agent::builtin_providers as bp;
use crate::agent::config::Config;
use anyhow::{Context, Result};
use toml::Value as TomlValue;
use toml::map::Map as TomlMap;
use xai_grok_login::provider_oauth as po;

/// Parsed `/provider` operation.
#[derive(Debug)]
pub(crate) enum ProviderOp {
    /// `/provider` — list every provider with its status.
    List,
    /// `/provider <id>` — details for one provider.
    Info { id: String },
    /// `/provider use <id>` — declare a built-in provider (write an empty
    /// `[model_providers.<id>]` block so it activates without an env key).
    Use { id: String },
    /// `/provider add <id> <base-url> [flags]` — register a custom provider.
    Add {
        id: String,
        base_url: String,
        fields: AddFields,
    },
    /// `/provider key <id> <api-key>` — store a literal key on the provider.
    Key { id: String, api_key: String },
    /// `/provider model <id> <model> [--name N] [--context-window N]`
    Model {
        provider: String,
        model: String,
        name: Option<String>,
        context_window: Option<u64>,
    },
    /// `/provider remove <id>` or `/provider remove <id> <model>`.
    Remove { id: String, model: Option<String> },
    /// `/provider oauth <id> [device|code <input>|logout]` — built-in
    /// browser/device sign-in for catalog providers that support OAuth.
    Oauth { id: String, action: OauthAction },
    /// Usage (also the fallback for a parse error, which is carried here).
    Help { error: Option<String> },
}

#[derive(Debug)]
pub(crate) enum OauthAction {
    /// Start the flow: `Preferred` (browser, or device when the provider is
    /// device-only) unless `device` forces the device path (Codex).
    Login { device: bool },
    /// `/provider oauth <id> code <input>` — paste the redirect URL or code
    /// into an in-flight login (manual path when loopback can't receive it).
    Code { input: String },
    /// `/provider oauth <id> logout` — drop stored credentials and the
    /// `auth` helper block written at sign-in.
    Logout,
}

/// Optional `[model_providers.<id>]` fields collected from `add` flags.
#[derive(Debug, Default)]
pub(crate) struct AddFields {
    pub backend: Option<String>,
    pub auth_scheme: Option<String>,
    pub env_key: Vec<String>,
    pub api_key: Option<String>,
    pub extra_headers: Vec<(String, String)>,
    pub context_window: Option<u64>,
}

const USAGE: &str = "\
Usage:
  /provider                                  List providers and their status
  /provider <id>                             Show one provider
  /provider use <id>                         Activate a built-in provider without an env key
  /provider add <id> <base-url> [options]    Register a custom provider
  /provider key <id> <api-key>               Store a literal API key on a provider
  /provider oauth <id> [device]              Sign in via browser/device OAuth (subscription login)
  /provider oauth <id> code <input>          Feed a pasted redirect/code into a running login
  /provider oauth <id> logout                Clear stored OAuth credentials
  /provider model <id> <model> [options]     Add a model to a provider
  /provider remove <id> [<model>]            Remove a provider or one of its models

Options for add: --backend chat_completions|responses|messages
                 --auth-scheme bearer|x_api_key
                 --env-key NAME[,NAME2]
                 --key <api-key>              (prefer --env-key; --key is saved in this transcript)
                 --header 'Name=value'        (repeatable)
                 --context-window <tokens>
Options for model: --name <label>            --context-window <tokens>

Examples:
  /provider use ollama
  /provider model ollama llama3.2 --name 'Llama 3.2'
  /provider add my-gateway https://gw.example.com/v1 --backend responses --env-key MY_GW_KEY
  /provider key deepseek sk-...";

/// Parse `/provider` args. Never fails outright — a malformed invocation maps
/// to `Help` carrying the error message.
pub(crate) fn parse(args: &str) -> ProviderOp {
    match parse_inner(args) {
        Ok(op) => op,
        Err(e) => ProviderOp::Help { error: Some(e) },
    }
}

/// Shell-ish arg splitter: whitespace-delimited, with `'...'`/`"..."` quoting so
/// `--name 'Llama 3.2'` and `--header 'X-Foo=a b'` carry spaces. Backslashes are
/// literal inside single quotes and escape the next char elsewhere.
fn split_args(args: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut escape = false;
    let mut has_token = false;
    for c in args.chars() {
        if escape {
            cur.push(c);
            escape = false;
            continue;
        }
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else if c == '\\' && q == '"' {
                    escape = true;
                } else {
                    cur.push(c);
                }
            }
            None => match c {
                '\'' | '"' => {
                    has_token = true;
                    quote = Some(c);
                }
                '\\' => {
                    has_token = true;
                    escape = true;
                }
                c if c.is_whitespace() => {
                    if has_token || !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                        has_token = false;
                    }
                }
                _ => {
                    has_token = true;
                    cur.push(c);
                }
            },
        }
    }
    if has_token || !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn parse_inner(args: &str) -> Result<ProviderOp, String> {
    let tokens = split_args(args);
    let mut it = tokens.iter().map(String::as_str);
    let sub = it.next().unwrap_or("");
    match sub {
        "" | "list" => {
            if let Some(extra) = it.next() {
                return Err(format!("unexpected argument '{extra}'"));
            }
            Ok(ProviderOp::List)
        }
        "use" => {
            let id = it.next().ok_or("missing provider id: /provider use <id>")?;
            reject_flags(&mut it)?;
            Ok(ProviderOp::Use { id: id.to_string() })
        }
        "key" => {
            let id = it
                .next()
                .ok_or("missing provider id: /provider key <id> <key>")?;
            let key = it.next().ok_or("missing api key")?;
            reject_flags(&mut it)?;
            Ok(ProviderOp::Key {
                id: id.to_string(),
                api_key: key.to_string(),
            })
        }
        "add" => {
            let id = it
                .next()
                .ok_or("missing provider id: /provider add <id> <base-url>")?;
            let base_url = it
                .next()
                .ok_or("missing base url: /provider add <id> <base-url>")?;
            let fields = parse_add_flags(it)?;
            Ok(ProviderOp::Add {
                id: id.to_string(),
                base_url: base_url.to_string(),
                fields,
            })
        }
        "model" => {
            let provider = it
                .next()
                .ok_or("missing provider id: /provider model <id> <model>")?;
            let model = it
                .next()
                .ok_or("missing model id: /provider model <id> <model>")?;
            let (name, context_window) = parse_model_flags(it)?;
            Ok(ProviderOp::Model {
                provider: provider.to_string(),
                model: model.to_string(),
                name,
                context_window,
            })
        }
        "oauth" => {
            let id = it
                .next()
                .ok_or("missing provider id: /provider oauth <id> [device|code <input>|logout]")?;
            let action = match it.next() {
                None | Some("login") => OauthAction::Login { device: false },
                Some("device") => OauthAction::Login { device: true },
                Some("code") => {
                    let input = it
                        .next()
                        .ok_or("missing input: /provider oauth <id> code <redirect-url-or-code>")?;
                    OauthAction::Code {
                        input: input.to_string(),
                    }
                }
                Some("logout") => OauthAction::Logout,
                Some(other) => return Err(format!("unknown oauth action '{other}'")),
            };
            reject_flags(&mut it)?;
            Ok(ProviderOp::Oauth {
                id: id.to_string(),
                action,
            })
        }
        "remove" | "rm" => {
            let id = it
                .next()
                .ok_or("missing provider id: /provider remove <id> [<model>]")?;
            let model = it.next().map(str::to_string);
            reject_flags(&mut it)?;
            Ok(ProviderOp::Remove {
                id: id.to_string(),
                model,
            })
        }
        "help" | "-h" | "--help" => Ok(ProviderOp::Help { error: None }),
        other if !other.contains(char::is_whitespace) && !other.starts_with("--") => {
            reject_flags(&mut it)?;
            Ok(ProviderOp::Info {
                id: other.to_string(),
            })
        }
        other => Err(format!("unknown subcommand '{other}'")),
    }
}

fn reject_flags<'a>(mut it: impl Iterator<Item = &'a str>) -> Result<(), String> {
    match it.next() {
        Some(extra) => Err(format!("unexpected argument '{extra}'")),
        None => Ok(()),
    }
}

fn parse_add_flags<'a>(it: impl Iterator<Item = &'a str>) -> Result<AddFields, String> {
    let mut fields = AddFields::default();
    let mut it = it.peekable();
    while let Some(tok) = it.next() {
        let (flag, inline_val) = match tok.split_once('=') {
            Some((name, val)) => (name, Some(val.to_string())),
            None => (tok, None),
        };
        let mut take = |name: &str| -> Result<String, String> {
            if let Some(v) = inline_val.clone() {
                return Ok(v);
            }
            it.next()
                .map(str::to_string)
                .ok_or_else(|| format!("flag {name} requires a value"))
        };
        match flag {
            "--backend" => fields.backend = Some(take("--backend")?),
            "--auth-scheme" => fields.auth_scheme = Some(take("--auth-scheme")?),
            "--env-key" => {
                for name in take("--env-key")?.split(',') {
                    let name = name.trim();
                    if !name.is_empty() {
                        fields.env_key.push(name.to_string());
                    }
                }
            }
            "--key" | "--api-key" => fields.api_key = Some(take("--key")?),
            "--header" => {
                let kv = take("--header")?;
                let (k, v) = kv
                    .split_once('=')
                    .ok_or("--header requires Name=value format")?;
                if k.is_empty() {
                    return Err("--header name cannot be empty".to_string());
                }
                fields.extra_headers.push((k.to_string(), v.to_string()));
            }
            "--context-window" => {
                let raw = take("--context-window")?;
                fields.context_window = Some(
                    raw.parse::<u64>()
                        .map_err(|_| format!("--context-window must be a number, got '{raw}'"))?,
                );
            }
            other => return Err(format!("unknown flag '{other}'")),
        }
    }
    Ok(fields)
}

fn parse_model_flags<'a>(
    it: impl Iterator<Item = &'a str>,
) -> Result<(Option<String>, Option<u64>), String> {
    let mut name = None;
    let mut context_window = None;
    let mut it = it.peekable();
    while let Some(tok) = it.next() {
        let (flag, inline_val) = match tok.split_once('=') {
            Some((n, v)) => (n, Some(v.to_string())),
            None => (tok, None),
        };
        let mut take = |fname: &str| -> Result<String, String> {
            if let Some(v) = inline_val.clone() {
                return Ok(v);
            }
            it.next()
                .map(str::to_string)
                .ok_or_else(|| format!("flag {fname} requires a value"))
        };
        match flag {
            "--name" => name = Some(take("--name")?),
            "--context-window" => {
                let raw = take("--context-window")?;
                context_window = Some(
                    raw.parse::<u64>()
                        .map_err(|_| format!("--context-window must be a number, got '{raw}'"))?,
                );
            }
            other => return Err(format!("unknown flag '{other}'")),
        }
    }
    Ok((name, context_window))
}

/// Execute a parsed op and return the text to surface in the pager.
pub(crate) async fn run(op: &ProviderOp) -> String {
    run_with_ui(op, None).await
}

/// `run` plus a mid-flow notice sink for `ProviderOp::Oauth` logins (device
/// codes and authorize URLs must reach the user while the flow is running).
pub(crate) async fn run_with_ui(
    op: &ProviderOp,
    ui: Option<&mut (dyn po::LoginUi + '_)>,
) -> String {
    match run_inner(op, ui).await {
        Ok(text) => text,
        Err(e) => format!("provider: {e:#}"),
    }
}

/// Render one [`po::LoginNotice`] as session output text (device code /
/// authorize URL plus the manual-paste escape hatch).
pub(crate) fn oauth_notice_text(provider_id: &str, notice: &po::LoginNotice) -> String {
    match notice {
        po::LoginNotice::DeviceCode {
            verification_uri,
            verification_uri_complete,
            user_code,
            expires_secs,
        } => {
            let mins = expires_secs.div_ceil(60);
            let mut text = format!(
                "Sign in: open {verification_uri} and enter code `{user_code}` (expires in {mins} min)."
            );
            if let Some(complete) = verification_uri_complete {
                text.push_str(&format!("\nShortcut (code prefilled): {complete}"));
            }
            text
        }
        po::LoginNotice::AuthUrl { url, callback_hint } => format!(
            "Open this URL to sign in:\n  {url}\n\
             After approval it redirects to {callback_hint} — if nothing opens or the \
             redirect fails, paste the redirected URL here with `/provider oauth {provider_id} code <url>`"
        ),
    }
}

async fn run_inner(op: &ProviderOp, ui: Option<&mut (dyn po::LoginUi + '_)>) -> Result<String> {
    match op {
        ProviderOp::List => list_text(),
        ProviderOp::Info { id } => info_text(id),
        ProviderOp::Help { error } => Ok(match error {
            Some(e) => format!("provider: {e}\n\n{USAGE}"),
            None => USAGE.to_string(),
        }),
        ProviderOp::Use { id } => use_provider(id).await,
        ProviderOp::Add {
            id,
            base_url,
            fields,
        } => add_provider(id, base_url, fields).await,
        ProviderOp::Key { id, api_key } => set_provider_key(id, api_key).await,
        ProviderOp::Model {
            provider,
            model,
            name,
            context_window,
        } => add_model(provider, model, name.as_deref(), *context_window).await,
        ProviderOp::Remove { id, model } => remove(id, model.as_deref()).await,
        ProviderOp::Oauth { id, action } => oauth_op(id, action, ui).await,
    }
}

// ---------------------------------------------------------------------------
// Built-in OAuth sign-in
// ---------------------------------------------------------------------------

async fn oauth_op(
    id: &str,
    action: &OauthAction,
    ui: Option<&mut (dyn po::LoginUi + '_)>,
) -> Result<String> {
    match action {
        OauthAction::Code { input } => {
            po::deliver_manual_input(id, input.clone())?;
            Ok(format!(
                "pasted input delivered — the '{id}' sign-in continues in its original turn."
            ))
        }
        OauthAction::Logout => oauth_logout(id).await,
        OauthAction::Login { device } => oauth_login(id, *device, ui).await,
    }
}

async fn oauth_login(
    id: &str,
    device: bool,
    ui: Option<&mut (dyn po::LoginUi + '_)>,
) -> Result<String> {
    let Some(spec) = po::spec_for(id) else {
        return Ok(format!(
            "'{id}' has no built-in OAuth sign-in (supported: {}). \
             `/provider key {id} <api-key>` stores an API key instead.",
            po::oauth_ids().join(", ")
        ));
    };
    let Some(ui) = ui else {
        return Ok(
            "OAuth sign-in must run in an interactive session — retry from the TUI.".to_string(),
        );
    };
    let method = if device {
        po::LoginMethod::Device
    } else {
        po::LoginMethod::Preferred
    };
    let creds = po::run_login(id, method, ui).await?;
    write_oauth_provider(id, spec, &creds).await
}

/// Persist the `[model_providers.<id>]` block that makes the signed-in
/// provider usable: `api_key` for permanent-key OAuth (OpenRouter), else an
/// inline `auth` helper `{ oauth = "<id>" }` so requests mint through the
/// stored credential.
async fn write_oauth_provider(
    id: &str,
    spec: &'static po::OAuthSpec,
    creds: &po::StoredCredential,
) -> Result<String> {
    let id_owned = id.to_string();
    let api_key = creds.api_key.clone();
    let account_id = creds.account_id.clone();
    let bearer = spec.bearer;
    let report = rmw_user_config(move |root| {
        let table = providers_table(root)?;
        let block = table
            .entry(id_owned.clone())
            .or_insert_with(|| TomlValue::Table(TomlMap::new()))
            .as_table_mut()
            .with_context(|| format!("[model_providers.{id_owned}] must be a table"))?;
        match api_key {
            Some(key) => {
                block.insert("api_key".to_string(), TomlValue::String(key));
            }
            None => {
                let mut auth = TomlMap::new();
                auth.insert("oauth".to_string(), TomlValue::String(id_owned.clone()));
                block.insert("auth".to_string(), TomlValue::Table(auth));
                if bearer {
                    block.insert(
                        "auth_scheme".to_string(),
                        TomlValue::String("bearer".to_string()),
                    );
                }
            }
        }
        // Stable account binding for Codex; Anthropic OAuth needs the beta flag.
        let mut header_writes: Vec<(&str, String)> = Vec::new();
        if let Some(account) = account_id {
            header_writes.push(("chatgpt-account-id", account));
        }
        if bearer && id_owned == "anthropic" {
            header_writes.push(("anthropic-beta", "oauth-2025-04-20".to_string()));
        }
        if !header_writes.is_empty() {
            let headers = block
                .entry("extra_headers".to_string())
                .or_insert_with(|| TomlValue::Table(TomlMap::new()))
                .as_table_mut()
                .with_context(|| {
                    format!("[model_providers.{id_owned}].extra_headers must be a table")
                })?;
            for (k, v) in header_writes {
                headers.insert(k.to_string(), TomlValue::String(v));
            }
        }
        Ok(format!(
            "signed in to '{id_owned}' — provider block written to config.toml"
        ))
    })
    .await?;
    Ok(format!(
        "{report}\nTokens live in provider-auth.json and refresh automatically; \
         `{id}` models are now active (Ctrl+M). `/provider oauth {id} logout` signs out."
    ))
}

async fn oauth_logout(id: &str) -> Result<String> {
    let had_creds = po::load_credentials(id).is_some();
    po::clear_credentials(id)?;
    let id_owned = id.to_string();
    let report = rmw_user_config(move |root| {
        let Some(block) = providers_table(root)?.get_mut(&id_owned) else {
            return Ok(format!(
                "provider '{id_owned}' has no config block — nothing to strip."
            ));
        };
        let Some(block) = block.as_table_mut() else {
            return Ok(format!(
                "[model_providers.{id_owned}] is not a table — left untouched."
            ));
        };
        let mut removed = Vec::new();
        if block.remove("auth").is_some() {
            removed.push("auth");
        }
        // An OAuth-written `api_key` (OpenRouter) belongs to the sign-in.
        if po::spec_for(&id_owned).is_some_and(|s| matches!(s.refresh, po::RefreshStyle::ApiKey))
            && block.remove("api_key").is_some()
        {
            removed.push("api_key");
        }
        if let Some(headers) = block
            .get_mut("extra_headers")
            .and_then(TomlValue::as_table_mut)
        {
            for k in ["chatgpt-account-id", "anthropic-beta"] {
                if headers.remove(k).is_some() {
                    removed.push(k);
                }
            }
        }
        if bearer_sign_in(&id_owned) {
            block.remove("auth_scheme");
        }
        Ok(if removed.is_empty() {
            format!("provider '{id_owned}': no OAuth-managed fields to strip.")
        } else {
            format!(
                "provider '{id_owned}': removed {} from config.toml.",
                removed.join(", ")
            )
        })
    })
    .await?;
    Ok(format!(
        "{}{report}",
        if had_creds {
            format!("cleared stored credentials for '{id}'. ")
        } else {
            format!("no stored credentials for '{id}'. ")
        }
    ))
}

/// `spec.bearer` without tripping on unknown ids.
fn bearer_sign_in(id: &str) -> bool {
    po::spec_for(id).is_some_and(|s| s.bearer)
}

fn load_cfg() -> Result<Config> {
    crate::config::load_agent_config_disk_only().map_err(|e| anyhow::anyhow!("load config: {e}"))
}

fn list_text() -> Result<String> {
    let cfg = load_cfg()?;
    let rows = bp::provider_statuses(&cfg);
    if rows.is_empty() {
        return Ok("No providers configured.".to_string());
    }
    let mut out = String::from("Providers:\n");
    for r in &rows {
        let state = if !r.allowed {
            "disabled by GROK_BUILTIN_PROVIDERS"
        } else if r.active {
            "active"
        } else {
            "inactive"
        };
        let mut detail = String::new();
        if !r.env_keys.is_empty() {
            detail.push_str(&format!(" env: {}", r.env_keys.join("/")));
        }
        if r.model_count > 0 {
            detail.push_str(&format!(" models: {}", r.model_count));
        }
        if let Some(url) = &r.base_url {
            detail.push_str(&format!(" url: {url}"));
        }
        out.push_str(&format!(
            "  {:<14} {:<8} {:<7}{}\n",
            r.id,
            state,
            if r.builtin { "builtin" } else { "custom" },
            detail
        ));
    }
    out.push_str("\nSet the provider's env var to activate it, or `/provider use <id>` to declare it without a key.\n`/provider help` for adding custom providers and models.");
    Ok(out)
}

fn info_text(id: &str) -> Result<String> {
    let cfg = load_cfg()?;
    let rows = bp::provider_statuses(&cfg);
    let Some(r) = rows.iter().find(|r| r.id == id) else {
        return Ok(format!(
            "Unknown provider '{id}'. `/provider` lists known ids; `/provider add {id} <base-url>` registers it."
        ));
    };
    let mut out = format!("Provider: {}\n", r.id);
    out.push_str(&format!(
        "  source:   {}\n",
        match (r.builtin, r.user_defined) {
            (true, true) => "builtin (overridden in config.toml)",
            (true, false) => "builtin",
            (false, true) => "custom (config.toml)",
            (false, false) => "unknown",
        }
    ));
    if let Some(url) = &r.base_url {
        out.push_str(&format!("  base_url: {url}\n"));
    }
    if let Some(b) = &r.api_backend {
        out.push_str(&format!("  backend:  {}\n", backend_name(b)));
    }
    if let Some(spec) = po::spec_for(id) {
        out.push_str(&format!(
            "  oauth:    {} ({})",
            spec.display_name,
            if po::load_credentials(id).is_some() {
                "signed in"
            } else {
                "not signed in"
            }
        ));
    }
    if !r.env_keys.is_empty() {
        let set: Vec<String> = r
            .env_keys
            .iter()
            .filter(|n| std::env::var(n.as_str()).is_ok_and(|v| !v.trim().is_empty()))
            .cloned()
            .collect();
        out.push_str(&format!(
            "  env_key:  {} ({})\n",
            r.env_keys.join(", "),
            if set.is_empty() { "not set" } else { "set" }
        ));
    }
    out.push_str(&format!(
        "  status:   {}\n",
        if r.active { "active" } else { "inactive" }
    ));
    if r.builtin {
        let models = bp::builtin_model_ids(id);
        if !models.is_empty() {
            let preview: Vec<&str> = models.iter().take(6).map(String::as_str).collect();
            let more = if models.len() > 6 {
                format!(" … +{}", models.len() - 6)
            } else {
                String::new()
            };
            out.push_str(&format!("  models:   {}{}\n", preview.join(", "), more));
        }
        if !r.active {
            if r.requires_key {
                let hint = r.env_keys.first().map(String::as_str).unwrap_or("<ENV>");
                out.push_str(&format!(
                    "\nActivate: `export {hint}=<key>` then restart, or `/provider use {id}` to enable now (key still needed for requests: `/provider key {id} <key>`).\n"
                ));
            } else {
                out.push_str(&format!(
                    "\nActivate: `/provider model {id} <model-id>` adds a local model — no key needed.\n"
                ));
            }
        } else {
            out.push_str(&format!(
                "\nAdd a model: `/provider model {id} <model-id>`.\n"
            ));
        }
    } else {
        out.push_str(&format!(
            "\nAdd a model: `/provider model {id} <model-id>`.\n"
        ));
    }
    Ok(out)
}

fn backend_name(b: &crate::sampling::ApiBackend) -> &'static str {
    match b {
        crate::sampling::ApiBackend::ChatCompletions => "chat_completions",
        crate::sampling::ApiBackend::Responses => "responses",
        crate::sampling::ApiBackend::Messages => "messages",
    }
}

/// Read-modify-write the user `config.toml` under the shared write guard.
/// `edit` returns the report string; on `Ok` the doc is serialized and written
/// atomically. The config watcher picks the write up and reloads models live.
async fn rmw_user_config(
    edit: impl FnOnce(&mut TomlMap<String, TomlValue>) -> Result<String> + Send + 'static,
) -> Result<String> {
    let guard = crate::util::config::lock_config_writes()
        .await
        .map_err(|e| anyhow::anyhow!("lock config.toml: {e}"))?;
    guard
        .run_blocking(move || {
            let path = crate::util::config::user_config_path();
            let (dest, content) = crate::util::config::read_follow_bound(&path)
                .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
            let mut doc = crate::util::config::parse_existing_config_toml(&content)
                .map_err(|e| anyhow::anyhow!("parse {}: {e}", path.display()))?;
            let root = doc
                .as_table_mut()
                .with_context(|| format!("{} must contain a TOML table", path.display()))?;
            let report = edit(root)?;
            let serialized = toml::to_string_pretty(&doc)
                .map_err(|e| anyhow::anyhow!("serialize {}: {e}", path.display()))?;
            crate::util::config::atomic_write_follow_bound(&path, &dest, &serialized)
                .map_err(|e| anyhow::anyhow!("write {}: {e}", path.display()))?;
            Ok::<String, anyhow::Error>(report)
        })
        .await
        .map_err(|e| anyhow::anyhow!("config write task failed: {e}"))?
}

fn providers_table<'a>(
    root: &'a mut TomlMap<String, TomlValue>,
) -> Result<&'a mut TomlMap<String, TomlValue>> {
    let entry = root
        .entry("model_providers".to_string())
        .or_insert_with(|| TomlValue::Table(TomlMap::new()));
    entry
        .as_table_mut()
        .context("[model_providers] must be a table")
}

fn models_table<'a>(
    root: &'a mut TomlMap<String, TomlValue>,
) -> Result<&'a mut TomlMap<String, TomlValue>> {
    let entry = root
        .entry("model".to_string())
        .or_insert_with(|| TomlValue::Table(TomlMap::new()));
    entry.as_table_mut().context("[model] must be a table")
}

async fn use_provider(id: &str) -> Result<String> {
    if !bp::is_builtin_id(id) {
        return Ok(format!(
            "'{id}' is not a built-in provider. `/provider add {id} <base-url>` registers a custom one; `/provider` lists built-ins."
        ));
    }
    let id = id.to_string();
    let report = rmw_user_config(move |root| {
        let table = providers_table(root)?;
        if table.contains_key(&id) {
            return Ok(format!(
                "provider '{id}' already declared in config.toml — no change."
            ));
        }
        table.insert(id.clone(), TomlValue::Table(TomlMap::new()));
        Ok(format!(
            "provider '{id}' declared — an empty [model_providers.{id}] block was written to config.toml; its catalog models are now active."
        ))
    })
    .await?;
    Ok(format!(
        "{report}\nThe model list reloads automatically (Ctrl+M picker updates shortly)."
    ))
}

async fn add_provider(id: &str, base_url: &str, fields: &AddFields) -> Result<String> {
    if let Some(b) = &fields.backend
        && !matches!(b.as_str(), "chat_completions" | "responses" | "messages")
    {
        return Ok(format!(
            "unknown --backend '{b}' (expected chat_completions|responses|messages)."
        ));
    }
    if let Some(s) = &fields.auth_scheme
        && !matches!(s.as_str(), "bearer" | "x_api_key")
    {
        return Ok(format!(
            "unknown --auth-scheme '{s}' (expected bearer|x_api_key)."
        ));
    }
    let id_owned = id.to_string();
    let base_url = base_url.to_string();
    let fields = AddFields {
        backend: fields.backend.clone(),
        auth_scheme: fields.auth_scheme.clone(),
        env_key: fields.env_key.clone(),
        api_key: fields.api_key.clone(),
        extra_headers: fields.extra_headers.clone(),
        context_window: fields.context_window,
    };
    let key_note = if fields.api_key.is_some() {
        "\nnote: the key was stored in config.toml and echoed into this session's transcript — prefer `--env-key NAME` next time."
    } else {
        ""
    };
    let report = rmw_user_config(move |root| {
        let table = providers_table(root)?;
        let block = table
            .entry(id_owned.clone())
            .or_insert_with(|| TomlValue::Table(TomlMap::new()))
            .as_table_mut()
            .with_context(|| format!("[model_providers.{id_owned}] must be a table"))?;
        block.insert("base_url".to_string(), TomlValue::String(base_url));
        if let Some(b) = fields.backend {
            block.insert("api_backend".to_string(), TomlValue::String(b));
        }
        if let Some(s) = fields.auth_scheme {
            block.insert("auth_scheme".to_string(), TomlValue::String(s));
        }
        if !fields.env_key.is_empty() {
            let env = if fields.env_key.len() == 1 {
                TomlValue::String(fields.env_key.first().expect("len checked above").clone())
            } else {
                TomlValue::Array(
                    fields
                        .env_key
                        .iter()
                        .map(|e| TomlValue::String(e.clone()))
                        .collect(),
                )
            };
            block.insert("env_key".to_string(), env);
        }
        if let Some(k) = fields.api_key {
            block.insert("api_key".to_string(), TomlValue::String(k));
        }
        if !fields.extra_headers.is_empty() {
            let mut headers = TomlMap::new();
            for (k, v) in fields.extra_headers {
                headers.insert(k, TomlValue::String(v));
            }
            block.insert("extra_headers".to_string(), TomlValue::Table(headers));
        }
        if let Some(cw) = fields.context_window {
            block.insert("context_window".to_string(), TomlValue::Integer(cw as i64));
        }
        Ok(format!("provider '{id_owned}' written to config.toml"))
    })
    .await?;
    Ok(format!(
        "{report}{key_note}\nAdd models with `/provider model {id} <model-id>`; the model list reloads automatically."
    ))
}

async fn set_provider_key(id: &str, api_key: &str) -> Result<String> {
    let id_owned = id.to_string();
    let api_key = api_key.to_string();
    let report = rmw_user_config(move |root| {
        let table = providers_table(root)?;
        let block = table
            .entry(id_owned.clone())
            .or_insert_with(|| TomlValue::Table(TomlMap::new()))
            .as_table_mut()
            .with_context(|| format!("[model_providers.{id_owned}] must be a table"))?;
        block.insert("api_key".to_string(), TomlValue::String(api_key));
        Ok(format!("api key saved for provider '{id_owned}'"))
    })
    .await?;
    Ok(format!(
        "{report}\nnote: the key was stored in config.toml and echoed into this session's transcript.\nThe model list reloads automatically."
    ))
}

async fn add_model(
    provider: &str,
    model: &str,
    name: Option<&str>,
    context_window: Option<u64>,
) -> Result<String> {
    let cfg = load_cfg()?;
    let provider_exists = bp::is_builtin_id(provider) || cfg.model_providers.contains_key(provider);
    if !provider_exists {
        return Ok(format!(
            "unknown provider '{provider}'. Register it first: `/provider add {provider} <base-url>` or `/provider use {provider}` if built-in."
        ));
    }
    let key = format!("{provider}/{model}");
    let provider = provider.to_string();
    let model = model.to_string();
    let name = name.map(str::to_string);
    let report = rmw_user_config(move |root| {
        let table = models_table(root)?;
        let existed = table.contains_key(&key);
        let entry = table
            .entry(key.clone())
            .or_insert_with(|| TomlValue::Table(TomlMap::new()))
            .as_table_mut()
            .with_context(|| format!("[model.\"{key}\"] must be a table"))?;
        entry.insert("model".to_string(), TomlValue::String(model));
        entry.insert("model_provider".to_string(), TomlValue::String(provider));
        if let Some(n) = name {
            entry.insert("name".to_string(), TomlValue::String(n));
        }
        if let Some(cw) = context_window {
            entry.insert("context_window".to_string(), TomlValue::Integer(cw as i64));
        }
        Ok(if existed {
            format!("model '{key}' updated in config.toml")
        } else {
            format!("model '{key}' added to config.toml")
        })
    })
    .await?;
    Ok(format!(
        "{report}\nIt appears in the model picker (Ctrl+M) once the config reload lands."
    ))
}

async fn remove(id: &str, model: Option<&str>) -> Result<String> {
    let id_owned = id.to_string();
    let model_owned = model.map(str::to_string);
    let report = rmw_user_config(move |root| {
        match &model_owned {
            Some(m) => {
                let key = format!("{id_owned}/{m}");
                let table = models_table(root)?;
                if table.remove(&key).is_none() {
                    return Ok(format!("no model '{key}' in config.toml — nothing to remove."));
                }
                Ok(format!("model '{key}' removed from config.toml"))
            }
            None => {
                let table = providers_table(root)?;
                if table.remove(&id_owned).is_none() {
                    return Ok(format!(
                        "no [model_providers.{id_owned}] block in config.toml — nothing to remove."
                    ));
                }
                // Surface catalog keys the user added that now lose their provider.
                let orphaned: Vec<String> = models_table(root)?
                    .keys()
                    .filter(|k| k.starts_with(&format!("{id_owned}/")))
                    .cloned()
                    .collect();
                let mut report = format!("provider '{id_owned}' removed from config.toml");
                if !orphaned.is_empty() {
                    report.push_str(&format!(
                        "\nstill configured but now providerless: {} — remove with `/provider remove {} <model>`",
                        orphaned.join(", "),
                        id_owned
                    ));
                }
                Ok(report)
            }
        }
    })
    .await?;
    Ok(format!("{report}\nThe model list reloads automatically."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bare_lists() {
        assert!(matches!(parse(""), ProviderOp::List));
        assert!(matches!(parse("  "), ProviderOp::List));
        assert!(matches!(parse("list"), ProviderOp::List));
    }

    #[test]
    fn parse_info_and_subcommands() {
        assert!(matches!(
            parse("openai"),
            ProviderOp::Info { ref id } if id == "openai"
        ));
        assert!(matches!(
            parse("use ollama"),
            ProviderOp::Use { ref id } if id == "ollama"
        ));
        assert!(matches!(
            parse("remove deepseek"),
            ProviderOp::Remove { ref id, model: None } if id == "deepseek"
        ));
        assert!(matches!(
            parse("remove deepseek deepseek-chat"),
            ProviderOp::Remove { ref id, model: Some(ref m) } if id == "deepseek" && m == "deepseek-chat"
        ));
    }

    #[test]
    fn parse_oauth_actions() {
        assert!(matches!(
            parse("oauth anthropic"),
            ProviderOp::Oauth {
                ref id,
                action: OauthAction::Login { device: false }
            } if id == "anthropic"
        ));
        assert!(matches!(
            parse("oauth openai-codex device"),
            ProviderOp::Oauth {
                ref id,
                action: OauthAction::Login { device: true }
            } if id == "openai-codex"
        ));
        assert!(matches!(
            parse("oauth anthropic logout"),
            ProviderOp::Oauth {
                ref id,
                action: OauthAction::Logout
            } if id == "anthropic"
        ));
        assert!(matches!(
            parse("oauth anthropic code 'http://localhost:53692/callback?code=x&state=y'"),
            ProviderOp::Oauth {
                ref id,
                action: OauthAction::Code { ref input }
            } if id == "anthropic" && input.contains("code=x")
        ));
        assert!(matches!(
            parse("oauth"),
            ProviderOp::Help { error: Some(_) }
        ));
        assert!(matches!(
            parse("oauth anthropic bogus"),
            ProviderOp::Help { error: Some(_) }
        ));
    }

    #[test]
    fn parse_add_flags() {
        let ProviderOp::Add {
            id,
            base_url,
            fields,
        } = parse(
            "add gw https://gw.example/v1 --backend responses --env-key KEY1,KEY2 --header X-A=1 --context-window 128000",
        )
        else {
            panic!("expected Add");
        };
        assert_eq!(id, "gw");
        assert_eq!(base_url, "https://gw.example/v1");
        assert_eq!(fields.backend.as_deref(), Some("responses"));
        assert_eq!(fields.env_key, vec!["KEY1".to_string(), "KEY2".to_string()]);
        assert_eq!(
            fields.extra_headers,
            vec![("X-A".to_string(), "1".to_string())]
        );
        assert_eq!(fields.context_window, Some(128000));
    }

    #[test]
    fn parse_add_inline_eq_and_key() {
        let ProviderOp::Add { fields, .. } =
            parse("add gw https://gw/v1 --key=sk-123 --auth-scheme x_api_key")
        else {
            panic!("expected Add");
        };
        assert_eq!(fields.api_key.as_deref(), Some("sk-123"));
        assert_eq!(fields.auth_scheme.as_deref(), Some("x_api_key"));
    }

    #[test]
    fn parse_model_flags() {
        let ProviderOp::Model {
            provider,
            model,
            name,
            context_window,
        } = parse("model ollama llama3.2 --context-window 32000")
        else {
            panic!("expected Model");
        };
        assert_eq!(provider, "ollama");
        assert_eq!(model, "llama3.2");
        assert_eq!(name, None);
        assert_eq!(context_window, Some(32000));
    }

    #[test]
    fn parse_errors_become_help() {
        assert!(matches!(
            parse("add onlyid"),
            ProviderOp::Help { error: Some(_) }
        ));
        assert!(matches!(
            parse("key onlyid"),
            ProviderOp::Help { error: Some(_) }
        ));
        assert!(matches!(
            parse("bogus --flag"),
            ProviderOp::Help { error: Some(_) }
        ));
    }

    #[test]
    fn rmw_inserts_provider_block() {
        let mut root = TomlMap::new();
        let table = providers_table(&mut root).unwrap();
        table.insert("gw".to_string(), TomlValue::Table(TomlMap::new()));
        let s = toml::to_string_pretty(&root).unwrap();
        assert!(s.contains("[model_providers.gw]"));
    }

    #[test]
    fn split_args_handles_quotes() {
        assert_eq!(
            split_args("model ollama llama3.2 --name 'Llama 3.2'"),
            vec!["model", "ollama", "llama3.2", "--name", "Llama 3.2"]
        );
        assert_eq!(
            split_args("add gw https://x --header \"X-Foo=a b\""),
            vec!["add", "gw", "https://x", "--header", "X-Foo=a b"]
        );
        assert_eq!(split_args(""), Vec::<String>::new());
    }

    #[test]
    fn parse_model_quoted_name() {
        let ProviderOp::Model { name, .. } = parse("model ollama llama3.2 --name 'Llama 3.2'")
        else {
            panic!("expected Model");
        };
        assert_eq!(name.as_deref(), Some("Llama 3.2"));
    }

    /// Env-based config paths are process-global, so every write-path check lives
    /// in this one `#[serial]` test to avoid racing other tests' EnvGuards.
    #[tokio::test]
    #[serial_test::serial]
    async fn write_ops_roundtrip_user_config() {
        let home = tempfile::tempdir().unwrap();
        let _home = xai_grok_test_support::env::EnvGuard::set("HOME", home.path());
        let _grok =
            xai_grok_test_support::env::EnvGuard::set("GROK_HOME", home.path().join(".grok"));
        let cfg_path = home.path().join(".grok/config.toml");

        let out = run(&parse("use ollama")).await;
        assert!(out.contains("declared"), "unexpected output: {out}");
        let written = std::fs::read_to_string(&cfg_path).unwrap();
        assert!(written.contains("[model_providers.ollama]"), "{written}");

        let out = run(&parse("model ollama llama3.2 --name 'Llama 3.2'")).await;
        assert!(out.contains("added"), "unexpected output: {out}");
        let written = std::fs::read_to_string(&cfg_path).unwrap();
        assert!(written.contains("[model.\"ollama/llama3.2\"]"), "{written}");
        assert!(written.contains("model_provider = \"ollama\""), "{written}");
        assert!(written.contains("name = \"Llama 3.2\""), "{written}");

        let out = run(&parse(
            "add gw https://gw.example/v1 --backend responses --env-key KEY1,KEY2 --header X-A=1",
        ))
        .await;
        assert!(out.contains("written"), "unexpected output: {out}");
        let written = std::fs::read_to_string(&cfg_path).unwrap();
        assert!(
            written.contains("base_url = \"https://gw.example/v1\""),
            "{written}"
        );
        assert!(written.contains("api_backend = \"responses\""), "{written}");
        assert!(written.contains("[model_providers.gw]"), "{written}");
        assert!(written.contains("env_key"), "{written}");
        assert!(written.contains("KEY1"), "{written}");
        assert!(
            written.contains("[model_providers.gw.extra_headers]"),
            "{written}"
        );
        // Earlier blocks survive the read-modify-write.
        assert!(written.contains("[model_providers.ollama]"), "{written}");
        assert!(written.contains("[model.\"ollama/llama3.2\"]"), "{written}");

        let out = run(&parse("remove ollama llama3.2")).await;
        assert!(out.contains("removed"), "unexpected output: {out}");
        let written = std::fs::read_to_string(&cfg_path).unwrap();
        assert!(!written.contains("ollama/llama3.2"), "{written}");

        let out = run(&parse("remove ollama")).await;
        assert!(out.contains("removed"), "unexpected output: {out}");
        let written = std::fs::read_to_string(&cfg_path).unwrap();
        assert!(!written.contains("[model_providers.ollama]"), "{written}");
    }
}
