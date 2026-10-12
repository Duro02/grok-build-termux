//! `/provider` — interactive picker for model providers (pi-style).
//!
//! Staged dropdowns: stage 1 lists every catalog provider plus user-registered
//! `[model_providers.*]` blocks and a "+ custom provider" row; stage 2 offers
//! the verbs that apply to the picked provider (details / activate / api key /
//! add model / remove). Free-text stages (ids, URLs, keys, model names) return
//! no items so the dropdown closes and the user keeps typing. Execution
//! forwards the raw command to the agent-side `/provider` builtin as a prompt.
//!
//! `provider_rows()` is shared with `/login`, whose stage-1 picker reuses the
//! same provider list to choose where a key should go.

use std::sync::LazyLock;

use crate::slash::command::{
    AppCtx, ArgItem, CommandExecCtx, CommandResult, SlashCommand, slash_meta,
};

/// Same env filter the agent applies to the embedded catalog.
const BUILTIN_PROVIDERS_ENV: &str = "GROK_BUILTIN_PROVIDERS";

/// One picker row: a catalog entry or a `[model_providers.*]` config block.
pub(crate) struct ProviderRow {
    pub id: String,
    /// Display name (catalog `name`; custom providers use their id).
    pub name: String,
    /// From the embedded catalog (vs. a user-only `[model_providers.*]` block).
    pub builtin: bool,
    /// A `[model_providers.<id>]` block exists in user config.
    pub user_defined: bool,
    /// Credential resolves without `/provider key` (env set or keyless).
    pub active: bool,
    pub requires_key: bool,
    /// Resolved base URL for display (catalog or the user's block).
    pub base_url: String,
    /// Env var names the provider reads its API key from.
    pub env_keys: Vec<String>,
    /// Built-in OAuth flow id — the provider offers browser/device sign-in
    /// through `/provider oauth <id>`.
    pub oauth: Option<String>,
}

struct CatalogProvider {
    id: String,
    name: String,
    base_url: String,
    requires_key: bool,
    env_keys: Vec<String>,
    oauth: Option<String>,
}

#[derive(serde::Deserialize)]
struct JsonProvider {
    id: String,
    name: Option<String>,
    base_url: Option<String>,
    requires_key: Option<bool>,
    #[serde(default)]
    env_key: Vec<String>,
    oauth: Option<String>,
}

#[derive(serde::Deserialize)]
struct JsonCatalog {
    providers: Vec<JsonProvider>,
}

static CATALOG: LazyLock<Vec<CatalogProvider>> = LazyLock::new(|| {
    let Ok(catalog) = serde_json::from_str::<JsonCatalog>(xai_grok_models::BUILTIN_PROVIDERS_JSON)
    else {
        return Vec::new();
    };
    catalog
        .providers
        .into_iter()
        .map(|p| CatalogProvider {
            name: p.name.unwrap_or_else(|| p.id.clone()),
            base_url: p.base_url.unwrap_or_default(),
            requires_key: p.requires_key.unwrap_or(true),
            env_keys: p.env_key,
            oauth: p.oauth,
            id: p.id,
        })
        .collect()
});

/// `[model_providers]` blocks from `<grok_home>/config.toml` (id → base_url).
fn user_providers() -> Vec<(String, String)> {
    let Some(home) = xai_dirs::resolve_grok_home() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(home.join("config.toml")) else {
        return Vec::new();
    };
    providers_from_config(&text)
}

