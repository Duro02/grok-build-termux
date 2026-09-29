//! `/login`: pi-style auth entry.
//!
//! Bare `/login` (or picking "Grok.com account") keeps the browser OAuth flow.
//! `/login ` opens a provider picker — the same catalog rows as `/provider` —
//! and choosing one hands the prompt back for the key text, which is forwarded
//! to the agent as `/provider key <id> <api-key>`.

use crate::app::actions::Action;
use crate::slash::command::{
    AppCtx, ArgItem, CommandExecCtx, CommandResult, SlashCommand, slash_meta,
};
use crate::slash::commands::provider::{ProviderRow, provider_rows, row_description};

pub struct LoginCommand;

impl SlashCommand for LoginCommand {
    slash_meta! {
        name: "login",
        description: "Log in or store a provider API key",
        usage: "/login [<provider> [<api-key>]]",
        takes_args: true,
        arg_placeholder: "[<provider> [<api-key>]]",
    }

    fn suggest_args(&self, _ctx: &AppCtx, args_query: &str) -> Option<Vec<ArgItem>> {
        let trimmed = args_query.trim_start();
        let first = trimmed.split_whitespace().next().unwrap_or("");
        // Stage 2: an OAuth-capable provider was picked — choose how to sign in.
        // (Second token means we're past stage 2 — `key`/free text follows.)
        if trimmed.ends_with(char::is_whitespace) && trimmed.split_whitespace().count() == 1 {
            if let Some(spec) = xai_grok_login::provider_oauth::spec_for(first) {
                let (display, description) = match spec.flow {
                    xai_grok_login::provider_oauth::OAuthFlow::PkceLoopback(_) => (
                        "sign in (browser)",
                        "opens a browser; manual paste supported",
                    ),
                    xai_grok_login::provider_oauth::OAuthFlow::DeviceCode(_) => (
                        "sign in (device code)",
                        "open the link on any device and enter the code",
                    ),
                };
                let mut items = vec![ArgItem {
                    display: display.to_string(),
                    match_text: format!("{first} oauth sign in login"),
                    insert_text: format!("{first} oauth"),
                    description: description.to_string(),
                }];
                if spec.device_flow.is_some()
                    && matches!(
                        spec.flow,
                        xai_grok_login::provider_oauth::OAuthFlow::PkceLoopback(_)
                    )
                {
                    items.push(ArgItem {
                        display: "device code sign-in".to_string(),
                        match_text: format!("{first} oauth device headless ssh"),
                        insert_text: format!("{first} oauth device"),
                        description: "for headless/SSH — open a link on another device".to_string(),
                    });
                }
                items.push(ArgItem {
                    display: "API key".to_string(),
                    match_text: format!("{first} api key manual"),
                    insert_text: format!("{first} key "),
                    description: "type the key next".to_string(),
                });
                return Some(items);
            }
            return None;
        }
        if !first.is_empty() && trimmed.split_whitespace().nth(1).is_some() {
            return None;
        }
        let mut items = vec![ArgItem {
            display: "Grok.com account".to_string(),
            match_text: "grok.com account xai oauth subscription".to_string(),
            insert_text: "grok.com".to_string(),
            description: "browser sign-in".to_string(),
        }];
        for row in provider_rows()
            .into_iter()
            .filter(|r| r.requires_key || !r.builtin)
        {
            let (display, match_text) = if row.oauth.is_some() {
                (
                    display_name(&row).to_string(),
                    format!("{} {} oauth sign in api key", row.id, row.name),
                )
            } else {
                (
                    format!("{} API key", display_name(&row)),
                    format!("{} {} api key", row.id, row.name),
                )
            };
            items.push(ArgItem {
                display,
                match_text,
                insert_text: format!("{} ", row.id),
                description: row_description(&row),
            });
        }
        items.push(ArgItem {
            display: "+ custom provider".to_string(),
            match_text: "add custom provider register new".to_string(),
            insert_text: "add ".to_string(),
            description: "register a provider not in the catalog".to_string(),
        });
        Some(items)
    }

    fn run(&self, ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let args = args.trim();
        if args.is_empty() || args.eq_ignore_ascii_case("grok.com") {
            return CommandResult::Action(Action::Login);
        }
        if ctx.session_id.is_none() {
            return CommandResult::Error(
                "provider keys need an active session — open one first, or run bare /login"
                    .to_string(),
            );
        }
        if args == "add" || args.starts_with("add ") {
            return CommandResult::PassThrough(format!("/provider {args}"));
        }
        // `/login <id> oauth [device]` and `/login <id> key <key>` come from
        // the stage-2 picker; bare `/login <id> <key>` still means key entry.
        let mut words = args.split_whitespace();
        let id = words.next().unwrap_or_default();
        match words.next() {
            Some("oauth") => {
                let mut cmd = format!("/provider oauth {id}");
                if let Some(method) = words.next() {
                    cmd.push(' ');
                    cmd.push_str(method);
                }
                CommandResult::PassThrough(cmd)
            }
            Some("key") => {
                let key = args
                    .split_once(char::is_whitespace)
                    .and_then(|(_, r)| r.split_once(char::is_whitespace))
                    .map(|(_, r)| r.trim().to_string())
                    .unwrap_or_default();
                if key.is_empty() {
                    CommandResult::PassThrough(format!("/provider key {id}"))
                } else {
                    CommandResult::PassThrough(format!("/provider key {id} {key}"))
                }
            }
            _ => CommandResult::PassThrough(format!("/provider key {args}")),
        }
    }
}

fn display_name(row: &ProviderRow) -> &str {
    if row.name.is_empty() {
        &row.id
    } else {
        &row.name
    }
}
