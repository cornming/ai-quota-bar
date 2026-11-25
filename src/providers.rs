//! 四家的額度來源。每家分成「取得」（讀登入資料、連線）與「解析」（純函式，有測試）。
//!
//! 都是讀各工具已經存好的登入資料；只有 Claude 的登入快到期時會照 Claude Code 的流程更新並寫回（見 claude_auth）。

use crate::{claude_auth, http};
use crate::model::*;
use serde_json::Value;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    Claude,
    Codex,
    Copilot,
    Antigravity,
}

pub const ALL: [Provider; 4] = [Provider::Claude, Provider::Codex, Provider::Copilot, Provider::Antigravity];

impl Provider {
    pub fn index(self) -> usize {
        self as usize
    }
    pub fn name(self) -> &'static str {
        match self {
            Provider::Claude => "Claude",
            Provider::Codex => "Codex",
            Provider::Copilot => "Copilot",
            Provider::Antigravity => "Antigravity",
        }
    }
    /// 工作列上的名稱。
    pub fn short(self) -> &'static str {
        match self {
            Provider::Claude => "CLAUDE",
            Provider::Codex => "CODEX",
            Provider::Copilot => "COPILOT",
            Provider::Antigravity => "AGY",
        }
    }
    /// 自動更新間隔。Antigravity 每次要啟動 CLI，所以拉長。
    pub fn interval(self) -> Duration {
        match self {
            Provider::Antigravity => Duration::from_secs(600),
            _ => Duration::from_secs(120),
        }
    }
    pub fn fetch(self) -> Result<Quota, FetchError> {
        match self {
            Provider::Claude => claude(),
            Provider::Codex => codex(),
            Provider::Copilot => copilot(),
            Provider::Antigravity => antigravity(),
        }
    }
}

fn home() -> PathBuf {
    std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

/// 讀 JSON 檔。各工具會即時改寫登入檔，讀到一半壞掉就稍等重試。
fn read_json(path: &PathBuf) -> Option<Value> {
    for i in 0..3 {
        if let Ok(s) = std::fs::read_to_string(path) {
            if let Ok(v) = serde_json::from_str(&s) {
                return Some(v);
            }
        }
        if i < 2 {
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    None
}

fn http_error(who: &str, status: u32, body: &str) -> FetchError {
    match status {
        401 | 403 => FetchError::LoginExpired(format!("{who} 登入已過期，開啟 {who} 後會自動恢復")),
        429 => FetchError::Other(format!("{who} 查詢太頻繁，稍後再試")),
        s => FetchError::Other(format!("{who} 回應錯誤（HTTP {s}）{}", body.chars().take(80).collect::<String>())),
    }
}

fn parse_body(who: &str, body: &str) -> Result<Value, FetchError> {
    serde_json::from_str(body).map_err(|_| FetchError::Other(format!("{who} 回應格式無法辨識")))
}

fn pct(v: &Value) -> Option<f64> {
    v.as_f64().filter(|p| p.is_finite())
}

// ---------- Claude ----------

fn claude_dir() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).unwrap_or_else(|| home().join(".claude"))
}

fn claude_usage(token: &str) -> Result<(u32, String), FetchError> {
    http::get(
        "api.anthropic.com",
        "/api/oauth/usage",
        &[
            ("Authorization", &format!("Bearer {token}")),
            ("anthropic-beta", "oauth-2025-04-20"),
            ("Accept", "application/json"),
        ],
    )
    .map_err(FetchError::Other)
}

static LAST_FORCED_REFRESH: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);

fn may_force_refresh() -> bool {
    let mut last = LAST_FORCED_REFRESH.lock().unwrap();
    if last.is_some_and(|t| t.elapsed() < Duration::from_secs(30 * 60)) {
        return false;
    }
    *last = Some(Instant::now());
    true
}

/// 讀 Claude Code 的登入；快到期或被拒絕時，照 Claude Code 的流程更新登入再查一次。
fn claude() -> Result<Quota, FetchError> {
    let dir = claude_dir();
    let creds = claude_auth::load(&dir)?;
    let plan = creds.plan.clone();
    let mut token = if claude_auth::needs_refresh(&creds, now_ms()) {
        claude_auth::refresh(&dir, None, &claude_auth::post_token)?
    } else {
        creds.access
    };
    let (mut status, mut body) = claude_usage(&token)?;
    // 被拒絕時強制換一次 token；但如果換完還是被拒，30 分鐘內不再強制換，
    // 免得伺服器端有別的問題時每 2 分鐘就輪替一次登入。
    if status == 401 && may_force_refresh() {
        token = claude_auth::refresh(&dir, Some(&token), &claude_auth::post_token)?;
        (status, body) = claude_usage(&token)?;
    }
    if status != 200 {
        return Err(http_error("Claude", status, &body));
    }
    parse_claude(&parse_body("Claude", &body)?, plan)
}

