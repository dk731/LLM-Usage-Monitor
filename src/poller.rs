use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::ffi::c_void;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use std::os::windows::process::CommandExt;

use crate::diagnose;
use crate::localization::Strings;
use crate::models::{AppUsageData, Provider, UsageData, UsageSection, PROVIDER_COUNT};

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const KIMI_REFRESH_URL: &str = "https://www.kimi.com/api/auth/token/refresh";
const KIMI_STATS_URL: &str =
    "https://www.kimi.com/apiv2/kimi.gateway.membership.v2.MembershipService/GetSubscriptionStats";
/// Kimi's web client sends a browser UA; the gateway rejects requests without one.
const KIMI_USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) \
     Chrome/151.0.0.0 Safari/537.36";
/// Refresh the access token this long before it actually expires.
const KIMI_ACCESS_TOKEN_SKEW_SECS: u64 = 300;
/// Reports credits consumed for the signed-in seat. Uses the Copilot editor's
/// own OAuth token, so it needs no personal access token.
const COPILOT_QUOTA_URL: &str = "https://api.github.com/copilot_internal/user";
const COPILOT_API_VERSION: &str = "2022-11-28";
const ANTIGRAVITY_CREDENTIAL_TARGET: &str = "gemini:antigravity";
const ANTIGRAVITY_ENDPOINTS: &[&str] = &[
    "https://daily-cloudcode-pa.googleapis.com",
    "https://daily-cloudcode-pa.sandbox.googleapis.com",
    "https://cloudcode-pa.googleapis.com",
];
const CREATE_NO_WINDOW: u32 = 0x08000000;

const MODEL_FALLBACK_CHAIN: &[&str] = &["claude-3-haiku-20240307", "claude-haiku-4-5-20251001"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PollError {
    AuthRequired,
    NoCredentials,
    TokenExpired,
    RequestFailed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialWatchMode {
    ActiveSource,
    AllSources,
    Antigravity,
}

pub type CredentialWatchSnapshot = Vec<String>;

#[derive(Deserialize)]
struct UsageResponse {
    five_hour: Option<UsageBucket>,
    seven_day: Option<UsageBucket>,
}

#[derive(Deserialize)]
struct UsageBucket {
    utilization: f64,
    resets_at: Option<String>,
}

#[derive(Deserialize)]
struct CodexAuthFile {
    tokens: Option<CodexTokenData>,
}

#[derive(Clone, Deserialize)]
struct CodexTokenData {
    access_token: String,
    account_id: Option<String>,
}

#[derive(Deserialize)]
struct CodexUsageResponse {
    rate_limit: Option<Option<Box<CodexRateLimitDetails>>>,
}

#[derive(Deserialize)]
struct CodexRateLimitDetails {
    primary_window: Option<Option<Box<CodexRateLimitWindow>>>,
    secondary_window: Option<Option<Box<CodexRateLimitWindow>>>,
}

#[derive(Deserialize)]
struct CodexRateLimitWindow {
    used_percent: f64,
    reset_at: i64,
}

#[derive(Deserialize)]
struct AntigravityAuthFile {
    token: AntigravityTokenData,
}

#[derive(Deserialize)]
struct AntigravityTokenData {
    access_token: String,
}

#[derive(Deserialize)]
struct AntigravityLoadResponse {
    #[serde(rename = "cloudaicompanionProject")]
    project: Option<String>,
}

#[derive(Deserialize)]
struct AntigravityModelsResponse {
    models: HashMap<String, AntigravityModelInfo>,
}

#[derive(Deserialize)]
struct AntigravityModelInfo {
    #[serde(rename = "quotaInfo")]
    quota_info: Option<AntigravityQuotaInfo>,
}

#[derive(Deserialize)]
struct AntigravityQuotaInfo {
    #[serde(rename = "remainingFraction")]
    remaining_fraction: Option<f64>,
    #[serde(rename = "resetTime")]
    reset_time: Option<String>,
}

#[derive(Deserialize)]
struct AntigravityQuotaSummaryResponse {
    groups: Option<Vec<AntigravityQuotaSummaryGroup>>,
}

#[derive(Deserialize)]
struct AntigravityQuotaSummaryGroup {
    #[serde(rename = "displayName")]
    display_name: Option<String>,
    description: Option<String>,
    buckets: Option<Vec<AntigravityQuotaSummaryBucket>>,
}

#[derive(Clone, Deserialize)]
struct AntigravityQuotaSummaryBucket {
    #[serde(rename = "bucketId")]
    bucket_id: Option<String>,
    #[serde(rename = "displayName")]
    display_name: Option<String>,
    window: Option<String>,
    #[serde(rename = "remainingFraction")]
    remaining_fraction: Option<f64>,
    #[serde(rename = "resetTime")]
    reset_time: Option<String>,
}

/// On-disk Kimi credentials. `refresh_token` is supplied by the user (copied
/// from the Kimi web app's local storage); the access token is a cache that the
/// poller maintains itself.
#[derive(Default, Deserialize, serde::Serialize)]
struct KimiCredentialFile {
    #[serde(default)]
    refresh_token: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    access_expires_at: Option<u64>,
}

#[derive(Deserialize)]
struct KimiRefreshResponse {
    access_token: String,
    /// Kimi mints a fresh refresh token on every call. The previous one stays
    /// valid, so persisting this is an optimisation (it rolls the 90-day
    /// expiry forward) rather than a correctness requirement.
    #[serde(default)]
    refresh_token: Option<String>,
}

#[derive(Deserialize)]
struct KimiStatsResponse {
    #[serde(rename = "ratelimitCode5h")]
    ratelimit_5h: Option<KimiRateLimit>,
    #[serde(rename = "ratelimitCode7d")]
    ratelimit_7d: Option<KimiRateLimit>,
}

#[derive(Deserialize)]
struct KimiRateLimit {
    /// Fraction of the window consumed, 0.0–1.0.
    #[serde(default)]
    ratio: f64,
    #[serde(default)]
    enabled: bool,
    #[serde(rename = "resetTime")]
    reset_time: Option<String>,
}

/// On-disk Copilot configuration. Only `org` and `token` are needed for the
/// budget row; `included_credits` supplies the denominator for the credits row,
/// which GitHub does not expose through any API.
#[derive(Default, Deserialize)]
struct CopilotConfigFile {
    #[serde(default)]
    token: String,
    #[serde(default)]
    org: String,
    #[serde(default)]
    included_credits: Option<f64>,
}

/// Shape of `~/AppData/Local/github-copilot/apps.json`, written by the editor
/// extensions when you sign in to Copilot.
#[derive(Deserialize)]
struct CopilotAppEntry {
    oauth_token: String,
}

#[derive(Deserialize)]
struct CopilotQuotaResponse {
    quota_reset_date: Option<String>,
    quota_snapshots: Option<HashMap<String, CopilotQuotaSnapshot>>,
}

#[derive(Deserialize)]
struct CopilotQuotaSnapshot {
    #[serde(default)]
    credits_used: f64,
}

#[derive(Deserialize)]
struct CopilotBudgetsResponse {
    #[serde(default)]
    budgets: Vec<CopilotBudget>,
}

#[derive(Deserialize)]
struct CopilotBudget {
    #[serde(default)]
    budget_product_sku: String,
    #[serde(default)]
    budget_amount: f64,
}

#[derive(Deserialize)]
struct CopilotUsageResponse {
    #[serde(rename = "usageItems", default)]
    usage_items: Vec<CopilotUsageItem>,
}

#[derive(Deserialize)]
struct CopilotUsageItem {
    #[serde(default)]
    product: String,
    #[serde(rename = "netAmount", default)]
    net_amount: f64,
}

#[repr(C)]
struct CredentialW {
    flags: u32,
    type_: u32,
    target_name: *mut u16,
    comment: *mut u16,
    last_written: u64,
    credential_blob_size: u32,
    credential_blob: *mut u8,
    persist: u32,
    attribute_count: u32,
    attributes: *mut c_void,
    target_alias: *mut u16,
    user_name: *mut u16,
}

#[link(name = "Advapi32")]
extern "system" {
    fn CredReadW(
        target_name: *const u16,
        type_: u32,
        reserved_flags: u32,
        credential: *mut *mut CredentialW,
    ) -> i32;
    fn CredFree(buffer: *mut c_void);
}

pub fn poll(enabled: [bool; PROVIDER_COUNT]) -> Result<AppUsageData, PollError> {
    poll_with(enabled, |provider| match provider {
        Provider::ClaudeCode => poll_claude_code(),
        Provider::Codex => poll_codex(),
        Provider::Antigravity => poll_antigravity(),
        Provider::Kimi => poll_kimi(),
        Provider::Copilot => poll_copilot(),
    })
}

fn poll_with(
    enabled: [bool; PROVIDER_COUNT],
    mut poll_provider: impl FnMut(Provider) -> Result<UsageData, PollError>,
) -> Result<AppUsageData, PollError> {
    let mut data = AppUsageData::default();
    let mut first_error = None;
    let active_provider_count = enabled.iter().filter(|on| **on).count();

    for provider in Provider::ALL {
        if !enabled[provider.index()] {
            continue;
        }

        match poll_provider(provider) {
            Ok(usage) => data.set(provider, usage),
            Err(error) => {
                // With a single provider the widget surfaces the error itself,
                // so only log when a failure would otherwise be invisible.
                if active_provider_count > 1 {
                    diagnose::log(format!(
                        "{} usage poll failed: {error:?}",
                        provider.log_name()
                    ));
                }
                first_error.get_or_insert(error);
            }
        }
    }

    if data.is_empty() {
        Err(first_error.unwrap_or(PollError::RequestFailed))
    } else {
        Ok(data)
    }
}

fn poll_claude_code() -> Result<UsageData, PollError> {
    let creds = match read_first_credentials() {
        Some(c) => c,
        None => {
            diagnose::log("poll failed: no Claude credentials found");
            return Err(PollError::NoCredentials);
        }
    };

    let creds = refresh_or_fallback(creds)?;

    fetch_usage_with_fallback(&creds.access_token)
}

fn poll_codex() -> Result<UsageData, PollError> {
    let creds = match read_codex_credentials() {
        Some(creds) => creds,
        None => {
            diagnose::log("Codex usage poll failed: no Codex credentials found");
            return Err(PollError::NoCredentials);
        }
    };

    match fetch_codex_usage(&creds.access_token, creds.account_id.as_deref()) {
        Ok(data) => Ok(data),
        Err(PollError::AuthRequired) => {
            cli_refresh_codex_token();
            let refreshed = read_codex_credentials().ok_or(PollError::TokenExpired)?;
            fetch_codex_usage(&refreshed.access_token, refreshed.account_id.as_deref())
        }
        Err(error) => Err(error),
    }
}

fn poll_antigravity() -> Result<UsageData, PollError> {
    let creds = match read_antigravity_credentials() {
        Some(creds) => creds,
        None => {
            diagnose::log("Antigravity usage poll failed: no Antigravity credentials found");
            return Err(PollError::NoCredentials);
        }
    };

    fetch_antigravity_usage(&creds.access_token)
}

pub fn kimi_credentials_path() -> PathBuf {
    let appdata = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(appdata)
        .join("ClaudeCodeUsageMonitor")
        .join("kimi.json")
}

fn read_kimi_credentials() -> Option<KimiCredentialFile> {
    let content = std::fs::read_to_string(kimi_credentials_path()).ok()?;
    let creds: KimiCredentialFile = serde_json::from_str(&content).ok()?;
    if creds.refresh_token.trim().is_empty() {
        return None;
    }
    Some(creds)
}

fn write_kimi_credentials(creds: &KimiCredentialFile) {
    let path = kimi_credentials_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match serde_json::to_string_pretty(creds) {
        Ok(json) => {
            if let Err(error) = std::fs::write(&path, json) {
                diagnose::log_error("unable to write Kimi credentials", error);
            }
        }
        Err(error) => diagnose::log_error("unable to serialize Kimi credentials", error),
    }
}

fn poll_kimi() -> Result<UsageData, PollError> {
    let mut creds = match read_kimi_credentials() {
        Some(creds) => creds,
        None => {
            diagnose::log("Kimi usage poll failed: no Kimi refresh token configured");
            return Err(PollError::NoCredentials);
        }
    };

    let access_token = kimi_access_token(&mut creds)?;

    match fetch_kimi_usage(&access_token) {
        Ok(data) => Ok(data),
        // A cached access token can be revoked before its stated expiry;
        // force one refresh before giving up.
        Err(PollError::AuthRequired) => {
            diagnose::log("Kimi access token rejected; forcing refresh");
            creds.access_token.clear();
            creds.access_expires_at = None;
            let refreshed = kimi_access_token(&mut creds)?;
            fetch_kimi_usage(&refreshed)
        }
        Err(error) => Err(error),
    }
}

/// Return a usable access token, refreshing and persisting one if the cached
/// token is missing or close to expiry.
fn kimi_access_token(creds: &mut KimiCredentialFile) -> Result<String, PollError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let cached_is_valid = !creds.access_token.is_empty()
        && creds
            .access_expires_at
            .is_some_and(|expires| expires > now + KIMI_ACCESS_TOKEN_SKEW_SECS);

    if cached_is_valid {
        return Ok(creds.access_token.clone());
    }

    let refreshed = refresh_kimi_token(&creds.refresh_token)?;

    creds.access_token = refreshed.access_token.clone();
    creds.access_expires_at = jwt_expiry(&refreshed.access_token);
    if let Some(new_refresh) = refreshed.refresh_token.filter(|t| !t.is_empty()) {
        creds.refresh_token = new_refresh;
    }
    write_kimi_credentials(creds);

    Ok(refreshed.access_token)
}

fn refresh_kimi_token(refresh_token: &str) -> Result<KimiRefreshResponse, PollError> {
    let agent = build_agent()?;
    let resp = match agent
        .get(KIMI_REFRESH_URL)
        .set("Authorization", &format!("Bearer {refresh_token}"))
        .set("User-Agent", KIMI_USER_AGENT)
        .set("x-msh-platform", "web")
        .call()
    {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, _)) if code == 401 || code == 403 => {
            diagnose::log(format!(
                "Kimi refresh endpoint returned auth error status {code}; re-authentication required"
            ));
            return Err(PollError::TokenExpired);
        }
        Err(error) => {
            diagnose::log_error("Kimi refresh endpoint request failed", error);
            return Err(PollError::RequestFailed);
        }
    };

    resp.into_json().map_err(|error| {
        diagnose::log_error("unable to parse Kimi refresh response", error);
        PollError::RequestFailed
    })
}

