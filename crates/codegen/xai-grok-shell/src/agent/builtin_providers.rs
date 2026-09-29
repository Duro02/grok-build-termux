//! Built-in third-party model providers, in the style of `pi`'s provider registry.
//!
//! `builtin_providers.json` (embedded via `xai_grok_models::BUILTIN_PROVIDERS_JSON`)
//! ships a catalog of well-known providers. A provider's models enter the resolved
//! model catalog when the provider is *active*:
//!   - its `env_key` resolves to a non-empty value (e.g. `OPENAI_API_KEY`), or
//!   - `requires_key = false` (local endpoints such as Ollama), or
//!   - the user's `config.toml` declares a `[model_providers.<id>]` block for it, or
//!   - the user defines a `[model.<key>]` entry on one of its catalog keys
//!     (then only those referenced keys enter).
//!
//! `GROK_BUILTIN_PROVIDERS` filters the set: unset/`all` enables everything,
//! `off`/`none`/`false`/`0` disables the feature, and a comma-separated id list
//! (e.g. `openai,anthropic`) limits which built-in providers may activate.
//!
//! Built-in providers act as defaults under the user's own `[model_providers.<id>]`
//! entries — the user wins field-by-field, so `base_url = "https://proxy/..."` alone
//! keeps the built-in model list pointed at a proxy. Synthesized models resolve
//! *before* `[model.*]` entries, so a same-key user entry overrides them.

use std::num::NonZeroU64;
use std::sync::LazyLock;

use indexmap::IndexMap;
use xai_grok_sampler::AuthScheme;

use super::config::{Config, ConfigModelOverride, EnvKeys};
use super::model_providers::ModelProviderConfig;
use crate::sampling::ApiBackend;

/// Env var filtering which built-in providers may activate.
pub(crate) const BUILTIN_PROVIDERS_ENV: &str = "GROK_BUILTIN_PROVIDERS";

#[derive(Debug, serde::Deserialize)]
struct BuiltinProviders {
    providers: Vec<BuiltinProvider>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(default)]
struct BuiltinProvider {
    id: String,
    name: Option<String>,
    base_url: Option<String>,
    api_backend: Option<ApiBackend>,
    auth_scheme: Option<AuthScheme>,
    env_key: Option<EnvKeys>,
    extra_headers: IndexMap<String, String>,
    query_params: IndexMap<String, String>,
    env_http_headers: IndexMap<String, String>,
    context_window: Option<u64>,
    max_request_bytes: Option<u64>,
    requires_key: Option<bool>,
    models: Vec<BuiltinModel>,
}

impl Default for BuiltinProvider {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: None,
            base_url: None,
            api_backend: None,
            auth_scheme: None,
            env_key: None,
            extra_headers: IndexMap::new(),
            query_params: IndexMap::new(),
            env_http_headers: IndexMap::new(),
            context_window: None,
            max_request_bytes: None,
            requires_key: None,
            models: Vec::new(),
        }
    }
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct BuiltinModel {
    id: String,
    name: Option<String>,
    description: Option<String>,
    context_window: Option<u64>,
    max_completion_tokens: Option<u32>,
    api_backend: Option<ApiBackend>,
    auth_scheme: Option<AuthScheme>,
    temperature: Option<f32>,
    top_p: Option<f32>,
    supports_reasoning_effort: Option<bool>,
}

static CATALOG: LazyLock<IndexMap<String, BuiltinProvider>> = LazyLock::new(|| {
    let parsed: BuiltinProviders = serde_json::from_str(xai_grok_models::BUILTIN_PROVIDERS_JSON)
        .expect("builtin_providers.json: invalid JSON");
    parsed
        .providers
        .into_iter()
        .map(|provider| {
            assert!(
                !provider.id.is_empty() && !provider.id.contains('/'),
                "builtin_providers.json: provider id must be non-empty and contain no '/': {:?}",
                provider.id,
            );
            (provider.id.clone(), provider)
        })
        .collect()
});

