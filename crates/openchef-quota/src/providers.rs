//! The Claude and `ChatGPT` probes: endpoints, response parsing and the CLIs' stored
//! logins (§10.1, §10.2).
//!
//! Logins are only read: OpenChef never refreshes, stores, logs or forwards tokens, and
//! no error message here contains one.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use openchef_runtime::BoxFuture;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};
use serde::Deserialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::Error;
use crate::probe::{HttpsFetcher, Prober};
use crate::state::{Observation, Source, Window, canonical, clamp};

/// Claude's subscription usage endpoint (undocumented; §10.2 risk policy).
pub const CLAUDE_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
/// The beta flag the Claude endpoint requires.
const CLAUDE_BETA: &str = "oauth-2025-04-20";
/// The `ChatGPT` plan usage endpoint (undocumented; the Codex app-server's
/// `account/rateLimits/read` is preferred where a session is running).
pub const CHATGPT_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";

const FIVE_HOURS: Duration = Duration::from_hours(5);
const SIX_HOURS: Duration = Duration::from_hours(6);
const SIX_DAYS: Duration = Duration::from_hours(6 * 24);
const SEVEN_DAYS: Duration = Duration::from_hours(7 * 24);

/// A bearer token. Its `Debug` hides the value so it cannot end up in a log line.
#[derive(Clone, PartialEq, Eq)]
pub struct Token(String);

impl Token {
    /// Wraps a token read from a CLI's login.
    #[must_use]
    pub const fn new(token: String) -> Self {
        Self(token)
    }

    /// The token itself, for the provider's `Authorization` header only.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

/// A CLI's stored login.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Login {
    /// The OAuth access token.
    pub token: Token,
    /// The account id, where the provider needs one (`ChatGPT`).
    pub account: Option<String>,
}

/// Reads a CLI's stored login. Implementations only read; they never refresh.
pub trait Credentials: Send + Sync {
    /// The current login, or [`Error::NoCredentials`] when there is none usable.
    fn read(&self) -> BoxFuture<'_, Result<Login, Error>>;
}

/// Claude's subscription usage probe with Claude Code's OAuth login.
pub fn claude_probe(
    pool: impl Into<String>,
    credentials: Box<dyn Credentials>,
) -> Result<Prober, Error> {
    let fetcher = HttpsFetcher::new(CLAUDE_USAGE_URL, credentials, claude_headers)?;
    Ok(Prober::new(pool, "claude", Box::new(fetcher), parse_claude))
}

fn claude_headers(login: &Login) -> Result<HeaderMap, Error> {
    let mut h = HeaderMap::new();
    h.insert(AUTHORIZATION, bearer(&login.token)?);
    h.insert(
        HeaderName::from_static("anthropic-beta"),
        HeaderValue::from_static(CLAUDE_BETA),
    );
    Ok(h)
}

/// The `ChatGPT` plan usage probe with Codex's stored login: the fallback where no Codex
/// app-server session reports rate limits (§10.2).
pub fn chatgpt_probe(
    pool: impl Into<String>,
    credentials: Box<dyn Credentials>,
) -> Result<Prober, Error> {
    let fetcher = HttpsFetcher::new(CHATGPT_USAGE_URL, credentials, chatgpt_headers)?;
    Ok(Prober::new(
        pool,
        "chatgpt",
        Box::new(fetcher),
        parse_chatgpt,
    ))
}

fn chatgpt_headers(login: &Login) -> Result<HeaderMap, Error> {
    let mut h = HeaderMap::new();
    h.insert(AUTHORIZATION, bearer(&login.token)?);
    if let Some(account) = login.account.as_deref().filter(|a| !a.is_empty()) {
        let v = HeaderValue::from_str(account)
            .map_err(|_| no_credentials("the ChatGPT account id is not a valid header value"))?;
        h.insert(HeaderName::from_static("chatgpt-account-id"), v);
    }
    Ok(h)
}