fn fetch_kimi_usage(access_token: &str) -> Result<UsageData, PollError> {
    let agent = build_agent()?;
    let resp = match agent
        .post(KIMI_STATS_URL)
        .set("Authorization", &format!("Bearer {access_token}"))
        .set("Content-Type", "application/json")
        .set("connect-protocol-version", "1")
        .set("Origin", "https://www.kimi.com")
        .set("Referer", "https://www.kimi.com/code/console")
        .set("User-Agent", KIMI_USER_AGENT)
        .set("x-msh-platform", "web")
        .send_string("{}")
    {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, _)) if code == 401 || code == 403 => {
            diagnose::log(format!(
                "Kimi usage endpoint returned auth error status {code}; refresh required"
            ));
            return Err(PollError::AuthRequired);
        }
        Err(error) => {
            diagnose::log_error("Kimi usage endpoint request failed", error);
            return Err(PollError::RequestFailed);
        }
    };

    let response: KimiStatsResponse = match resp.into_json() {
        Ok(response) => response,
        Err(error) => {
            diagnose::log_error("unable to parse Kimi usage response", error);
            return Err(PollError::RequestFailed);
        }
    };

    kimi_usage_from_response(response).ok_or(PollError::RequestFailed)
}

fn kimi_usage_from_response(response: KimiStatsResponse) -> Option<UsageData> {
    // Both windows disabled means the account has no coding subscription, which
    // is a different situation from a transport failure — but there is nothing
    // meaningful to draw either way.
    let session = response.ratelimit_5h.as_ref().map(kimi_section);
    let weekly = response.ratelimit_7d.as_ref().map(kimi_section);
    if session.is_none() && weekly.is_none() {
        return None;
    }

    Some(UsageData {
        session: session.unwrap_or_default(),
        weekly: weekly.unwrap_or_default(),
    })
}

fn kimi_section(limit: &KimiRateLimit) -> UsageSection {
    UsageSection {
        countdown_override: None,
        // `ratio` is a 0–1 fraction of the window consumed; the widget works in
        // percentages like every other provider.
        percentage: if limit.enabled {
            (limit.ratio * 100.0).clamp(0.0, 100.0)
        } else {
            0.0
        },
        resets_at: parse_iso8601(limit.reset_time.as_deref()),
    }
}

/// Read the `exp` claim from a JWT without verifying it. Only used to decide
/// when to refresh, so a malformed token simply forces a refresh next poll.
fn jwt_expiry(token: &str) -> Option<u64> {
    let payload = token.split('.').nth(1)?;
    let decoded = base64_url_decode(payload)?;
    let json: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    json.get("exp")?.as_u64()
}

fn base64_url_decode(input: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

    let mut buffer = 0u32;
    let mut bits = 0u32;
    let mut out = Vec::with_capacity(input.len() * 3 / 4);

    for byte in input.bytes() {
        if byte == b'=' {
            break;
        }
        let value = TABLE.iter().position(|c| *c == byte)? as u32;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }

    Some(out)
}

pub fn copilot_config_path() -> PathBuf {
    let appdata = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(appdata)
        .join("ClaudeCodeUsageMonitor")
        .join("copilot.json")
}

fn read_copilot_config() -> CopilotConfigFile {
    std::fs::read_to_string(copilot_config_path())
        .ok()
        .and_then(|content| serde_json::from_str(&content).ok())
        .unwrap_or_default()
}

