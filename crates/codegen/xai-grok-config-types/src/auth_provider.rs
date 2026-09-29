//! Model auth-provider config (`[auth_provider.<name>]`), a leaf type shared by
//! the login flow and `agent::config`. It lives here so neither side needs to
//! depend on the other.

/// One named `[auth_provider.<name>]` table, honored only from the trusted config layers (`parse_auth_providers`).
/// A new field here needs a `parse_auth_providers` warning decision.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(default)]
pub struct AuthProviderConfig {
    /// Command to run; without `args` it uses the platform shell, with `args` it execs directly.
    pub command: String,
    /// Command arguments; when set (even empty) the command execs directly.
    pub args: Option<Vec<String>>,
    /// Fallback token lifetime used when the output carries no `expires_in`.
    pub token_ttl_secs: Option<u64>,
    /// Max seconds to wait for the command (default 30, clamped to 1..=600).
    pub timeout_secs: Option<u64>,
    /// Working directory for the command; a leading `~` expands to home.
    pub cwd: Option<String>,
    /// Built-in OAuth provider id (e.g. `"openai-codex"`); when set, the mint
    /// path reads `<grok_home>/provider-auth.json` and refreshes in-process —
    /// no helper command runs. Written by `/provider oauth <id>`.
    pub oauth: Option<String>,
}

impl AuthProviderConfig {
    pub fn is_usable(&self) -> bool {
        !self.command.trim().is_empty() || self.oauth.is_some()
    }
}