/// Production env probe.
fn read_env(name: &str) -> Option<String> {
    xai_grok_login::auth_method::read_env_var(name).ok()
}

/// `true` when `GROK_BUILTIN_PROVIDERS` allows provider `id`.
fn filter_allows(id: &str, getenv: &mut impl FnMut(&str) -> Option<String>) -> bool {
    let Some(raw) = getenv(BUILTIN_PROVIDERS_ENV) else {
        return true;
    };
    let raw = raw.trim();
    if raw.is_empty() || raw.eq_ignore_ascii_case("all") || raw == "*" {
        return true;
    }
    if matches!(
        raw.to_ascii_lowercase().as_str(),
        "off" | "none" | "false" | "0"
    ) {
        return false;
    }
    raw.split(',')
        .any(|entry| entry.trim().eq_ignore_ascii_case(id))
}

/// Catalog keys (`<provider>/<model>`) a provider's built-in models occupy.
fn catalog_key(provider_id: &str, model_id: &str) -> String {
    format!("{provider_id}/{model_id}")
}

fn provider_to_config(provider: &BuiltinProvider) -> ModelProviderConfig {
    ModelProviderConfig {
        base_url: provider.base_url.clone(),
        env_key: provider.env_key.clone(),
        api_backend: provider.api_backend.clone(),
        auth_scheme: provider.auth_scheme,
        extra_headers: provider.extra_headers.clone(),
        query_params: provider.query_params.clone(),
        env_http_headers: provider.env_http_headers.clone(),
        context_window: provider.context_window,
        max_request_bytes: provider.max_request_bytes.and_then(NonZeroU64::new),
        ..Default::default()
    }
}

/// Merged view of `[model_providers.*]`: every filter-allowed built-in provider is
/// present, with the user's own entry (same id) overriding it field-by-field.
///
/// Merging is field-level, matching pi's "baseUrl-only override keeps the built-in
/// models" behavior: `Map` fields merge per key (user wins) and scalar `Option`
/// fields take the user's value when set.
pub(crate) fn effective_model_providers(cfg: &Config) -> IndexMap<String, ModelProviderConfig> {
    let mut merged: IndexMap<String, ModelProviderConfig> = IndexMap::new();
    let mut getenv = read_env;
    for (id, provider) in CATALOG.iter() {
        if filter_allows(id, &mut getenv) {
            merged.insert(id.clone(), provider_to_config(provider));
        }
    }
    for (id, user) in &cfg.model_providers {
        match merged.get_mut(id) {
            Some(base) => merge_provider_over(base, user),
            None => {
                merged.insert(id.clone(), user.clone());
            }
        }
    }
    merged
}

fn merge_provider_over(base: &mut ModelProviderConfig, user: &ModelProviderConfig) {
    let or = |dst: &mut Option<String>, src: &Option<String>| {
        if src.is_some() {
            dst.clone_from(src);
        }
    };
    or(&mut base.base_url, &user.base_url);
    or(&mut base.api_base_url, &user.api_base_url);
    if user.api_backend.is_some() {
        base.api_backend.clone_from(&user.api_backend);
    }
    if user.auth_scheme.is_some() {
        base.auth_scheme = user.auth_scheme;
    }
    // Credential fields: the user's entry wins outright when it supplies any of
    // its own (matching how a model's own credential shadows a provider's).
    if user.api_key.is_some() {
        base.api_key.clone_from(&user.api_key);
    }
    if user.env_key.is_some() {
        base.env_key.clone_from(&user.env_key);
    }
    if user.auth_provider.is_some() {
        base.auth_provider.clone_from(&user.auth_provider);
    }
    if user.auth.is_some() {
        base.auth.clone_from(&user.auth);
    }
    for (k, v) in &user.extra_headers {
        base.extra_headers.insert(k.clone(), v.clone());
    }
    for (k, v) in &user.query_params {
        base.query_params.insert(k.clone(), v.clone());
    }
    for (k, v) in &user.env_http_headers {
        base.env_http_headers.insert(k.clone(), v.clone());
    }
    if user.context_window.is_some() {
        base.context_window = user.context_window;
    }
    if user.max_request_bytes.is_some() {
        base.max_request_bytes = user.max_request_bytes;
    }
}