/// The Copilot editor extensions store an OAuth token per GitHub host. It only
/// unlocks `copilot_internal/*`, which is all the credits row needs.
fn read_copilot_editor_token() -> Option<String> {
    let local = std::env::var("LOCALAPPDATA").ok()?;
    let path = PathBuf::from(local)
        .join("github-copilot")
        .join("apps.json");
    let content = std::fs::read_to_string(path).ok()?;
    let apps: HashMap<String, CopilotAppEntry> = serde_json::from_str(&content).ok()?;
    apps.into_values()
        .map(|entry| entry.oauth_token)
        .find(|token| !token.is_empty())
}

fn poll_copilot() -> Result<UsageData, PollError> {
    let config = read_copilot_config();
    let mut data = UsageData::default();
    let mut any = false;

    // Credits row. Works from the editor token alone, so it stays available
    // even when no personal access token has been configured.
    match fetch_copilot_credits(config.included_credits) {
        Ok(section) => {
            data.session = section;
            any = true;
        }
        Err(error) => diagnose::log(format!("Copilot credits unavailable: {error:?}")),
    }

    // Budget row. Needs a PAT with org billing read.
    if !config.token.is_empty() && !config.org.is_empty() {
        match fetch_copilot_budget(&config.token, &config.org) {
            Ok(section) => {
                data.weekly = section;
                any = true;
            }
            Err(error) => diagnose::log(format!("Copilot budget unavailable: {error:?}")),
        }
    } else {
        diagnose::log("Copilot budget row skipped: no token/org in copilot.json");
    }

    if any {
        Ok(data)
    } else {
        Err(PollError::NoCredentials)
    }
}

fn fetch_copilot_credits(included_credits: Option<f64>) -> Result<UsageSection, PollError> {
    let token = match read_copilot_editor_token() {
        Some(token) => token,
        None => {
            diagnose::log("Copilot poll failed: no Copilot editor credentials found");
            return Err(PollError::NoCredentials);
        }
    };

    let agent = build_agent()?;
    let resp = match agent
        .get(COPILOT_QUOTA_URL)
        .set("Authorization", &format!("token {token}"))
        .set("Editor-Version", "ClaudeCodeUsageMonitor/1.0")
        .call()
    {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, _)) if code == 401 || code == 403 => {
            diagnose::log(format!(
                "Copilot quota endpoint returned auth error status {code}"
            ));
            return Err(PollError::AuthRequired);
        }
        Err(error) => {
            diagnose::log_error("Copilot quota endpoint request failed", error);
            return Err(PollError::RequestFailed);
        }
    };

    let response: CopilotQuotaResponse = match resp.into_json() {
        Ok(response) => response,
        Err(error) => {
            diagnose::log_error("unable to parse Copilot quota response", error);
            return Err(PollError::RequestFailed);
        }
    };

    Ok(copilot_credits_section(response, included_credits))
}

fn copilot_credits_section(
    response: CopilotQuotaResponse,
    included_credits: Option<f64>,
) -> UsageSection {
    let credits_used = response
        .quota_snapshots
        .as_ref()
        .and_then(|snapshots| snapshots.get("premium_interactions"))
        .map(|snapshot| snapshot.credits_used)
        .unwrap_or(0.0);

    // GitHub exposes no included-credit allowance, so without a configured
    // denominator show the raw credit count and leave the bar empty.
    match included_credits.filter(|total| *total > 0.0) {
        Some(total) => UsageSection {
            percentage: (credits_used / total * 100.0).clamp(0.0, 100.0),
            resets_at: parse_date_only(response.quota_reset_date.as_deref()),
            countdown_override: None,
        },
        None => UsageSection {
            percentage: 0.0,
            resets_at: parse_date_only(response.quota_reset_date.as_deref()),
            countdown_override: Some(format!("{credits_used:.0}cr")),
        },
    }
}

fn fetch_copilot_budget(token: &str, org: &str) -> Result<UsageSection, PollError> {
    let agent = build_agent()?;

    let budgets: CopilotBudgetsResponse = copilot_api_get(
        &agent,
        token,
        &format!("https://api.github.com/organizations/{org}/settings/billing/budgets"),
    )?;
    let budget_amount = budgets
        .budgets
        .iter()
        .find(|budget| budget.budget_product_sku == "copilot")
        .map(|budget| budget.budget_amount)
        .unwrap_or(0.0);

    let (year, month) = current_year_month();
    let usage: CopilotUsageResponse = copilot_api_get(
        &agent,
        token,
        &format!(
            "https://api.github.com/organizations/{org}/settings/billing/usage?year={year}&month={month}"
        ),
    )?;
    let spend: f64 = usage
        .usage_items
        .iter()
        .filter(|item| item.product == "copilot")
        .map(|item| item.net_amount)
        .sum();

    Ok(UsageSection {
        percentage: if budget_amount > 0.0 {
            (spend / budget_amount * 100.0).clamp(0.0, 100.0)
        } else {
            0.0
        },
        resets_at: next_month_start(year, month),
        countdown_override: Some(format!("${spend:.2}")),
    })
}

fn copilot_api_get<T: serde::de::DeserializeOwned>(
    agent: &ureq::Agent,
    token: &str,
    url: &str,
) -> Result<T, PollError> {
    let resp = match agent
        .get(url)
        .set("Authorization", &format!("Bearer {token}"))
        .set("Accept", "application/vnd.github+json")
        .set("X-GitHub-Api-Version", COPILOT_API_VERSION)
        .call()
    {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, _)) if code == 401 || code == 403 => {
            diagnose::log(format!(
                "Copilot billing endpoint returned auth error status {code}; check the token's org permissions"
            ));
            return Err(PollError::AuthRequired);
        }
        Err(error) => {
            diagnose::log_error("Copilot billing endpoint request failed", error);
            return Err(PollError::RequestFailed);
        }
    };

    resp.into_json().map_err(|error| {
        diagnose::log_error("unable to parse Copilot billing response", error);
        PollError::RequestFailed
    })
}

/// Parse a bare `YYYY-MM-DD` date as midnight UTC.
fn parse_date_only(value: Option<&str>) -> Option<SystemTime> {
    let value = value?;
    parse_iso8601(Some(&format!("{value}T00:00:00")))
}

fn current_year_month() -> (u64, u64) {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (year, month, _) = civil_from_unix(secs);
    (year, month)
}

fn next_month_start(year: u64, month: u64) -> Option<SystemTime> {
    let (next_year, next_month) = if month >= 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    parse_iso8601(Some(&format!("{next_year:04}-{next_month:02}-01T00:00:00")))
}

/// Convert a Unix timestamp to a civil (year, month, day) in UTC.
fn civil_from_unix(secs: u64) -> (u64, u64, u64) {
    let mut days = secs / 86400;
    let mut year = 1970u64;
    loop {
        let year_days = if is_leap(year) { 366 } else { 365 };
        if days < year_days {
            break;
        }
        days -= year_days;
        year += 1;
    }

    let month_lengths = [
        31,
        if is_leap(year) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 1u64;
    for length in month_lengths {
        if days < length {
            break;
        }
        days -= length;
        month += 1;
    }

    (year, month, days + 1)
}

fn refresh_or_fallback(mut creds: Credentials) -> Result<Credentials, PollError> {
    loop {
        if !is_token_expired(creds.expires_at) {
            return Ok(creds);
        }

        let source = creds.source.clone();
        cli_refresh_token(&source);

        match read_credentials_from_source(&source) {
            Some(refreshed) if !is_token_expired(refreshed.expires_at) => return Ok(refreshed),
            Some(_) => diagnose::log(format!(
                "credentials from {source:?} still expired after refresh attempt"
            )),
            None => diagnose::log(format!(
                "credentials from {source:?} unavailable after refresh attempt"
            )),
        }

        match read_next_credentials_after(&source) {
            Some(next) => creds = next,
            None => return Err(PollError::TokenExpired),
        }
    }
}

/// Invoke the Claude CLI with a minimal prompt to force its internal
/// OAuth token refresh.
fn cli_refresh_token(source: &CredentialSource) {
    match source {
        CredentialSource::Windows(_) => cli_refresh_windows_token(),
        CredentialSource::Wsl { distro } => cli_refresh_wsl_token(distro),
    }
}

fn cli_refresh_windows_token() {
    let claude_path = resolve_windows_claude_path();
    let is_cmd = claude_path.to_lowercase().ends_with(".cmd");
    diagnose::log(format!(
        "attempting Windows Claude token refresh via {claude_path}"
    ));

    let args: &[&str] = &["-p", "."];

    let mut cmd = if is_cmd {
        let mut c = Command::new("cmd.exe");
        c.arg("/c").arg(&claude_path).args(args);
        c
    } else {
        let mut c = Command::new(&claude_path);
        c.args(args);
        c
    };
    cmd.env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(error) => {
            diagnose::log_error("unable to spawn Windows Claude token refresh", error);
            return;
        }
    };

    // Wait up to 30 seconds — don't block the poll thread forever
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > Duration::from_secs(30) {
                    let _ = child.kill();
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(_) => break,
        }
    }
}