pub fn parse_claude(v: &Value, plan: Option<String>) -> Result<Quota, FetchError> {
    let win = |key: &str, short: &str, name: &str| -> Option<Window> {
        let w = &v[key];
        Some(Window {
            short: short.into(),
            name: name.into(),
            used: pct(&w["utilization"])?,
            resets_at: w["resets_at"].as_str().and_then(parse_time),
            detail: None,
        })
    };
    let windows: Vec<Window> = [win("five_hour", "5H", "5 小時"), win("seven_day", "7D", "每週")]
        .into_iter()
        .flatten()
        .collect();
    let extra: Vec<Window> = [
        win("seven_day_opus", "Op", "每週 Opus"),
        win("seven_day_sonnet", "So", "每週 Sonnet"),
        win("seven_day_oauth_apps", "App", "每週 外部 App"),
        win("seven_day_cowork", "Co", "每週 Cowork"),
    ]
    .into_iter()
    .flatten()
    .collect();
    if windows.is_empty() && extra.is_empty() {
        return Err(FetchError::Other("Claude 沒有回報額度（免費方案沒有用量上限資料）".into()));
    }
    Ok(Quota { plan, windows, extra })
}

// ---------- Codex ----------

fn codex() -> Result<Quota, FetchError> {
    let dir = std::env::var_os("CODEX_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".codex"));
    let auth = read_json(&dir.join("auth.json")).ok_or_else(|| FetchError::NotSetUp("找不到 Codex 登入資料".into()))?;
    let t = &auth["tokens"];
    let (Some(token), Some(account)) = (t["access_token"].as_str(), t["account_id"].as_str()) else {
        return Err(FetchError::NotSetUp("Codex 尚未用 ChatGPT 帳號登入".into()));
    };
    let (status, body) = http::get(
        "chatgpt.com",
        "/backend-api/wham/usage",
        &[
            ("Authorization", &format!("Bearer {token}")),
            ("ChatGPT-Account-Id", account),
            ("Accept", "application/json"),
        ],
    )
    .map_err(FetchError::Other)?;
    if status != 200 {
        return Err(http_error("Codex", status, &body));
    }
    parse_codex(&parse_body("Codex", &body)?, now_secs())
}

pub fn parse_codex(v: &Value, now: i64) -> Result<Quota, FetchError> {
    let rl = &v["rate_limit"];
    let windows: Vec<Window> = ["primary_window", "secondary_window"]
        .iter()
        .filter_map(|k| {
            let w = &rl[*k];
            let used = pct(&w["used_percent"])?;
            let (short, name) = window_label(w["limit_window_seconds"].as_i64().unwrap_or(0));
            let resets_at = w["reset_at"]
                .as_i64()
                .or_else(|| w["reset_after_seconds"].as_i64().map(|s| now + s));
            Some(Window { short, name, used, resets_at, detail: None })
        })
        .collect();
    if windows.is_empty() {
        return Err(FetchError::Other("Codex 沒有回報額度".into()));
    }
    Ok(Quota { plan: v["plan_type"].as_str().map(str::to_string), windows, extra: vec![] })
}

// ---------- GitHub Copilot ----------

/// 隱藏主控台視窗啟動子程序。
fn quiet(cmd: &mut Command) -> &mut Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW)
}