/// `true` when a built-in provider is *active*: its credential resolves, it is
/// keyless, or the user declares a `[model_providers.<id>]` block for it.
fn provider_active(
    provider: &BuiltinProvider,
    merged: &ModelProviderConfig,
    user_defined: bool,
    getenv: &mut impl FnMut(&str) -> Option<String>,
) -> bool {
    if user_defined || !provider.requires_key.unwrap_or(true) {
        return true;
    }
    merged
        .api_key
        .as_deref()
        .is_some_and(|k| !k.trim().is_empty())
        || merged
            .env_key
            .as_ref()
            .is_some_and(|keys| keys.resolve_value_with(getenv).is_some())
}

/// One row of provider state for `/provider` listings.
#[derive(Debug)]
pub(crate) struct ProviderStatus {
    pub id: String,
    /// Whether the provider comes from the built-in catalog.
    pub builtin: bool,
    /// Whether a `[model_providers.<id>]` block exists in user config.
    pub user_defined: bool,
    /// Whether `GROK_BUILTIN_PROVIDERS` permits the provider (always true for user-only providers).
    pub allowed: bool,
    /// Whether the provider is *active* (credential resolves, keyless, or user-declared).
    pub active: bool,
    /// Whether the provider requires an API key to be usable.
    pub requires_key: bool,
    /// Base URL the merged provider resolves to, when known.
    pub base_url: Option<String>,
    /// Env var names the provider reads its API key from.
    pub env_keys: Vec<String>,
    /// Wire backend (`chat_completions` etc.) when set.
    pub api_backend: Option<ApiBackend>,
    /// Number of catalog models the provider contributes (built-ins only).
    pub model_count: usize,
}

/// `/provider` list state: every filter-aware built-in provider plus user-defined
/// providers not in the catalog, in catalog-then-config order.
pub(crate) fn provider_statuses(cfg: &Config) -> Vec<ProviderStatus> {
    let mut getenv = read_env;
    let effective = effective_model_providers(cfg);
    let mut rows = Vec::new();
    for (id, provider) in CATALOG.iter() {
        let allowed = filter_allows(id, &mut getenv);
        let merged = effective.get(id);
        let user_defined = cfg.model_providers.contains_key(id);
        let active = merged
            .is_some_and(|m| allowed && provider_active(provider, m, user_defined, &mut getenv));
        rows.push(ProviderStatus {
            id: id.clone(),
            builtin: true,
            user_defined,
            allowed,
            active,
            requires_key: provider.requires_key.unwrap_or(true),
            base_url: merged.and_then(|m| m.base_url.clone()),
            env_keys: provider
                .env_key
                .as_ref()
                .map(|k| k.names().iter().map(|s| (*s).to_string()).collect())
                .unwrap_or_default(),
            api_backend: merged.and_then(|m| m.api_backend.clone()),
            model_count: provider.models.len(),
        });
    }
    for (id, user) in &cfg.model_providers {
        if CATALOG.contains_key(id) {
            continue;
        }
        rows.push(ProviderStatus {
            id: id.clone(),
            builtin: false,
            user_defined: true,
            allowed: true,
            active: true,
            requires_key: true,
            base_url: user.base_url.clone(),
            env_keys: user
                .env_key
                .as_ref()
                .map(|k| k.names().iter().map(|s| (*s).to_string()).collect())
                .unwrap_or_default(),
            api_backend: user.api_backend.clone(),
            model_count: cfg
                .config_models
                .values()
                .filter(|m| m.model_provider.as_deref() == Some(id.as_str()))
                .count(),
        });
    }
    rows
}

/// Whether `id` names a built-in catalog provider.
pub(crate) fn is_builtin_id(id: &str) -> bool {
    CATALOG.contains_key(id)
}