/// The `Authorization` value, marked sensitive so HTTP-level debugging never prints it.
fn bearer(token: &Token) -> Result<HeaderValue, Error> {
    let mut v = HeaderValue::from_str(&format!("Bearer {}", token.expose()))
        .map_err(|_| no_credentials("the stored token is not a valid header value"))?;
    v.set_sensitive(true);
    Ok(v)
}

fn no_credentials(reason: &str) -> Error {
    Error::NoCredentials {
        reason: reason.to_owned(),
    }
}

fn shape(probe: &str, reason: impl Into<String>) -> Error {
    Error::ShapeChanged {
        probe: probe.to_owned(),
        reason: reason.into(),
    }
}

#[derive(Deserialize)]
struct ClaudeUsage {
    five_hour: Option<ClaudeWindow>,
    seven_day: Option<ClaudeWindow>,
}

#[derive(Deserialize)]
struct ClaudeWindow {
    utilization: Option<f64>,
    resets_at: Option<String>,
}

/// Reads Claude's `five_hour` and `seven_day` windows: utilization as a percentage,
/// resets as RFC 3339 (phase-0 findings §1). Every other field is ignored; no window
/// object, or one without utilization, is a shape change.
pub fn parse_claude(body: &[u8], at: OffsetDateTime) -> Result<Observation, Error> {
    let raw: ClaudeUsage =
        serde_json::from_slice(body).map_err(|e| shape("claude", format!("decode: {e}")))?;
    if raw.five_hour.is_none() && raw.seven_day.is_none() {
        return Err(shape("claude", "neither five_hour nor seven_day present"));
    }
    let mut windows = Vec::new();
    for (name, duration, w) in [
        ("five_hour", FIVE_HOURS, raw.five_hour),
        ("weekly", SEVEN_DAYS, raw.seven_day),
    ] {
        if let Some(w) = w {
            windows.push(claude_window(name, duration, &w)?);
        }
    }
    Ok(Observation {
        pool: String::new(),
        source: Source::Probe,
        at,
        windows,
        exhausted: false,
    })
}

fn claude_window(name: &str, duration: Duration, w: &ClaudeWindow) -> Result<Window, Error> {
    let utilization = w
        .utilization
        .ok_or_else(|| shape("claude", format!("{name}.utilization missing")))?;
    let resets_at = match w.resets_at.as_deref() {
        None | Some("") => None,
        Some(s) => Some(
            OffsetDateTime::parse(s, &Rfc3339)
                .map_err(|e| shape("claude", format!("{name}.resets_at: {e}")))?,
        ),
    };
    Ok(Window {
        name: name.to_owned(),
        utilization: clamp(utilization / 100.0),
        resets_at,
        duration: Some(duration),
    })
}

#[derive(Deserialize)]
struct ChatgptUsage {
    rate_limit: Option<ChatgptRateLimit>,
}

#[derive(Deserialize)]
struct ChatgptRateLimit {
    limit_reached: Option<bool>,
    primary_window: Option<ChatgptWindow>,
    secondary_window: Option<ChatgptWindow>,
}

#[derive(Deserialize)]
struct ChatgptWindow {
    used_percent: Option<f64>,
    limit_window_seconds: Option<i64>,
    reset_at: Option<i64>,
}

/// Reads the `ChatGPT` response's `rate_limit.{limit_reached, primary_window, secondary_window}`; each
/// window has `used_percent`, `limit_window_seconds` and `reset_at` (epoch seconds).
pub fn parse_chatgpt(body: &[u8], at: OffsetDateTime) -> Result<Observation, Error> {
    let raw: ChatgptUsage =
        serde_json::from_slice(body).map_err(|e| shape("chatgpt", format!("decode: {e}")))?;
    let Some(rl) = raw.rate_limit else {
        return Err(shape("chatgpt", "rate_limit missing"));
    };
    let (Some(limit_reached), Some(primary)) = (rl.limit_reached, rl.primary_window) else {
        return Err(shape(
            "chatgpt",
            "rate_limit.limit_reached or primary_window missing",
        ));
    };
    let mut windows = vec![chatgpt_window("primary", &primary)?];
    // Free plans have no secondary window.
    if let Some(secondary) = rl.secondary_window {
        windows.push(chatgpt_window("secondary", &secondary)?);
    }
    Ok(Observation {
        pool: String::new(),
        source: Source::Probe,
        at,
        windows,
        exhausted: limit_reached,
    })
}