fn cli_refresh_wsl_token(distro: &str) {
    diagnose::log(format!(
        "attempting WSL Claude token refresh in distro {distro}"
    ));
    let mut cmd = Command::new("wsl.exe");
    cmd.arg("-d")
        .arg(distro)
        .arg("--")
        .arg("bash")
        .arg("-lic")
        .arg("if command -v claude >/dev/null 2>&1; then claude -p .; elif [ -x \"$HOME/.local/bin/claude\" ]; then \"$HOME/.local/bin/claude\" -p .; else exit 127; fi")
        .env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(error) => {
            diagnose::log_error("unable to spawn WSL Claude token refresh", error);
            return;
        }
    };

    wait_for_refresh(&mut child);
}

fn cli_refresh_codex_token() {
    let codex_path = resolve_windows_codex_path();
    let is_cmd = codex_path.to_lowercase().ends_with(".cmd");
    let is_ps1 = codex_path.to_lowercase().ends_with(".ps1");
    diagnose::log(format!(
        "attempting Windows Codex token refresh via {codex_path}"
    ));

    let args: &[&str] = &["exec", "."];

    let mut cmd = if is_cmd {
        let mut c = Command::new("cmd.exe");
        c.arg("/c").arg(&codex_path).args(args);
        c
    } else if is_ps1 {
        let mut c = Command::new("powershell.exe");
        c.arg("-NoProfile")
            .arg("-ExecutionPolicy")
            .arg("Bypass")
            .arg("-File")
            .arg(&codex_path)
            .args(args);
        c
    } else {
        let mut c = Command::new(&codex_path);
        c.args(args);
        c
    };
    cmd.creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(error) => {
            diagnose::log_error("unable to spawn Windows Codex token refresh", error);
            return;
        }
    };

    wait_for_refresh(&mut child);
}

/// Spawn a command and wait up to `timeout` for it to finish.
/// Returns None if the process fails to start or exceeds the deadline.
fn run_with_timeout(cmd: &mut Command, timeout: Duration) -> Option<std::process::Output> {
    let mut child = cmd.spawn().ok()?;
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => return None,
        }
    }
}

fn wait_for_refresh(child: &mut std::process::Child) {
    // Wait up to 30 seconds; don't block the poll thread forever.
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > Duration::from_secs(30) {
                    let _ = child.kill();
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(_) => break,
        }
    }
}

/// Resolve the full path to the `claude` CLI executable.
fn resolve_windows_claude_path() -> String {
    for name in &["claude.cmd", "claude"] {
        if Command::new(name)
            .arg("--version")
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
        {
            return name.to_string();
        }
    }

    for name in &["claude.cmd", "claude"] {
        if let Ok(output) = Command::new("where.exe")
            .arg(name)
            .creation_flags(CREATE_NO_WINDOW)
            .output()
        {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if let Some(first_line) = stdout.lines().next() {
                    let path = first_line.trim().to_string();
                    if !path.is_empty() {
                        return path;
                    }
                }
            }
        }
    }

    "claude.cmd".to_string()
}

fn resolve_windows_codex_path() -> String {
    for name in &["codex.cmd", "codex.ps1", "codex.exe", "codex"] {
        if Command::new(name)
            .arg("--version")
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
        {
            return name.to_string();
        }
    }

    for name in &["codex.cmd", "codex.ps1", "codex.exe", "codex"] {
        if let Ok(output) = Command::new("where.exe")
            .arg(name)
            .creation_flags(CREATE_NO_WINDOW)
            .output()
        {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if let Some(first_line) = stdout.lines().next() {
                    let path = first_line.trim().to_string();
                    if !path.is_empty() {
                        return path;
                    }
                }
            }
        }
    }

    "codex.cmd".to_string()
}

fn build_agent() -> Result<ureq::Agent, PollError> {
    let tls = native_tls::TlsConnector::new().map_err(|_| PollError::RequestFailed)?;
    Ok(ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .tls_connector(std::sync::Arc::new(tls))
        .build())
}

pub fn credential_watch_snapshot(mode: CredentialWatchMode) -> CredentialWatchSnapshot {
    if mode == CredentialWatchMode::Antigravity {
        return vec![antigravity_credential_watch_signature()];
    }

    let sources = match mode {
        CredentialWatchMode::ActiveSource => read_first_credentials()
            .map(|creds| vec![creds.source])
            .unwrap_or_else(all_known_credential_sources),
        CredentialWatchMode::AllSources => all_known_credential_sources(),
        CredentialWatchMode::Antigravity => unreachable!(),
    };

    let mut snapshot: CredentialWatchSnapshot = sources
        .into_iter()
        .filter_map(|source| credential_watch_signature(&source))
        .collect();
    snapshot.sort();
    snapshot.dedup();
    snapshot
}

fn all_known_credential_sources() -> Vec<CredentialSource> {
    let mut sources = Vec::new();
    if let Some(source) = windows_credential_source() {
        sources.push(source);
    }
    for distro in list_wsl_distros() {
        sources.push(CredentialSource::Wsl { distro });
    }
    sources
}

fn windows_credential_source() -> Option<CredentialSource> {
    let home = dirs::home_dir()?;
    Some(CredentialSource::Windows(
        home.join(".claude").join(".credentials.json"),
    ))
}

fn credential_watch_signature(source: &CredentialSource) -> Option<String> {
    match source {
        CredentialSource::Windows(path) => Some(windows_credential_watch_signature(path)),
        CredentialSource::Wsl { distro } => wsl_credential_watch_signature(distro),
    }
}

fn windows_credential_watch_signature(path: &PathBuf) -> String {
    let key = format!("win:{}", path.display());
    match std::fs::metadata(path) {
        Ok(metadata) => {
            let modified = metadata
                .modified()
                .ok()
                .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
                .map(|value| value.as_secs())
                .unwrap_or(0);
            format!("{key}|present|{}|{modified}", metadata.len())
        }
        Err(_) => format!("{key}|missing"),
    }
}

fn wsl_credential_watch_signature(distro: &str) -> Option<String> {
    let output = run_with_timeout(
        Command::new("wsl.exe")
            .arg("-d")
            .arg(distro)
            .arg("--")
            .arg("sh")
            .arg("-lc")
            .arg(
                "if [ -f ~/.claude/.credentials.json ]; then \
                 stat -c 'present|%s|%Y' ~/.claude/.credentials.json; \
                 else echo missing; fi",
            )
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null()),
        Duration::from_secs(5),
    )?;

    let state = if output.status.success() {
        decode_wsl_text(&output.stdout).trim().to_string()
    } else {
        format!("status-{}", output.status)
    };

    Some(format!("wsl:{distro}|{state}"))
}

fn fetch_usage_with_fallback(token: &str) -> Result<UsageData, PollError> {
    // Try the dedicated usage endpoint first
    match try_usage_endpoint(token)? {
        Some(data) => {
            // If reset timers are missing, fill them in from the Messages API
            if data.session.resets_at.is_none() || data.weekly.resets_at.is_none() {
                if let Ok(fallback) = fetch_usage_via_messages(token) {
                    let mut merged = data;
                    if merged.session.resets_at.is_none() {
                        merged.session.resets_at = fallback.session.resets_at;
                    }
                    if merged.weekly.resets_at.is_none() {
                        merged.weekly.resets_at = fallback.weekly.resets_at;
                    }
                    return Ok(merged);
                }
            }
            return Ok(data);
        }
        None => {}
    }

    // Fall back to Messages API with rate limit headers
    let result = fetch_usage_via_messages(token);
    if result.is_err() {
        diagnose::log("usage endpoint and Messages API fallback both failed");
    }
    result
}

fn try_usage_endpoint(token: &str) -> Result<Option<UsageData>, PollError> {
    let agent = build_agent()?;

    let resp = match agent
        .get(USAGE_URL)
        .set("Authorization", &format!("Bearer {token}"))
        .set("anthropic-beta", "oauth-2025-04-20")
        .call()
    {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, _)) if code == 401 || code == 403 => {
            diagnose::log(format!(
                "usage endpoint returned auth error status {code}; re-login required"
            ));
            return Err(PollError::AuthRequired);
        }
        Err(_) => return Ok(None),
    };

    let response: UsageResponse = match resp.into_json() {
        Ok(response) => response,
        Err(_) => return Ok(None),
    };
    let mut data = UsageData::default();

    if let Some(bucket) = &response.five_hour {
        data.session.percentage = bucket.utilization;
        data.session.resets_at = parse_iso8601(bucket.resets_at.as_deref());
    }

    if let Some(bucket) = &response.seven_day {
        data.weekly.percentage = bucket.utilization;
        data.weekly.resets_at = parse_iso8601(bucket.resets_at.as_deref());
    }

    Ok(Some(data))
}