fn providers_from_config(text: &str) -> Vec<(String, String)> {
    let Ok(doc) = toml::from_str::<toml::Value>(text) else {
        return Vec::new();
    };
    doc.get("model_providers")
        .and_then(|t| t.as_table())
        .map(|t| {
            t.iter()
                .map(|(id, v)| {
                    let url = v
                        .get("base_url")
                        .and_then(|u| u.as_str())
                        .unwrap_or_default()
                        .to_string();
                    (id.clone(), url)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `GROK_BUILTIN_PROVIDERS`: unset/`all`/`*` allows everything, `off`/`none`/
/// `false`/`0` denies all, otherwise a comma-separated allowlist.
fn filter_allows(id: &str) -> bool {
    let Ok(raw) = std::env::var(BUILTIN_PROVIDERS_ENV) else {
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

/// Live provider rows for the pickers: allowed catalog providers first, then
/// user-only providers. Read on every call so a `/provider key`-style write in
/// the same session shows up immediately.
pub(crate) fn provider_rows() -> Vec<ProviderRow> {
    let user = user_providers();
    let mut rows = Vec::new();
    for p in CATALOG.iter() {
        if !filter_allows(&p.id) {
            continue;
        }
        let user_defined = user.iter().any(|(id, _)| id == &p.id);
        let env_set = p
            .env_keys
            .iter()
            .any(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty()));
        rows.push(ProviderRow {
            id: p.id.clone(),
            name: p.name.clone(),
            builtin: true,
            user_defined,
            active: user_defined
                || !p.requires_key
                || env_set
                || p.oauth.as_deref().is_some_and(|id| {
                    xai_grok_login::provider_oauth::load_credentials(id).is_some()
                }),
            requires_key: p.requires_key,
            base_url: p.base_url.clone(),
            env_keys: p.env_keys.clone(),
            oauth: p.oauth.clone(),
        });
    }
    for (id, base_url) in user {
        if CATALOG.iter().any(|p| p.id == id) {
            continue;
        }
        rows.push(ProviderRow {
            id,
            name: String::new(),
            builtin: false,
            user_defined: true,
            active: true,
            requires_key: true,
            base_url,
            env_keys: Vec::new(),
            oauth: None,
        });
    }
    rows
}

pub(crate) fn row_description(row: &ProviderRow) -> String {
    if !row.builtin {
        return format!("custom · {}", row.base_url);
    }
    if row.active && row.requires_key {
        "built-in · active".to_string()
    } else if !row.requires_key {
        "built-in · no key needed".to_string()
    } else {
        format!("built-in · set {}", row.env_keys.join(" or "))
    }
}

fn provider_item(row: &ProviderRow) -> ArgItem {
    let name = if row.name.is_empty() {
        row.id.clone()
    } else {
        row.name.clone()
    };
    let display = if row.active {
        format!("{name} (active)")
    } else {
        name.clone()
    };
    // Trailing space → the widget re-queries for the verb menu.
    ArgItem {
        display,
        match_text: format!("{} {}", row.id, name),
        insert_text: format!("{} ", row.id),
        description: row_description(row),
    }
}

/// Stage-1 items: all providers plus the custom-provider row.
fn stage_one_items() -> Vec<ArgItem> {
    let mut items: Vec<ArgItem> = provider_rows().iter().map(provider_item).collect();
    items.push(ArgItem {
        display: "+ custom provider".to_string(),
        match_text: "add custom provider register new".to_string(),
        insert_text: "add ".to_string(),
        description: "register a provider not in the catalog (id + base-url)".to_string(),
    });
    items
}

/// Stage-2 items for a picked provider: the verbs that apply to it.
/// `match_text` is `sort id verb` so a typed suffix like `openai key` still
/// narrows the menu (same trick as the `/model` effort stage).
fn verb_items(row: &ProviderRow) -> Vec<ArgItem> {
    let id = row.id.as_str();
    let mut specs: Vec<(&str, String, String)> = Vec::new();
    if row.oauth.is_some() {
        specs.push((
            "sign in (browser)",
            format!("oauth {id}"),
            "browser / device sign-in — subscription login".to_string(),
        ));
    }
    if row.requires_key {
        specs.push((
            "api key",
            format!("key {id} "),
            "store a key on this provider (typed next)".to_string(),
        ));
    }
    if row.builtin && !row.user_defined {
        specs.push((
            "activate",
            format!("use {id}"),
            if row.env_keys.is_empty() {
                "enable this provider".to_string()
            } else {
                format!("enable without exporting {}", row.env_keys.join(" or "))
            },
        ));
    }
    specs.push((
        "details",
        id.to_string(),
        "status, backend and models".to_string(),
    ));
    specs.push((
        "add model",
        format!("model {id} "),
        "register a model id under this provider".to_string(),
    ));
    if row.user_defined {
        specs.push((
            "remove",
            format!("remove {id}"),
            format!("delete the [model_providers.{id}] block"),
        ));
    }
    specs
        .into_iter()
        .enumerate()
        .map(|(i, (display, insert_text, description))| {
            let sort = char::from(b'a' + i as u8);
            ArgItem {
                match_text: format!("{sort} {id} {display}"),
                display: display.to_string(),
                insert_text,
                description,
            }
        })
        .collect()
}

/// Model keys `<id>/<model>` the user configured — the third arg of `remove`.
fn removable_models(id: &str) -> Vec<String> {
    let Some(home) = xai_dirs::resolve_grok_home() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(home.join("config.toml")) else {
        return Vec::new();
    };
    removable_models_from_config(&text, id)
}

fn removable_models_from_config(text: &str, id: &str) -> Vec<String> {
    let Ok(doc) = toml::from_str::<toml::Value>(text) else {
        return Vec::new();
    };
    let prefix = format!("{id}/");
    doc.get("model")
        .and_then(|t| t.as_table())
        .map(|t| {
            t.keys()
                .filter_map(|k| k.strip_prefix(&prefix).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Stage ≥3 for verb-first typed args like `use `, `key o`, `remove openai `.
/// `rest` is the raw text after the verb (leading space included); the slot
/// index being typed is `tokens.len()` when the text ends in whitespace.
fn verb_arg_items(verb: &str, rest: &str) -> Option<Vec<ArgItem>> {
    let rows = provider_rows();
    let tokens: Vec<&str> = rest.split_whitespace().collect();
    let slot = if rest.ends_with(char::is_whitespace) || tokens.is_empty() {
        tokens.len()
    } else {
        tokens.len() - 1
    };
    let provider_item_for = |verb: &str, terminal: bool, r: &ProviderRow| ArgItem {
        display: r.id.clone(),
        match_text: format!("{verb} {}", r.id),
        insert_text: if terminal {
            format!("{verb} {}", r.id)
        } else {
            format!("{verb} {} ", r.id)
        },
        description: row_description(r),
    };
    match verb {
        // `/provider use <id>`: only built-ins not already in the config.
        "use" => (slot == 0).then(|| {
            rows.iter()
                .filter(|r| r.builtin && !r.user_defined)
                .map(|r| provider_item_for(verb, true, r))
                .collect()
        }),
        // `/provider oauth <id>`: OAuth-capable providers only.
        "oauth" => (slot == 0).then(|| {
            rows.iter()
                .filter(|r| r.oauth.is_some())
                .map(|r| provider_item_for(verb, true, r))
                .collect()
        }),
        // `/provider key|model <id> <free text>`: complete the id with a
        // trailing space so the chain marker hands the last arg to the user.
        "key" | "model" => (slot == 0).then(|| {
            rows.iter()
                .map(|r| provider_item_for(verb, false, r))
                .collect()
        }),
        // `/provider remove <id>` (whole block) or `remove <id> <model>`.
        "remove" | "rm" => match slot {
            0 => Some(
                rows.iter()
                    .filter(|r| r.user_defined)
                    .map(|r| provider_item_for("remove", true, r))
                    .collect(),
            ),
            1 => {
                let id = tokens.first().copied()?;
                Some(
                    removable_models(id)
                        .into_iter()
                        .map(|m| ArgItem {
                            display: format!("{id}/{m}"),
                            match_text: format!("remove {id} {m}"),
                            insert_text: format!("remove {id} {m}"),
                            description: "remove this model".to_string(),
                        })
                        .collect(),
                )
            }
            _ => None,
        },
        _ => None,
    }
}

/// Manage model providers via a staged picker, then forward to the agent.
pub struct ProviderCommand;

impl SlashCommand for ProviderCommand {
    slash_meta! {
        name: "provider",
        description: "Manage model providers",
        usage: "/provider [<id> | <verb>]",
        takes_args: true,
        session_scoped: true,
        arg_placeholder: "[<id> | <verb>]",
    }

    fn suggest_args(&self, _ctx: &AppCtx, args_query: &str) -> Option<Vec<ArgItem>> {
        let trimmed = args_query.trim_start();
        let first = trimmed.split_whitespace().next().unwrap_or("");
        if first.is_empty()
            || (!trimmed.ends_with(char::is_whitespace)
                && trimmed.split_whitespace().nth(1).is_none())
        {
            // Empty or still typing the first token: provider list.
            return Some(stage_one_items());
        }
        let rest = &trimmed[first.len()..];
        if first.eq_ignore_ascii_case("add") {
            // "add <id> <base-url> [flags]" is all free text.
            return None;
        }
        if ["use", "key", "model", "remove", "rm", "oauth"].contains(&first) {
            return verb_arg_items(first, rest);
        }
        // "<id>" picked or typed: the verb menu. Extra text just narrows it.
        provider_rows()
            .iter()
            .find(|r| r.id == first)
            .map(verb_items)
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let args = args.trim();
        if args.is_empty() {
            return CommandResult::PassThrough("/provider".to_string());
        }
        CommandResult::PassThrough(format!("/provider {args}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::model_state::ModelState;

    fn ctx<'a>(models: &'a ModelState) -> AppCtx<'a> {
        AppCtx {
            models,
            cwd: std::path::Path::new("."),
            has_session_announcements: false,
            billing_surface_visible: true,
            usage_command_visible: true,
            workflows_available: false,
            saved_workflows: &[],
            workflow_runs: &[],
            screen_mode: crate::app::ScreenMode::Fullscreen,
            current_title: None,
        }
    }

    #[test]
    fn stage_one_lists_catalog_and_custom_row() {
        let models = ModelState::default();
        let ctx = ctx(&models);
        let items = ProviderCommand.suggest_args(&ctx, "").unwrap();
        let openai = items
            .iter()
            .find(|i| i.match_text.contains("openai"))
            .unwrap();
        assert!(openai.insert_text.ends_with(' '), "chains into verbs");
        assert!(items.iter().any(|i| i.insert_text == "add "));
    }

    #[test]
    fn picked_provider_shows_verbs() {
        let models = ModelState::default();
        let ctx = ctx(&models);
        let items = ProviderCommand.suggest_args(&ctx, "openai ").unwrap();
        assert!(items.iter().any(|i| i.insert_text == "key openai "));
        assert!(items.iter().any(|i| i.insert_text == "use openai"));
        assert!(items.iter().any(|i| i.insert_text == "openai"));
    }

    #[test]
    fn free_text_stages_close_the_menu() {
        let models = ModelState::default();
        let ctx = ctx(&models);
        assert!(ProviderCommand.suggest_args(&ctx, "add ").is_none());
        assert!(ProviderCommand.suggest_args(&ctx, "key openai ").is_none());
        assert!(ProviderCommand.suggest_args(&ctx, "nope extra").is_none());
    }

    #[test]
    fn verb_first_typed_args_suggest_ids() {
        let models = ModelState::default();
        let ctx = ctx(&models);
        let items = ProviderCommand.suggest_args(&ctx, "use ").unwrap();
        assert!(items.iter().any(|i| i.insert_text == "use ollama"));
        let items = ProviderCommand.suggest_args(&ctx, "key ").unwrap();
        assert!(items.iter().any(|i| i.insert_text == "key openai "));
    }

    #[test]
    fn config_parse_reads_documents_not_values() {
        // `text.parse::<toml::Value>()` only parses value expressions and
        // always fails on real config.toml documents — regression test.
        let text = r#"
[model_providers.myapi]
base_url = "https://api.example.com/v1"

[model."myapi/cool-1"]
model = "cool-1"

[model_providers.other]
"#;
        let providers = providers_from_config(text);
        assert!(providers.iter().any(|(id, _)| id == "myapi"));
        assert!(providers.iter().any(|(id, _)| id == "other"));
        assert_eq!(removable_models_from_config(text, "myapi"), vec!["cool-1"]);
        assert!(removable_models_from_config(text, "other").is_empty());
    }

    #[test]
    fn run_forwards_to_agent_builtin() {
        let models = ModelState::default();
        let mut exec = crate::slash::command::CommandExecCtx {
            models: &models,
            session_id: None,
            bundle_state: &crate::app::bundle::BundleState {
                has_cache: false,
                version: String::new(),
                personas: Vec::new(),
                roles: Vec::new(),
                agents: Vec::new(),
                skills: Vec::new(),
                persona_details: Vec::new(),
                role_details: Vec::new(),
            },
            screen_mode: crate::app::ScreenMode::Inline,
            billing_surface_visible: true,
            usage_command_visible: true,
            pager_state: crate::settings::PagerLocalSnapshot::default(),
        };
        match ProviderCommand.run(&mut exec, "key openai sk-x") {
            CommandResult::PassThrough(text) => {
                assert_eq!(text, "/provider key openai sk-x")
            }
            other => panic!("expected PassThrough, got {other:?}"),
        }
    }
}