fn chatgpt_window(name: &str, w: &ChatgptWindow) -> Result<Window, Error> {
    let (Some(used), Some(reset)) = (w.used_percent, w.reset_at) else {
        return Err(shape(
            "chatgpt",
            format!("{name}_window.used_percent or reset_at missing"),
        ));
    };
    let resets_at = OffsetDateTime::from_unix_timestamp(reset)
        .map_err(|e| shape("chatgpt", format!("{name}_window.reset_at: {e}")))?;
    let duration = w
        .limit_window_seconds
        .and_then(|s| u64::try_from(s).ok())
        .filter(|&s| s > 0)
        .map(Duration::from_secs);
    // Name by length where known: the 5 h window is "five_hour" however it is listed.
    let name = match duration {
        Some(d) if d <= SIX_HOURS => "five_hour".to_owned(),
        Some(d) if d >= SIX_DAYS => "weekly".to_owned(),
        Some(_) | None => canonical(name),
    };
    Ok(Window {
        name,
        utilization: clamp(used / 100.0),
        resets_at: Some(resets_at),
        duration,
    })
}

/// The directory a CLI keeps its login in: `$<var>` if set, else `<home>/<fallback>`.
fn config_dir(
    home: &Path,
    env: impl Fn(&str) -> Option<String>,
    var: &str,
    fallback: &str,
) -> PathBuf {
    env(var)
        .filter(|d| !d.is_empty())
        .map_or_else(|| home.join(fallback), PathBuf::from)
}

/// Claude Code's OAuth login: the macOS keychain item `Claude Code-credentials`,
/// otherwise `<dir>/.credentials.json`. An expired token is reported as missing; it is
/// never refreshed.
#[derive(Debug, Clone)]
pub struct ClaudeCredentials {
    dir: PathBuf,
    keychain: bool,
    now: fn() -> OffsetDateTime,
}

/// Claude Code's login for `home`, in `<CLAUDE_CONFIG_DIR or ~/.claude>`; `env` looks up
/// environment variables (`|k| std::env::var(k).ok()` in production).
pub fn claude_credentials(home: &Path, env: impl Fn(&str) -> Option<String>) -> ClaudeCredentials {
    ClaudeCredentials {
        keychain: cfg!(target_os = "macos"),
        ..ClaudeCredentials::from_dir(&config_dir(home, env, "CLAUDE_CONFIG_DIR", ".claude"))
    }
}

impl ClaudeCredentials {
    /// Only the credentials file in `dir`, never the keychain.
    #[must_use]
    pub fn from_dir(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
            keychain: false,
            now: OffsetDateTime::now_utc,
        }
    }

    /// Uses `now` to judge expiry instead of the system clock.
    #[must_use]
    pub const fn with_clock(mut self, now: fn() -> OffsetDateTime) -> Self {
        self.now = now;
        self
    }

    async fn read_login(&self) -> Result<Login, Error> {
        if self.keychain
            && let Some(raw) = keychain_item().await
        {
            return claude_token(&raw, (self.now)());
        }
        let raw = read_secret(&self.dir.join(".credentials.json")).await?;
        claude_token(&raw, (self.now)())
    }
}

impl Credentials for ClaudeCredentials {
    fn read(&self) -> BoxFuture<'_, Result<Login, Error>> {
        Box::pin(self.read_login())
    }
}