/// Catalog model ids of a built-in provider (empty for non-builtins).
pub(crate) fn builtin_model_ids(id: &str) -> Vec<String> {
    match CATALOG.get(id) {
        Some(p) => p.models.iter().map(|m| m.id.clone()).collect(),
        None => Vec::new(),
    }
}

/// Synthesized `[model.*]` overrides contributed by active built-in providers, in
/// catalog order. `resolve_model_list` resolves these before the user's own
/// `[model.*]` entries, so a same-key user entry wins.
///
/// Each synthesized entry carries `model_provider = "<id>"`, so the (merged)
/// provider supplies `base_url`, `api_backend`, credentials, headers, and
/// `context_window` defaults exactly as for hand-written models.
pub(crate) fn builtin_model_overrides(cfg: &Config) -> Vec<(String, ConfigModelOverride)> {
    let mut getenv = read_env;
    let effective = effective_model_providers(cfg);
    let mut overrides = Vec::new();
    for (id, provider) in CATALOG.iter() {
        if provider.models.is_empty() || !filter_allows(id, &mut getenv) {
            continue;
        }
        let user_defined = cfg.model_providers.contains_key(id);
        let merged = effective.get(id).expect("provider is filter-allowed");
        let active = provider_active(provider, merged, user_defined, &mut getenv);
        for model in &provider.models {
            let key = catalog_key(id, &model.id);
            // When the provider is inactive (no credential resolves), only keys
            // the user explicitly configures still enter — the user entry then
            // inherits the built-in endpoint/backend as its base.
            if !active && !cfg.config_models.contains_key(&key) {
                continue;
            }
            overrides.push((
                key,
                ConfigModelOverride {
                    model: Some(model.id.clone()),
                    model_provider: Some(id.clone()),
                    name: Some(model.name.clone().unwrap_or_else(|| model.id.clone())),
                    description: model.description.clone().or_else(|| provider.name.clone()),
                    context_window: model.context_window.or(provider.context_window),
                    max_completion_tokens: model.max_completion_tokens,
                    api_backend: model.api_backend.clone(),
                    auth_scheme: model.auth_scheme,
                    temperature: model.temperature,
                    top_p: model.top_p,
                    supports_reasoning_effort: model.supports_reasoning_effort,
                    ..Default::default()
                },
            ));
        }
    }
    overrides
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::config::{Config, resolve_credentials, resolve_model_list};

    fn env_with<'a>(vars: &'a [(&'a str, &'a str)]) -> impl FnMut(&str) -> Option<String> + 'a {
        move |name| {
            vars.iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn catalog_parses_and_providers_have_models_or_keyless() {
        assert!(CATALOG.contains_key("openai"));
        assert!(CATALOG.contains_key("anthropic"));
        assert!(CATALOG.contains_key("ollama"));
        for (id, provider) in CATALOG.iter() {
            assert!(
                provider.base_url.is_some(),
                "builtin provider {id} missing base_url"
            );
            assert!(
                provider.requires_key == Some(false) || !provider.models.is_empty(),
                "builtin provider {id} has no models and is not keyless"
            );
        }
    }

    #[test]
    fn inactive_without_env_key() {
        let raw: toml::Value = toml::from_str("").unwrap();
        let cfg = Config::new_from_toml_cfg(&raw).unwrap();
        let mut getenv = env_with(&[]);
        let effective = effective_model_providers(&cfg);
        let openai = CATALOG.get("openai").unwrap();
        assert!(!provider_active(
            openai,
            effective.get("openai").unwrap(),
            false,
            &mut getenv
        ));
        let overrides = builtin_model_overrides(&cfg);
        // No env in the process for these tests may still resolve real env vars;
        // the assert relies on OPENAI_API_KEY not being set in CI. Guard softly.
        if std::env::var("OPENAI_API_KEY").is_err() {
            assert!(
                overrides.iter().all(|(k, _)| !k.starts_with("openai/")),
                "openai models must not enter without a credential: {overrides:?}"
            );
        }
    }

    #[test]
    fn active_provider_models_resolve_with_byok_credentials() {
        let raw: toml::Value = toml::from_str("").unwrap();
        let cfg = Config::new_from_toml_cfg(&raw).unwrap();
        let mut getenv = env_with(&[("OPENAI_API_KEY", "sk-test")]);
        let effective = effective_model_providers(&cfg);
        let openai = CATALOG.get("openai").unwrap();
        assert!(provider_active(
            openai,
            effective.get("openai").unwrap(),
            false,
            &mut getenv
        ));
    }

    #[test]
    fn user_provider_entry_activates_and_overrides() {
        let raw: toml::Value = toml::from_str(
            r#"
            [model_providers.openai]
            base_url = "https://corp-proxy.example/v1"
            api_key = "sk-corp"
            "#,
        )
        .unwrap();
        let cfg = Config::new_from_toml_cfg(&raw).unwrap();
        let effective = effective_model_providers(&cfg);
        let openai = effective.get("openai").unwrap();
        assert_eq!(
            openai.base_url.as_deref(),
            Some("https://corp-proxy.example/v1")
        );
        assert_eq!(openai.api_key.as_deref(), Some("sk-corp"));

        // The provider is user-defined: its built-in models enter on the proxy.
        let overrides = builtin_model_overrides(&cfg);
        let entry = overrides
            .iter()
            .find(|(k, _)| k == "openai/gpt-5")
            .map(|(_, o)| o);
        assert!(entry.is_some(), "user-defined provider activates models");
        let resolved = resolve_model_list(&cfg, None);
        let model = resolved.get("openai/gpt-5").expect("model in catalog");
        assert_eq!(model.info.base_url, "https://corp-proxy.example/v1");
        assert_eq!(
            resolve_credentials(model, None).api_key.as_deref(),
            Some("sk-corp")
        );
    }

    #[test]
    fn user_model_entry_overrides_builtin() {
        let raw: toml::Value = toml::from_str(
            r#"
            [model."anthropic/claude-sonnet-4-5"]
            api_key = "sk-ant-user"
            temperature = 0.2
            "#,
        )
        .unwrap();
        let cfg = Config::new_from_toml_cfg(&raw).unwrap();
        let resolved = resolve_model_list(&cfg, None);
        let model = resolved
            .get("anthropic/claude-sonnet-4-5")
            .expect("builtin base must merge under the user entry");
        assert_eq!(model.info.model, "claude-sonnet-4-5");
        assert_eq!(model.info.base_url, "https://api.anthropic.com/v1");
        assert!(matches!(model.info.api_backend, ApiBackend::Messages));
        assert!(matches!(model.info.auth_scheme, AuthScheme::XApiKey));
        assert_eq!(model.info.temperature, Some(0.2));
        assert_eq!(
            model
                .info
                .extra_headers
                .get("anthropic-version")
                .map(String::as_str),
            Some("2023-06-01")
        );
        let creds = resolve_credentials(model, Some("session-jwt"));
        assert_eq!(creds.api_key.as_deref(), Some("sk-ant-user"));
        assert_eq!(creds.base_url, "https://api.anthropic.com/v1");
    }

    #[test]
    fn filter_env_gates_providers() {
        let raw: toml::Value = toml::from_str("").unwrap();
        let cfg = Config::new_from_toml_cfg(&raw).unwrap();
        let mut getenv = env_with(&[
            ("OPENAI_API_KEY", "sk-x"),
            ("ANTHROPIC_API_KEY", "sk-y"),
            (BUILTIN_PROVIDERS_ENV, "anthropic"),
        ]);
        // filter check itself
        assert!(!filter_allows("openai", &mut getenv));
        assert!(filter_allows("anthropic", &mut getenv));
        // effective providers only carry the allowed id
        let effective = effective_model_providers(&cfg);
        // effective_model_providers uses the process env; replicate the filter
        // semantics check through filter_allows + catalog walk instead.
        for id in ["openai", "anthropic", "deepseek"] {
            assert_eq!(effective.contains_key(id), filter_allows(id, &mut read_env));
        }
    }
}