fn fetch_usage_via_messages(token: &str) -> Result<UsageData, PollError> {
    let agent = build_agent()?;

    for model in MODEL_FALLBACK_CHAIN {
        let body = serde_json::json!({
            "model": model,
            "max_tokens": 1,
            "messages": [{"role": "user", "content": "."}]
        });

        let response = match agent
            .post(MESSAGES_URL)
            .set("Authorization", &format!("Bearer {token}"))
            .set("anthropic-version", "2023-06-01")
            .set("anthropic-beta", "oauth-2025-04-20")
            .send_json(&body)
        {
            Ok(resp) => resp,
            Err(ureq::Error::Status(code, _)) if code == 401 || code == 403 => {
                diagnose::log(format!(
                    "messages endpoint returned auth error status {code}; re-login required"
                ));
                return Err(PollError::AuthRequired);
            }
            Err(ureq::Error::Status(_code, resp)) => resp,
            Err(_) => continue,
        };

        let h5 = response.header("anthropic-ratelimit-unified-5h-utilization");
        let h7 = response.header("anthropic-ratelimit-unified-7d-utilization");
        let hs = response.header("anthropic-ratelimit-unified-status");

        if h5.is_some() || h7.is_some() || hs.is_some() {
            return Ok(parse_rate_limit_headers(&response));
        }
    }

    Err(PollError::RequestFailed)
}

fn parse_rate_limit_headers(response: &ureq::Response) -> UsageData {
    let mut data = UsageData::default();

    data.session.percentage =
        get_header_f64(response, "anthropic-ratelimit-unified-5h-utilization") * 100.0;
    data.session.resets_at = unix_to_system_time(get_header_i64(
        response,
        "anthropic-ratelimit-unified-5h-reset",
    ));

    data.weekly.percentage =
        get_header_f64(response, "anthropic-ratelimit-unified-7d-utilization") * 100.0;
    data.weekly.resets_at = unix_to_system_time(get_header_i64(
        response,
        "anthropic-ratelimit-unified-7d-reset",
    ));

    let overall_reset = get_header_i64(response, "anthropic-ratelimit-unified-reset");

    if data.session.percentage == 0.0 && data.weekly.percentage == 0.0 {
        let status = response.header("anthropic-ratelimit-unified-status");
        if status == Some("rejected") {
            let claim = response.header("anthropic-ratelimit-unified-representative-claim");
            match claim {
                Some("five_hour") => data.session.percentage = 100.0,
                Some("seven_day") => data.weekly.percentage = 100.0,
                _ => {}
            }
        }

        if data.session.resets_at.is_none() && overall_reset.is_some() {
            data.session.resets_at = unix_to_system_time(overall_reset);
        }
    }

    data
}

fn fetch_codex_usage(token: &str, account_id: Option<&str>) -> Result<UsageData, PollError> {
    let agent = build_agent()?;
    let mut request = agent
        .get(CODEX_USAGE_URL)
        .set("Authorization", &format!("Bearer {token}"))
        .set("User-Agent", "codex-cli");

    if let Some(account_id) = account_id.filter(|value| !value.is_empty()) {
        request = request.set("ChatGPT-Account-Id", account_id);
    }

    let resp = match request.call() {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, _)) if code == 401 || code == 403 => {
            diagnose::log(format!(
                "Codex usage endpoint returned auth error status {code}; refresh required"
            ));
            return Err(PollError::AuthRequired);
        }
        Err(error) => {
            diagnose::log_error("Codex usage endpoint request failed", error);
            return Err(PollError::RequestFailed);
        }
    };

    let response: CodexUsageResponse = match resp.into_json() {
        Ok(response) => response,
        Err(error) => {
            diagnose::log_error("unable to parse Codex usage response", error);
            return Err(PollError::RequestFailed);
        }
    };

    codex_usage_from_response(response).ok_or(PollError::RequestFailed)
}

fn codex_usage_from_response(response: CodexUsageResponse) -> Option<UsageData> {
    let details = *response.rate_limit.flatten()?;
    let mut data = UsageData::default();

    if let Some(window) = details.primary_window.flatten() {
        data.session = codex_section_from_window(&window);
    }

    if let Some(window) = details.secondary_window.flatten() {
        data.weekly = codex_section_from_window(&window);
    }

    Some(data)
}

fn codex_section_from_window(window: &CodexRateLimitWindow) -> UsageSection {
    UsageSection {
        countdown_override: None,
        percentage: window.used_percent,
        resets_at: unix_to_system_time(Some(window.reset_at)),
    }
}

fn antigravity_credential_watch_signature() -> String {
    let Some(content) = read_windows_generic_credential(ANTIGRAVITY_CREDENTIAL_TARGET) else {
        return format!("{ANTIGRAVITY_CREDENTIAL_TARGET}|missing");
    };

    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    format!(
        "{ANTIGRAVITY_CREDENTIAL_TARGET}|present|{}|{}",
        content.len(),
        hasher.finish()
    )
}

fn fetch_antigravity_usage(token: &str) -> Result<UsageData, PollError> {
    let mut auth_error = false;
    let mut last_error = PollError::RequestFailed;

    for base_url in ANTIGRAVITY_ENDPOINTS {
        match fetch_antigravity_usage_from_endpoint(base_url, token) {
            Ok(data) => return Ok(data),
            Err(PollError::AuthRequired) => auth_error = true,
            Err(error) => last_error = error,
        }
    }

    if auth_error {
        Err(PollError::AuthRequired)
    } else {
        Err(last_error)
    }
}

fn fetch_antigravity_usage_from_endpoint(
    base_url: &str,
    token: &str,
) -> Result<UsageData, PollError> {
    let project = fetch_antigravity_project(base_url, token)?;
    if let Some(project) = project.as_deref() {
        match fetch_antigravity_quota_summary(base_url, token, project) {
            Ok(data) => return Ok(data),
            Err(PollError::AuthRequired) => return Err(PollError::AuthRequired),
            Err(error) => diagnose::log(format!(
                "Antigravity retrieveUserQuotaSummary failed, falling back to model quota: {error:?}"
            )),
        }
    }

    let session = fetch_antigravity_model_quota(base_url, token, project.as_deref())?;
    let weekly = UsageSection::default();

    Ok(UsageData { session, weekly })
}

fn fetch_antigravity_project(base_url: &str, token: &str) -> Result<Option<String>, PollError> {
    let agent = build_agent()?;
    let body = serde_json::json!({
        "metadata": {
            "ideType": "ANTIGRAVITY"
        }
    });

    let resp = match agent
        .post(&format!("{base_url}/v1internal:loadCodeAssist"))
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json")
        .set("User-Agent", "antigravity")
        .send_json(&body)
    {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, _)) if code == 401 || code == 403 => {
            diagnose::log(format!(
                "Antigravity loadCodeAssist returned auth error status {code}"
            ));
            return Err(PollError::AuthRequired);
        }
        Err(error) => {
            diagnose::log_error("Antigravity loadCodeAssist request failed", error);
            return Err(PollError::RequestFailed);
        }
    };

    let response: AntigravityLoadResponse = match resp.into_json() {
        Ok(response) => response,
        Err(error) => {
            diagnose::log_error("unable to parse Antigravity loadCodeAssist response", error);
            return Err(PollError::RequestFailed);
        }
    };

    Ok(response.project.filter(|project| !project.is_empty()))
}

fn fetch_antigravity_model_quota(
    base_url: &str,
    token: &str,
    project: Option<&str>,
) -> Result<UsageSection, PollError> {
    let agent = build_agent()?;
    let body = match project {
        Some(project) => serde_json::json!({ "project": project }),
        None => serde_json::json!({}),
    };

    let resp = match agent
        .post(&format!("{base_url}/v1internal:fetchAvailableModels"))
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json")
        .set("User-Agent", "antigravity")
        .send_json(&body)
    {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, _)) if code == 401 || code == 403 => {
            diagnose::log(format!(
                "Antigravity fetchAvailableModels returned auth error status {code}"
            ));
            return Err(PollError::AuthRequired);
        }
        Err(error) => {
            diagnose::log_error("Antigravity fetchAvailableModels request failed", error);
            return Err(PollError::RequestFailed);
        }
    };

    let response: AntigravityModelsResponse = match resp.into_json() {
        Ok(response) => response,
        Err(error) => {
            diagnose::log_error(
                "unable to parse Antigravity fetchAvailableModels response",
                error,
            );
            return Err(PollError::RequestFailed);
        }
    };

    best_antigravity_section(response.models.into_iter().filter_map(|(model, info)| {
        let quota = info.quota_info?;
        if !is_antigravity_display_model(&model) {
            return None;
        }
        antigravity_section_from_quota(quota)
    }))
    .ok_or(PollError::RequestFailed)
}