/// 使用者選的 Copilot 帳號（gh 登入的 github.com 帳號名稱）；None 表示用 gh 目前的帳號。
static COPILOT_LOGIN: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// GitHub 帳號名稱只能有英數字、`-`、`_`（企業帳號），而且不能以 `-` 開頭。
/// 登錄檔裡的值會當成 gh 的參數，先擋掉不合格的，避免被當成其他選項。
pub fn valid_login(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && !s.starts_with('-')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

pub fn set_copilot_login(login: Option<String>) {
    *COPILOT_LOGIN.lock().unwrap() = login.filter(|l| valid_login(l));
}

pub fn copilot_login() -> Option<String> {
    COPILOT_LOGIN.lock().unwrap().clone()
}

/// gh 在 github.com 上登入的所有帳號，第二個值表示是否為 gh 目前使用的帳號。
pub fn gh_accounts() -> Vec<(String, bool)> {
    let Ok(out) = quiet(Command::new("gh").args(["auth", "status", "--json", "hosts"])).output() else {
        return vec![];
    };
    parse_gh_accounts(&String::from_utf8_lossy(&out.stdout))
}

pub fn parse_gh_accounts(json: &str) -> Vec<(String, bool)> {
    let v: Value = serde_json::from_str(json).unwrap_or_default();
    v["hosts"]["github.com"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|a| a["state"].as_str() == Some("success"))
        .filter_map(|a| Some((a["login"].as_str()?.to_string(), a["active"].as_bool() == Some(true))))
        .collect()
}

fn gh_token(login: Option<&str>) -> Result<String, FetchError> {
    let mut cmd = Command::new("gh");
    cmd.args(["auth", "token", "--hostname", "github.com"]);
    if let Some(l) = login.filter(|l| valid_login(l)) {
        cmd.arg(format!("--user={l}"));
    }
    let out = quiet(&mut cmd)
        .output()
        .map_err(|_| FetchError::NotSetUp("需要 GitHub CLI（gh）登入才能查 Copilot".into()))?;
    let tok = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || tok.is_empty() {
        return Err(FetchError::NotSetUp(match login {
            Some(l) => format!("GitHub CLI 沒有登入 {l}（執行 gh auth login）"),
            None => "GitHub CLI 尚未登入（執行 gh auth login）".into(),
        }));
    }
    Ok(tok)
}

fn copilot() -> Result<Quota, FetchError> {
    let login = copilot_login();
    let token = gh_token(login.as_deref())?;
    let (status, body) = http::get(
        "api.github.com",
        "/copilot_internal/user",
        &[
            ("Authorization", &format!("token {token}")),
            ("Accept", "application/json"),
            ("Editor-Version", "vscode/1.96.2"),
            ("Editor-Plugin-Version", "copilot-chat/0.26.7"),
            ("User-Agent", "GitHubCopilotChat/0.26.7"),
            ("X-Github-Api-Version", "2025-04-01"),
        ],
    )
    .map_err(FetchError::Other)?;
    if status == 404 {
        return Err(FetchError::NotSetUp("這個 GitHub 帳號沒有 Copilot".into()));
    }
    if status != 200 {
        return Err(http_error("Copilot", status, &body));
    }
    let v = parse_body("Copilot", &body)?;
    let mut q = parse_copilot(&v)?;
    // 提示文字標出是哪個帳號。
    if let Some(user) = v["login"].as_str() {
        q.plan = Some(match q.plan {
            Some(p) => format!("{p} · {user}"),
            None => user.to_string(),
        });
    }
    Ok(q)
}

pub fn parse_copilot(v: &Value) -> Result<Quota, FetchError> {
    if v["access_type_sku"].as_str() == Some("subscription_ended") {
        return Err(FetchError::NotSetUp("Copilot 訂閱已結束".into()));
    }
    let resets_at = v["quota_reset_date_utc"]
        .as_str()
        .or(v["quota_reset_date"].as_str())
        .and_then(parse_time);
    let snaps = &v["quota_snapshots"];
    let windows: Vec<Window> = [
        ("premium_interactions", "PREM", "進階請求"),
        ("chat", "CHAT", "聊天"),
        ("completions", "CODE", "程式碼補全"),
    ]
    .iter()
    .filter_map(|(key, short, name)| {
        let s = &snaps[*key];
        // 免費方案的 premium_interactions 是 entitlement 0、has_quota false，不能當成用完。
        let entitlement = s["entitlement"].as_f64().unwrap_or(0.0);
        if s["unlimited"].as_bool() == Some(true) || entitlement <= 0.0 {
            return None;
        }
        let remaining = pct(&s["percent_remaining"])?;
        let left = s["remaining"].as_f64().or(s["quota_remaining"].as_f64());
        Some(Window {
            short: (*short).into(),
            name: (*name).into(),
            used: (100.0 - remaining).clamp(0.0, 100.0),
            resets_at,
            detail: left.map(|l| format!("剩 {} / {} 次", l.floor() as i64, entitlement as i64)),
        })
    })
    .collect();
    if windows.is_empty() {
        return Err(FetchError::Other("Copilot 沒有有上限的額度（可能是無限方案）".into()));
    }
    let plan = v["access_type_sku"].as_str().or(v["copilot_plan"].as_str()).map(str::to_string);
    Ok(Quota { plan, windows, extra: vec![] })
}

// ---------- Antigravity ----------

fn agy_path() -> Option<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from)?;
    let p = local.join("agy").join("bin").join("agy.exe");
    p.is_file().then_some(p)
}