/// The keychain item, or `None` when it cannot be read (not macOS, no item, locked
/// keychain, timeout): the file is the fallback either way.
async fn keychain_item() -> Option<Vec<u8>> {
    // Arguments, never a shell: nothing here is interpolated.
    let out = tokio::process::Command::new("/usr/bin/security")
        .args([
            "find-generic-password",
            "-s",
            "Claude Code-credentials",
            "-w",
        ])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(crate::probe::PROBE_TIMEOUT, out).await {
        Ok(Ok(out)) if out.status.success() => Some(out.stdout),
        Ok(Ok(_) | Err(_)) | Err(_) => None,
    }
}

#[derive(Deserialize)]
struct ClaudeLogin {
    #[serde(rename = "claudeAiOauth")]
    oauth: Option<ClaudeOauth>,
}

#[derive(Deserialize)]
struct ClaudeOauth {
    #[serde(rename = "accessToken", default)]
    access_token: String,
    /// Epoch milliseconds.
    #[serde(rename = "expiresAt", default)]
    expires_at: i64,
}

fn claude_token(raw: &[u8], now: OffsetDateTime) -> Result<Login, Error> {
    let oauth = serde_json::from_slice::<ClaudeLogin>(trim(raw))
        .ok()
        .and_then(|c| c.oauth)
        .filter(|o| !o.access_token.is_empty())
        .ok_or_else(|| no_credentials("Claude Code login has no OAuth token"))?;
    if oauth.expires_at > 0 {
        let expires =
            OffsetDateTime::from_unix_timestamp_nanos(i128::from(oauth.expires_at) * 1_000_000)
                .map_err(|_| no_credentials("Claude Code login has an invalid expiry"))?;
        if now > expires {
            return Err(no_credentials(
                "Claude Code login expired (run claude once to refresh it)",
            ));
        }
    }
    Ok(Login {
        token: Token::new(oauth.access_token),
        account: None,
    })
}

/// Codex's `ChatGPT` login from `<dir>/auth.json`.
#[derive(Debug, Clone)]
pub struct CodexCredentials {
    dir: PathBuf,
}

/// Codex's login for `home`, in `<CODEX_HOME or ~/.codex>`; `env` looks up environment
/// variables.
pub fn codex_credentials(home: &Path, env: impl Fn(&str) -> Option<String>) -> CodexCredentials {
    CodexCredentials {
        dir: config_dir(home, env, "CODEX_HOME", ".codex"),
    }
}

#[derive(Deserialize)]
struct CodexLogin {
    tokens: Option<CodexTokens>,
}

#[derive(Deserialize)]
struct CodexTokens {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    account_id: String,
}

impl CodexCredentials {
    async fn read_login(&self) -> Result<Login, Error> {
        let raw = read_secret(&self.dir.join("auth.json")).await?;
        let tokens = serde_json::from_slice::<CodexLogin>(&raw)
            .ok()
            .and_then(|c| c.tokens)
            .filter(|t| !t.access_token.is_empty())
            .ok_or_else(|| {
                no_credentials("Codex has no ChatGPT login (API-key logins have no plan usage)")
            })?;
        Ok(Login {
            token: Token::new(tokens.access_token),
            account: Some(tokens.account_id).filter(|a| !a.is_empty()),
        })
    }
}

impl Credentials for CodexCredentials {
    fn read(&self) -> BoxFuture<'_, Result<Login, Error>> {
        Box::pin(self.read_login())
    }
}

/// Reads a CLI's documented credential file, read-only. A missing file is "no
/// login"; the error never contains the file's content.
async fn read_secret(path: &Path) -> Result<Vec<u8>, Error> {
    match tokio::fs::read(path).await {
        Ok(b) => Ok(trim(&b).to_vec()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Err(Error::NoCredentials {
            reason: format!("{} not found", path.display()),
        }),
        Err(e) => Err(Error::ReadCredentials {
            path: path.to_path_buf(),
            reason: e.to_string(),
        }),
    }
}

fn trim(b: &[u8]) -> &[u8] {
    b.trim_ascii()
}
