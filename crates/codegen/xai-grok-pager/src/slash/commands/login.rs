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
        // The only choice is the first token: where the credential goes.
        // Everything after it is free text (the key, or `add` fields).
        let trimmed = args_query.trim_start();
        let first = trimmed.split_whitespace().next().unwrap_or("");
        if !first.is_empty()
            && (trimmed.ends_with(char::is_whitespace)
                || trimmed.split_whitespace().nth(1).is_some())
        {
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
            items.push(ArgItem {
                display: format!("{} API key", display_name(&row)),
                match_text: format!("{} {} api key", row.id, row.name),
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
            CommandResult::PassThrough(format!("/provider {args}"))
        } else {
            CommandResult::PassThrough(format!("/provider key {args}"))
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