/// 執行 `agy -p /usage --output-format json`。
///
/// 這個指令本身不會呼叫模型；但萬一 CLI 沒認出 `/usage`，就會被當成一般提問送出去、
/// 消耗額度。所以參數直接傳（不經 shell），而且結果不是 usage 指令就整個停用。
fn antigravity() -> Result<Quota, FetchError> {
    let exe = agy_path().ok_or_else(|| FetchError::NotSetUp("找不到 Antigravity CLI（agy）".into()))?;
    let mut child = quiet(
        Command::new(exe)
            .args(["-p", "/usage", "--output-format", "json"])
            .env("ANTIGRAVITY_DISABLE_AUTOUPDATE", "true")
            .current_dir(std::env::temp_dir())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null()),
    )
    .spawn()
    .map_err(|e| FetchError::Other(format!("無法執行 agy：{e}")))?;
    let mut stdout = child.stdout.take().expect("piped");
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = std::io::Read::read_to_string(&mut stdout, &mut s);
        s
    });
    // 正常的 /usage 約 3 秒；被當成提問時要 40 秒以上。逾時就當成誤送提問，直接停用，
    // 不然每 10 分鐘就會再送一次真的提問。
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(FetchError::Disabled(
                    "agy 查詢超過 20 秒（可能被當成一般提問），為了不消耗額度已停用 Antigravity，重開程式才會再試".into(),
                ));
            }
        }
    }
    let out = reader.join().unwrap_or_default();
    let v: Value = serde_json::from_str(out.trim_start_matches('\u{feff}').trim())
        .map_err(|_| FetchError::Other("agy 回應格式無法辨識".into()))?;
    parse_antigravity(&v)
}