fn fetch_antigravity_quota_summary(
    base_url: &str,
    token: &str,
    project: &str,
) -> Result<UsageData, PollError> {
    let agent = build_agent()?;
    let body = serde_json::json!({ "project": project });

    let resp = match agent
        .post(&format!("{base_url}/v1internal:retrieveUserQuotaSummary"))
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json")
        .set("User-Agent", "antigravity")
        .send_json(&body)
    {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, _)) if code == 401 || code == 403 => {
            return Err(PollError::AuthRequired);
        }
        Err(error) => {
            diagnose::log_error("Antigravity retrieveUserQuotaSummary request failed", error);
            return Err(PollError::RequestFailed);
        }
    };

    let response: AntigravityQuotaSummaryResponse = match resp.into_json() {
        Ok(response) => response,
        Err(error) => {
            diagnose::log_error(
                "unable to parse Antigravity retrieveUserQuotaSummary response",
                error,
            );
            return Err(PollError::RequestFailed);
        }
    };

    antigravity_usage_from_summary(response).ok_or(PollError::RequestFailed)
}

fn antigravity_section_from_quota(quota: AntigravityQuotaInfo) -> Option<UsageSection> {
    let remaining = quota.remaining_fraction?.clamp(0.0, 1.0);
    Some(UsageSection {
        countdown_override: None,
        percentage: (1.0 - remaining) * 100.0,
        resets_at: parse_iso8601(quota.reset_time.as_deref()),
    })
}

fn antigravity_section_from_summary_bucket(
    bucket: &AntigravityQuotaSummaryBucket,
) -> Option<UsageSection> {
    let remaining = bucket.remaining_fraction?.clamp(0.0, 1.0);
    Some(UsageSection {
        countdown_override: None,
        percentage: (1.0 - remaining) * 100.0,
        resets_at: parse_iso8601(bucket.reset_time.as_deref()),
    })
}

fn antigravity_usage_from_summary(response: AntigravityQuotaSummaryResponse) -> Option<UsageData> {
    let mut fallback = None;

    for group in response.groups.unwrap_or_default() {
        let is_gemini = is_antigravity_gemini_summary_group(&group);
        let usage = antigravity_usage_from_summary_group(group);

        if is_gemini && usage.is_some() {
            return usage;
        }

        if fallback.is_none() {
            fallback = usage;
        }
    }

    fallback
}

fn antigravity_usage_from_summary_group(group: AntigravityQuotaSummaryGroup) -> Option<UsageData> {
    let mut data = UsageData::default();
    let mut has_quota = false;

    for bucket in group.buckets.unwrap_or_default() {
        let Some(section) = antigravity_section_from_summary_bucket(&bucket) else {
            continue;
        };

        match bucket.window.as_deref() {
            Some(window) if window.eq_ignore_ascii_case("5h") => {
                data.session = section;
                has_quota = true;
            }
            Some(window) if window.eq_ignore_ascii_case("weekly") => {
                data.weekly = section;
                has_quota = true;
            }
            _ => {}
        }
    }

    has_quota.then_some(data)
}

fn is_antigravity_gemini_summary_group(group: &AntigravityQuotaSummaryGroup) -> bool {
    group
        .display_name
        .as_deref()
        .is_some_and(|name| name.to_ascii_lowercase().contains("gemini"))
        || group
            .description
            .as_deref()
            .is_some_and(|description| description.to_ascii_lowercase().contains("gemini"))
        || group.buckets.as_ref().is_some_and(|buckets| {
            buckets.iter().any(|bucket| {
                bucket
                    .bucket_id
                    .as_deref()
                    .is_some_and(|id| id.to_ascii_lowercase().starts_with("gemini-"))
                    || bucket
                        .display_name
                        .as_deref()
                        .is_some_and(|name| name.to_ascii_lowercase().contains("gemini"))
            })
        })
}

fn best_antigravity_section<I>(sections: I) -> Option<UsageSection>
where
    I: IntoIterator<Item = UsageSection>,
{
    sections.into_iter().max_by(|a, b| {
        a.percentage
            .partial_cmp(&b.percentage)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.resets_at.cmp(&b.resets_at))
    })
}

fn is_antigravity_display_model(model: &str) -> bool {
    model.starts_with("gemini")
        || model.starts_with("claude")
        || model.starts_with("gpt")
        || model.starts_with("image")
        || model.starts_with("imagen")
}

fn get_header_f64(response: &ureq::Response, name: &str) -> f64 {
    response
        .header(name)
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0)
}

fn get_header_i64(response: &ureq::Response, name: &str) -> Option<i64> {
    response.header(name).and_then(|s| s.parse::<i64>().ok())
}

fn unix_to_system_time(unix_secs: Option<i64>) -> Option<SystemTime> {
    let secs = unix_secs?;
    if secs < 0 {
        return None;
    }
    Some(UNIX_EPOCH + Duration::from_secs(secs as u64))
}

struct Credentials {
    access_token: String,
    expires_at: Option<i64>,
    source: CredentialSource,
}

#[derive(Clone, Debug)]
enum CredentialSource {
    Windows(PathBuf),
    Wsl { distro: String },
}

fn read_first_credentials() -> Option<Credentials> {
    if let Some(creds) = read_windows_credentials() {
        return Some(creds);
    }

    for distro in list_wsl_distros() {
        if let Some(creds) = read_wsl_credentials(&distro) {
            return Some(creds);
        }
    }

    None
}

fn read_windows_credentials() -> Option<Credentials> {
    let CredentialSource::Windows(cred_path) = windows_credential_source()? else {
        return None;
    };
    let content = match std::fs::read_to_string(&cred_path) {
        Ok(content) => content,
        Err(error) => {
            if diagnose::is_enabled() {
                diagnose::log_error(
                    &format!(
                        "unable to read Windows credentials at {}",
                        cred_path.display()
                    ),
                    error,
                );
            }
            return None;
        }
    };
    parse_credentials(&content, CredentialSource::Windows(cred_path))
}

fn read_credentials_from_source(source: &CredentialSource) -> Option<Credentials> {
    match source {
        CredentialSource::Windows(path) => {
            let content = std::fs::read_to_string(path).ok()?;
            parse_credentials(&content, source.clone())
        }
        CredentialSource::Wsl { distro } => read_wsl_credentials(distro),
    }
}

fn codex_auth_path() -> Option<PathBuf> {
    if let Some(codex_home) = std::env::var_os("CODEX_HOME").map(PathBuf::from) {
        return Some(codex_home.join("auth.json"));
    }

    Some(dirs::home_dir()?.join(".codex").join("auth.json"))
}

fn read_codex_credentials() -> Option<CodexTokenData> {
    let auth_path = codex_auth_path()?;
    let content = match std::fs::read_to_string(&auth_path) {
        Ok(content) => content,
        Err(error) => {
            diagnose::log_error(
                &format!(
                    "unable to read Codex credentials at {}",
                    auth_path.display()
                ),
                error,
            );
            return None;
        }
    };

    let auth: CodexAuthFile = serde_json::from_str(&content).ok()?;
    auth.tokens.filter(|tokens| !tokens.access_token.is_empty())
}

fn read_antigravity_credentials() -> Option<AntigravityTokenData> {
    let content = read_windows_generic_credential(ANTIGRAVITY_CREDENTIAL_TARGET)?;
    let auth: AntigravityAuthFile = serde_json::from_str(&content).ok()?;
    if auth.token.access_token.is_empty() {
        None
    } else {
        Some(auth.token)
    }
}

fn read_windows_generic_credential(target: &str) -> Option<String> {
    const CRED_TYPE_GENERIC: u32 = 1;

    let mut target_wide: Vec<u16> = target.encode_utf16().chain(std::iter::once(0)).collect();
    let mut credential: *mut CredentialW = std::ptr::null_mut();

    let ok = unsafe {
        CredReadW(
            target_wide.as_mut_ptr(),
            CRED_TYPE_GENERIC,
            0,
            &mut credential,
        )
    };

    if ok == 0 || credential.is_null() {
        diagnose::log(format!(
            "unable to read Windows generic credential target {target}"
        ));
        return None;
    }

    let result = unsafe {
        let cred = &*credential;
        if cred.credential_blob_size == 0 || cred.credential_blob.is_null() {
            CredFree(credential as *mut c_void);
            return None;
        }
        let bytes =
            std::slice::from_raw_parts(cred.credential_blob, cred.credential_blob_size as usize);
        let text = String::from_utf8(bytes.to_vec()).ok();
        CredFree(credential as *mut c_void);
        text
    };

    result
}

fn read_wsl_credentials(distro: &str) -> Option<Credentials> {
    let output = run_with_timeout(
        Command::new("wsl.exe")
            .arg("-d")
            .arg(distro)
            .arg("--")
            .arg("sh")
            .arg("-lc")
            .arg("cat ~/.claude/.credentials.json")
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null()),
        Duration::from_secs(5),
    )?;

    if !output.status.success() {
        diagnose::log(format!(
            "WSL credentials probe failed for distro {distro} with status {}",
            output.status
        ));
        return None;
    }

    let content = String::from_utf8(output.stdout).ok()?;
    parse_credentials(
        &content,
        CredentialSource::Wsl {
            distro: distro.to_string(),
        },
    )
}

fn parse_credentials(content: &str, source: CredentialSource) -> Option<Credentials> {
    let json: serde_json::Value = serde_json::from_str(content).ok()?;

    let oauth = json.get("claudeAiOauth")?;
    let access_token = oauth
        .get("accessToken")
        .and_then(|v| v.as_str())?
        .to_string();
    let expires_at = oauth.get("expiresAt").and_then(|v| v.as_i64());

    Some(Credentials {
        access_token,
        expires_at,
        source,
    })
}

fn read_next_credentials_after(source: &CredentialSource) -> Option<Credentials> {
    match source {
        CredentialSource::Windows(_) => {
            for distro in list_wsl_distros() {
                if let Some(creds) = read_wsl_credentials(&distro) {
                    return Some(creds);
                }
            }
        }
        CredentialSource::Wsl { distro } => {
            let mut past_current = false;
            for candidate_distro in list_wsl_distros() {
                if !past_current {
                    past_current = candidate_distro == *distro;
                    continue;
                }
                if let Some(creds) = read_wsl_credentials(&candidate_distro) {
                    return Some(creds);
                }
            }
        }
    }

    None
}

fn list_wsl_distros() -> Vec<String> {
    let output = match run_with_timeout(
        Command::new("wsl.exe")
            .args(["-l", "-q"])
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null()),
        Duration::from_secs(5),
    ) {
        Some(output) if output.status.success() => output,
        _ => {
            diagnose::log("unable to enumerate WSL distros");
            return Vec::new();
        }
    };

    let stdout = decode_wsl_text(&output.stdout);
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn decode_wsl_text(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }

    if let Some(decoded) = decode_utf16le(bytes) {
        return decoded;
    }

    String::from_utf8_lossy(bytes).into_owned()
}

fn decode_utf16le(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 2 || bytes.len() % 2 != 0 {
        return None;
    }

    let body = if bytes.starts_with(&[0xFF, 0xFE]) {
        &bytes[2..]
    } else if looks_like_utf16le(bytes) {
        bytes
    } else {
        return None;
    };

    let units: Vec<u16> = body
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect();

    Some(String::from_utf16_lossy(&units))
}

fn looks_like_utf16le(bytes: &[u8]) -> bool {
    let sample_len = bytes.len().min(128);
    let units = sample_len / 2;
    if units == 0 {
        return false;
    }

    let nul_high_bytes = bytes[..sample_len]
        .chunks_exact(2)
        .filter(|chunk| chunk[1] == 0)
        .count();

    nul_high_bytes * 2 >= units
}

fn is_token_expired(expires_at: Option<i64>) -> bool {
    let Some(exp) = expires_at else { return false };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    now >= exp
}

/// Parse an ISO 8601 timestamp string into a SystemTime.
fn parse_iso8601(s: Option<&str>) -> Option<SystemTime> {
    let s = s?;
    // Strip timezone offset to get "YYYY-MM-DDTHH:MM:SS" or with fractional seconds
    // The API returns formats like "2026-03-05T08:00:00.321598+00:00"
    let datetime_part = s.split('+').next().unwrap_or(s);
    let datetime_part = datetime_part.split('Z').next().unwrap_or(datetime_part);

    // Try parsing with and without fractional seconds
    let formats = ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S"];
    for fmt in &formats {
        if let Ok(secs) = parse_datetime_to_unix(datetime_part, fmt) {
            return Some(UNIX_EPOCH + Duration::from_secs(secs));
        }
    }
    None
}

/// Minimal datetime parser — avoids pulling in chrono/time crates.
fn parse_datetime_to_unix(s: &str, _fmt: &str) -> Result<u64, ()> {
    // Extract date and time parts from "YYYY-MM-DDTHH:MM:SS[.frac]"
    let (date_str, time_str) = s.split_once('T').ok_or(())?;
    let date_parts: Vec<&str> = date_str.split('-').collect();
    if date_parts.len() != 3 {
        return Err(());
    }

    let year: u64 = date_parts[0].parse().map_err(|_| ())?;
    let month: u64 = date_parts[1].parse().map_err(|_| ())?;
    let day: u64 = date_parts[2].parse().map_err(|_| ())?;

    // Strip fractional seconds
    let time_base = time_str.split('.').next().unwrap_or(time_str);
    let time_parts: Vec<&str> = time_base.split(':').collect();
    if time_parts.len() != 3 {
        return Err(());
    }

    let hour: u64 = time_parts[0].parse().map_err(|_| ())?;
    let min: u64 = time_parts[1].parse().map_err(|_| ())?;
    let sec: u64 = time_parts[2].parse().map_err(|_| ())?;

    // Days from year (using a simplified calculation for dates after 1970)
    let mut days: u64 = 0;
    for y in 1970..year {
        days += if is_leap(y) { 366 } else { 365 };
    }

    let month_days = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    for m in 1..month {
        days += month_days[m as usize];
        if m == 2 && is_leap(year) {
            days += 1;
        }
    }
    days += day - 1;

    Ok(days * 86400 + hour * 3600 + min * 60 + sec)
}