pub fn parse_antigravity(v: &Value) -> Result<Quota, FetchError> {
    let is_usage = v["command"]["name"].as_str() == Some("usage") && v["num_turns"].as_i64() == Some(0);
    if !is_usage {
        return Err(FetchError::Disabled(
            "agy 沒有把 /usage 當指令處理，為了不消耗額度已停用 Antigravity，重開程式才會再試".into(),
        ));
    }
    let mut windows = Vec::new();
    let mut extra = Vec::new();
    for g in v["command"]["data"]["groups"].as_array().into_iter().flatten() {
        let gname = g["name"].as_str().unwrap_or("?");
        let short = if gname.starts_with("Gemini") { "GEM" } else { "C+G" };
        let mut buckets: Vec<Window> = g["buckets"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|b| {
                let remaining = pct(&b["remaining_fraction"])?;
                let win = match b["window"].as_str() {
                    Some("5h") => "5 小時",
                    Some("weekly") => "每週",
                    Some(other) => other,
                    None => "?",
                };
                Some(Window {
                    short: short.into(),
                    name: format!("{gname}（{win}）"),
                    used: ((1.0 - remaining) * 100.0).clamp(0.0, 100.0),
                    resets_at: b["reset_time"].as_str().and_then(parse_time),
                    detail: None,
                })
            })
            .collect();
        if buckets.is_empty() {
            continue;
        }
        // 工作列顯示這組裡最緊的一個，其他放提示文字。
        buckets.sort_by(|a, b| b.used.total_cmp(&a.used));
        let mut it = buckets.into_iter();
        windows.push(it.next().unwrap());
        extra.extend(it);
    }
    if windows.is_empty() {
        return Err(FetchError::Other("Antigravity 沒有回報額度".into()));
    }
    Ok(Quota { plan: None, windows, extra })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Value {
        let path = format!("{}/fixtures/{name}.json", env!("CARGO_MANIFEST_DIR"));
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn claude_fixture() {
        let q = parse_claude(&fixture("claude"), Some("pro".into())).unwrap();
        assert_eq!(q.windows.len(), 2);
        assert_eq!(q.windows[0].short, "5H");
        assert!(q.windows[0].resets_at.is_some());
        assert_eq!(q.windows[1].short, "7D");
        assert!(q.extra.is_empty());
    }

    #[test]
    fn claude_free_has_no_limits() {
        let v = serde_json::json!({"five_hour": null, "seven_day": null});
        assert!(parse_claude(&v, None).is_err());
    }

    #[test]
    fn codex_fixture_monthly_window() {
        let v = fixture("codex");
        let q = parse_codex(&v, 0).unwrap();
        assert_eq!(q.windows.len(), 1, "secondary_window 是 null");
        assert_eq!(q.windows[0].short, "30D");
        assert_eq!(q.windows[0].resets_at, v["rate_limit"]["primary_window"]["reset_at"].as_i64());
        assert_eq!(q.plan.as_deref(), Some("free"));
    }

    #[test]
    fn codex_paid_two_windows() {
        let v = serde_json::json!({"plan_type": "plus", "rate_limit": {
            "primary_window": {"used_percent": 12, "limit_window_seconds": 18000, "reset_after_seconds": 60},
            "secondary_window": {"used_percent": 40, "limit_window_seconds": 604800, "reset_at": 99}}});
        let q = parse_codex(&v, 1000).unwrap();
        assert_eq!(q.windows[0].short, "5H");
        assert_eq!(q.windows[0].resets_at, Some(1060));
        assert_eq!(q.windows[1].short, "7D");
        assert_eq!(q.windows[1].used, 40.0);
    }

    #[test]
    fn copilot_free_skips_zero_premium() {
        let q = parse_copilot(&fixture("copilot")).unwrap();
        let shorts: Vec<&str> = q.windows.iter().map(|w| w.short.as_str()).collect();
        assert_eq!(shorts, ["CHAT", "CODE"]);
        assert!((q.windows[0].used - 0.3).abs() < 1e-9);
        assert_eq!(q.windows[0].resets_at, parse_time("2026-12-01"));
    }

    #[test]
    fn rejects_suspicious_logins() {
        assert!(valid_login("cornming"));
        assert!(valid_login("me_work"));
        assert!(valid_login("some-user"));
        assert!(!valid_login("--hostname=evil.example"));
        assert!(!valid_login("a b"));
        assert!(!valid_login("x\"&calc"));
        assert!(!valid_login(""));
    }

    #[test]
    fn gh_accounts_from_status_json() {
        let j = r#"{"hosts":{"github.com":[
            {"state":"success","active":true,"host":"github.com","login":"me"},
            {"state":"success","active":false,"host":"github.com","login":"me_work"},
            {"state":"error","active":false,"host":"github.com","login":"broken"}]}}"#;
        assert_eq!(parse_gh_accounts(j), vec![("me".into(), true), ("me_work".into(), false)]);
        assert!(parse_gh_accounts("not json").is_empty());
    }

    #[test]
    fn copilot_business_premium_only() {
        let v = serde_json::json!({"access_type_sku": "copilot_for_business_seat_quota",
            "quota_reset_date_utc": "2026-12-01T00:00:00.000Z", "quota_snapshots": {
            "chat": {"unlimited": true, "entitlement": 0, "percent_remaining": 100.0},
            "completions": {"unlimited": true, "entitlement": 0, "percent_remaining": 100.0},
            "premium_interactions": {"unlimited": false, "entitlement": 1900, "remaining": 714, "percent_remaining": 37.5}}});
        let q = parse_copilot(&v).unwrap();
        assert_eq!(q.windows.len(), 1);
        assert_eq!(q.windows[0].short, "PREM");
        assert_eq!(q.windows[0].used, 62.5);
        assert_eq!(q.windows[0].detail.as_deref(), Some("剩 714 / 1900 次"));
    }

    #[test]
    fn antigravity_fixture() {
        let q = parse_antigravity(&fixture("antigravity")).unwrap();
        assert_eq!(q.windows.len(), 2);
        assert_eq!(q.windows[0].short, "GEM");
        assert!((q.windows[0].used - 41.998).abs() < 0.01, "取 5 小時（較緊）");
        assert_eq!(q.windows[1].short, "C+G");
        assert!((q.windows[1].used - 50.87).abs() < 0.01, "取每週（較緊）");
        assert_eq!(q.extra.len(), 2);
    }

    #[test]
    fn antigravity_rejects_prompt_answers() {
        // CLI 若把 /usage 當成提問，回應會有 num_turns > 0 且沒有 command。
        let v = serde_json::json!({"status": "SUCCESS", "response": "…", "num_turns": 1});
        assert!(matches!(parse_antigravity(&v), Err(FetchError::Disabled(_))));
    }
}