fn is_leap(y: u64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// Format a usage section as "X% · Yh" style text
pub fn format_line(section: &UsageSection, strings: Strings) -> String {
    let pct = format!("{:.0}%", section.percentage);
    let cd = match section.countdown_override.as_deref() {
        Some(text) => text.to_string(),
        None => format_countdown(section.resets_at, strings),
    };
    if cd.is_empty() {
        pct
    } else {
        format!("{pct} \u{00b7} {cd}")
    }
}

fn format_countdown(resets_at: Option<SystemTime>, strings: Strings) -> String {
    let reset = match resets_at {
        Some(t) => t,
        None => return String::new(),
    };

    let remaining = match reset.duration_since(SystemTime::now()) {
        Ok(d) => d,
        Err(_) => return strings.now.to_string(),
    };

    format_countdown_from_secs(remaining.as_secs(), strings)
}

/// Calculate how long until the display text would change
pub fn time_until_display_change(resets_at: Option<SystemTime>) -> Option<Duration> {
    let reset = resets_at?;
    let remaining = reset.duration_since(SystemTime::now()).ok()?;
    Some(time_until_display_change_from_secs(remaining.as_secs()))
}

fn format_countdown_from_secs(total_secs: u64, strings: Strings) -> String {
    let total_mins = total_secs / 60;
    let total_hours = total_secs / 3600;
    let total_days = total_secs / 86400;

    if total_days >= 1 {
        format!("{total_days}{}", strings.day_suffix)
    } else if total_hours >= 1 {
        format!("{total_hours}{}", strings.hour_suffix)
    } else if total_mins >= 1 {
        format!("{total_mins}{}", strings.minute_suffix)
    } else {
        format!("{total_secs}{}", strings.second_suffix)
    }
}

fn time_until_display_change_from_secs(total_secs: u64) -> Duration {
    let total_mins = total_secs / 60;
    let total_hours = total_secs / 3600;
    let total_days = total_secs / 86400;

    let current_bucket_start = if total_days >= 1 {
        total_days * 86400
    } else if total_hours >= 1 {
        total_hours * 3600
    } else if total_mins >= 1 {
        total_mins * 60
    } else {
        total_secs
    };

    Duration::from_secs(total_secs.saturating_sub(current_bucket_start) + 1)
}

/// Returns true if either section has reached "now" (reset time has passed).
pub fn is_past_reset(data: &UsageData) -> bool {
    let now = SystemTime::now();
    let past = |s: &UsageSection| matches!(s.resets_at, Some(t) if now.duration_since(t).is_ok());
    past(&data.session) || past(&data.weekly)
}

pub fn app_is_past_reset(data: &AppUsageData) -> bool {
    Provider::ALL
        .iter()
        .filter_map(|provider| data.get(*provider))
        .any(is_past_reset)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage_with_session_percent(percentage: f64) -> UsageData {
        UsageData {
            session: UsageSection {
                countdown_override: None,
                percentage,
                resets_at: None,
            },
            weekly: UsageSection::default(),
        }
    }

    /// Build the enabled-provider mask from the providers that should be on.
    fn enabled(providers: &[Provider]) -> [bool; PROVIDER_COUNT] {
        let mut mask = [false; PROVIDER_COUNT];
        for provider in providers {
            mask[provider.index()] = true;
        }
        mask
    }

    #[test]
    fn claude_failure_does_not_block_codex_when_both_are_enabled() {
        let data = poll_with(
            enabled(&[Provider::ClaudeCode, Provider::Codex]),
            |provider| match provider {
                Provider::ClaudeCode => Err(PollError::AuthRequired),
                Provider::Codex => Ok(usage_with_session_percent(42.0)),
                other => unreachable!("{other:?} is disabled"),
            },
        )
        .expect("codex data should keep the poll successful");

        assert!(data.get(Provider::ClaudeCode).is_none());
        assert_eq!(
            data.get(Provider::Codex).unwrap().session.percentage,
            42.0
        );
    }

    #[test]
    fn codex_failure_does_not_block_claude_when_both_are_enabled() {
        let data = poll_with(
            enabled(&[Provider::ClaudeCode, Provider::Codex]),
            |provider| match provider {
                Provider::ClaudeCode => Ok(usage_with_session_percent(64.0)),
                Provider::Codex => Err(PollError::RequestFailed),
                other => unreachable!("{other:?} is disabled"),
            },
        )
        .expect("claude data should keep the poll successful");

        assert_eq!(
            data.get(Provider::ClaudeCode).unwrap().session.percentage,
            64.0
        );
        assert!(data.get(Provider::Codex).is_none());
    }

    #[test]
    fn returns_first_error_when_no_enabled_provider_succeeds() {
        let error = poll_with(
            enabled(&[Provider::ClaudeCode, Provider::Codex, Provider::Antigravity]),
            |provider| match provider {
                Provider::ClaudeCode => Err(PollError::AuthRequired),
                Provider::Codex => Err(PollError::RequestFailed),
                _ => Err(PollError::NoCredentials),
            },
        )
        .expect_err("all-provider failure should return an error");

        assert_eq!(error, PollError::AuthRequired);
    }

    #[test]
    fn antigravity_failure_does_not_block_codex_when_both_are_enabled() {
        let data = poll_with(
            enabled(&[Provider::Codex, Provider::Antigravity]),
            |provider| match provider {
                Provider::Codex => Ok(usage_with_session_percent(42.0)),
                Provider::Antigravity => Err(PollError::NoCredentials),
                other => unreachable!("{other:?} is disabled"),
            },
        )
        .expect("codex data should keep the poll successful");

        assert!(data.get(Provider::Antigravity).is_none());
        assert_eq!(
            data.get(Provider::Codex).unwrap().session.percentage,
            42.0
        );
    }

    #[test]
    fn kimi_failure_does_not_block_claude_when_both_are_enabled() {
        let data = poll_with(
            enabled(&[Provider::ClaudeCode, Provider::Kimi]),
            |provider| match provider {
                Provider::ClaudeCode => Ok(usage_with_session_percent(10.0)),
                Provider::Kimi => Err(PollError::NoCredentials),
                other => unreachable!("{other:?} is disabled"),
            },
        )
        .expect("claude data should keep the poll successful");

        assert!(data.get(Provider::Kimi).is_none());
        assert_eq!(
            data.get(Provider::ClaudeCode).unwrap().session.percentage,
            10.0
        );
    }

    #[test]
    fn kimi_ratios_become_percentages() {
        let response: KimiStatsResponse = serde_json::from_str(
            r#"{
                "ratelimitCode5h": {
                    "ratio": 0.5971,
                    "enabled": true,
                    "resetTime": "2026-08-12T19:32:28.560469934Z"
                },
                "ratelimitCode7d": {
                    "ratio": 0.2246,
                    "enabled": true,
                    "resetTime": "2026-08-19T09:32:28.560469934Z"
                },
                "subscriptionBalance": { "amountUsedRatio": 0.0449 }
            }"#,
        )
        .expect("stats response should deserialize");

        let usage = kimi_usage_from_response(response).expect("both windows should be present");

        assert!((usage.session.percentage - 59.71).abs() < 0.0001);
        assert!((usage.weekly.percentage - 22.46).abs() < 0.0001);
        assert!(usage.session.resets_at.is_some());
        assert!(usage.weekly.resets_at.is_some());
    }

    #[test]
    fn kimi_disabled_window_reports_no_usage() {
        let response: KimiStatsResponse = serde_json::from_str(
            r#"{
                "ratelimitCode5h": { "ratio": 0.9, "enabled": false, "resetTime": null },
                "ratelimitCode7d": { "ratio": 0.5, "enabled": true, "resetTime": null }
            }"#,
        )
        .expect("stats response should deserialize");

        let usage = kimi_usage_from_response(response).expect("windows are present");

        assert_eq!(usage.session.percentage, 0.0);
        assert!((usage.weekly.percentage - 50.0).abs() < 0.0001);
    }

    /// Live end-to-end check against the real Kimi endpoints. Requires a
    /// configured kimi.json, so it is ignored by default:
    ///   cargo test kimi_live -- --ignored --nocapture
    #[test]
    #[ignore]
    fn kimi_live_poll() {
        let usage = poll_kimi().expect("live Kimi poll should succeed");
        println!(
            "5h: {:.2}% resets {:?}",
            usage.session.percentage, usage.session.resets_at
        );
        println!(
            "7d: {:.2}% resets {:?}",
            usage.weekly.percentage, usage.weekly.resets_at
        );
        assert!(usage.session.percentage >= 0.0 && usage.session.percentage <= 100.0);
        assert!(usage.session.resets_at.is_some() || usage.weekly.resets_at.is_some());
    }

    /// Live end-to-end check against GitHub. Requires a configured
    /// copilot.json, so it is ignored by default:
    ///   cargo test copilot_live -- --ignored --nocapture
    #[test]
    #[ignore]
    fn copilot_live_poll() {
        let usage = poll_copilot().expect("live Copilot poll should succeed");
        let strings = crate::localization::LanguageId::English.strings();
        println!(
            "credits row: {:.2}%  text={:?}",
            usage.session.percentage,
            format_line(&usage.session, strings)
        );
        println!(
            "budget  row: {:.2}%  text={:?}",
            usage.weekly.percentage,
            format_line(&usage.weekly, strings)
        );
        assert!(usage.session.resets_at.is_some() || usage.weekly.resets_at.is_some());
    }

    #[test]
    fn copilot_credits_use_configured_denominator() {
        let json = r#"{
            "quota_reset_date": "2026-09-01",
            "quota_snapshots": {
                "premium_interactions": { "credits_used": 719, "entitlement": 0 }
            }
        }"#;

        let with_total: CopilotQuotaResponse = serde_json::from_str(json).unwrap();
        let section = copilot_credits_section(with_total, Some(1000.0));
        assert!((section.percentage - 71.9).abs() < 0.0001);
        assert!(section.countdown_override.is_none());
        assert!(section.resets_at.is_some());

        // Without a denominator the bar stays empty and the raw count shows.
        let without_total: CopilotQuotaResponse = serde_json::from_str(json).unwrap();
        let section = copilot_credits_section(without_total, None);
        assert_eq!(section.percentage, 0.0);
        assert_eq!(section.countdown_override.as_deref(), Some("719cr"));
    }

    #[test]
    fn civil_from_unix_matches_known_dates() {
        assert_eq!(civil_from_unix(0), (1970, 1, 1));
        // 2026-08-12T00:00:00Z
        assert_eq!(civil_from_unix(1786492800), (2026, 8, 12));
        // Leap day
        assert_eq!(civil_from_unix(1709164800), (2024, 2, 29));
    }

    #[test]
    fn countdown_override_replaces_the_reset_text() {
        let strings = crate::localization::LanguageId::English.strings();
        let section = UsageSection {
            percentage: 4.3,
            resets_at: None,
            countdown_override: Some("$7.35".to_string()),
        };
        assert_eq!(format_line(&section, strings), "4% \u{00b7} $7.35");
    }

    #[test]
    fn jwt_expiry_reads_exp_claim() {
        // {"typ":"access","exp":1786548700}
        let token = "aaa.eyJ0eXAiOiJhY2Nlc3MiLCJleHAiOjE3ODY1NDg3MDB9.bbb";
        assert_eq!(jwt_expiry(token), Some(1786548700));
        assert_eq!(jwt_expiry("not-a-jwt"), None);
    }

    #[test]
    fn antigravity_summary_prefers_gemini_group() {
        let response: AntigravityQuotaSummaryResponse = serde_json::from_str(
            r#"{
                "groups": [
                    {
                        "displayName": "Claude and GPT models",
                        "buckets": [
                            {
                                "bucketId": "3p-weekly",
                                "window": "weekly",
                                "resetTime": "2026-06-20T18:32:02Z",
                                "remainingFraction": 1
                            },
                            {
                                "bucketId": "3p-5h",
                                "window": "5h",
                                "resetTime": "2026-06-13T23:32:02Z",
                                "remainingFraction": 1
                            }
                        ]
                    },
                    {
                        "displayName": "Gemini Models",
                        "description": "Models within this group: Gemini Flash, Gemini Pro",
                        "buckets": [
                            {
                                "bucketId": "gemini-weekly",
                                "displayName": "Weekly Limit",
                                "window": "weekly",
                                "resetTime": "2026-06-20T17:08:54Z",
                                "remainingFraction": 0.99304295
                            },
                            {
                                "bucketId": "gemini-5h",
                                "displayName": "Five Hour Limit",
                                "window": "5h",
                                "resetTime": "2026-06-13T22:08:54Z",
                                "remainingFraction": 0.9582575
                            }
                        ]
                    }
                ]
            }"#,
        )
        .expect("summary response should deserialize");

        let usage =
            antigravity_usage_from_summary(response).expect("Gemini quota should be selected");

        assert!((usage.weekly.percentage - 0.695705).abs() < 0.000001);
        assert!((usage.session.percentage - 4.17425).abs() < 0.000001);
        assert!(usage.weekly.resets_at.is_some());
        assert!(usage.session.resets_at.is_some());
    }
}
