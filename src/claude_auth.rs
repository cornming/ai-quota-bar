//! 讀取並在需要時更新 Claude Code 的 OAuth 登入（`.credentials.json`）。
//!
//! 更新流程照 Claude Code 本身的做法，兩個程式才能共用同一個登入檔而不互相把對方登出：
//! 1. 取得同一組目錄鎖：`<設定資料夾>\.oauth_refresh.lock`，再加舊版的 `<設定資料夾>.lock`
//!    （proper-lockfile 格式：建立目錄即上鎖，超過 60 秒沒更新視為棄置）。
//! 2. 在鎖內重新讀檔：別人可能剛更新過，那就直接用新的，不重複更新。
//! 3. POST `https://platform.claude.com/v1/oauth/token`，欄位與 Claude Code 相同。
//! 4. 只改 `claudeAiOauth` 裡跟憑證有關的欄位，其他內容原樣保留，先寫暫存檔再整個換掉。
//!
//! 鎖要盡快放掉：Claude Code 搶不到鎖會重試約 5 次後放棄這次更新。

use crate::http;
use crate::model::{now_ms, FetchError};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const TOKEN_HOST: &str = "platform.claude.com";
const TOKEN_PATH: &str = "/v1/oauth/token";
/// 到期前 5 分鐘就更新（與 Claude Code 相同）。
const REFRESH_MARGIN_MS: i64 = 5 * 60 * 1000;
/// proper-lockfile 的 stale 設定。
const LOCK_STALE: Duration = Duration::from_secs(60);
/// 換 token 每個階段最多等 8 秒，整體遠低於 60 秒的鎖過期時間。
const TOKEN_TIMEOUT_MS: i32 = 8_000;

pub fn creds_path(dir: &Path) -> PathBuf {
    dir.join(".credentials.json")
}

pub struct Creds {
    pub root: Value,
    pub access: String,
    pub expires_at: Option<i64>,
    pub plan: Option<String>,
}

fn expired_msg() -> FetchError {
    FetchError::LoginExpired("Claude 登入已失效，請執行 claude 重新登入".into())
}

/// 讀登入檔。Claude Code 會即時改寫這個檔，讀到一半壞掉就稍等重試。
pub fn load(dir: &Path) -> Result<Creds, FetchError> {
    let path = creds_path(dir);
    let mut last = None;
    for i in 0..3 {
        if i > 0 {
            std::thread::sleep(Duration::from_millis(200));
        }
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(FetchError::NotSetUp("找不到 Claude Code 登入資料".into()))
            }
            Err(e) => {
                last = Some(e.to_string());
                continue;
            }
        };
        let Ok(root) = serde_json::from_str::<Value>(&text) else {
            last = Some("格式錯誤".into());
            continue;
        };
        let o = &root["claudeAiOauth"];
        let Some(access) = o["accessToken"].as_str().map(str::to_string) else {
            return Err(FetchError::NotSetUp("Claude Code 尚未登入訂閱帳號".into()));
        };
        return Ok(Creds {
            expires_at: o["expiresAt"].as_i64(),
            plan: o["subscriptionType"].as_str().map(str::to_string),
            access,
            root,
        });
    }
    Err(FetchError::Other(format!("讀不到 Claude 登入檔（{}）", last.unwrap_or_default())))
}

pub fn needs_refresh(c: &Creds, now: i64) -> bool {
    c.expires_at.is_some_and(|e| now + REFRESH_MARGIN_MS >= e)
}

/// proper-lockfile 相容的目錄鎖。
struct DirLock(PathBuf);

impl DirLock {
    fn try_acquire(path: &Path) -> Result<DirLock, std::io::ErrorKind> {
        for _ in 0..2 {
            match fs::create_dir(path) {
                Ok(()) => return Ok(DirLock(path.to_path_buf())),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = fs::metadata(path)
                        .and_then(|m| m.modified())
                        .map(|t| t.elapsed().unwrap_or_default() > LOCK_STALE)
                        .unwrap_or(false);
                    if !stale {
                        return Err(std::io::ErrorKind::AlreadyExists);
                    }
                    // 棄置的鎖：proper-lockfile 也是直接移除後重試。
                    let _ = fs::remove_dir(path);
                }
                Err(e) => return Err(e.kind()),
            }
        }
        Err(std::io::ErrorKind::AlreadyExists)
    }
}

impl Drop for DirLock {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.0);
    }
}

/// 舊版鎖：`realpath(設定資料夾) + ".lock"`，例如 `C:\Users\me\.claude.lock`。
fn legacy_lock_path(dir: &Path) -> PathBuf {
    let real = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let s = real.display().to_string();
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
    PathBuf::from(format!("{s}.lock"))
}

/// 取得兩把鎖。搶不到時稍等再試，仍不行就回 `None`（交給下次更新）。
fn lock(dir: &Path) -> Option<(DirLock, Option<DirLock>)> {
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(1000 + attempt * 300));
        }
        let Ok(main) = DirLock::try_acquire(&dir.join(".oauth_refresh.lock")) else {
            continue;
        };
        match DirLock::try_acquire(&legacy_lock_path(dir)) {
            Ok(legacy) => return Some((main, Some(legacy))),
            // 舊版鎖被別人拿著：放掉主鎖再試（Claude Code 也是這樣）。
            Err(std::io::ErrorKind::AlreadyExists) => drop(main),
            // 其他原因拿不到舊版鎖（例如沒權限）：Claude Code 只記錄後繼續。
            Err(_) => return Some((main, None)),
        }
    }
    None
}

/// 把換 token 的回應套進登入檔內容，回傳新的 access token。
pub fn apply_token_response(root: &mut Value, r: &Value, now: i64) -> Result<String, FetchError> {
    let access = r["access_token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| FetchError::Other("Claude 換 token 的回應格式無法辨識".into()))?
        .to_string();
    let o = root
        .get_mut("claudeAiOauth")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| FetchError::Other("Claude 登入檔格式無法辨識".into()))?;
    o.insert("accessToken".into(), json!(access));
    // 沒回新的 refresh token 就沿用舊的。
    if let Some(rt) = r["refresh_token"].as_str().filter(|s| !s.is_empty()) {
        o.insert("refreshToken".into(), json!(rt));
    }
    if let Some(sec) = r["expires_in"].as_i64() {
        o.insert("expiresAt".into(), json!(now + sec * 1000));
    }
    if let Some(scope) = r["scope"].as_str() {
        let scopes: Vec<&str> = scope.split(' ').filter(|s| !s.is_empty()).collect();
        if !scopes.is_empty() {
            o.insert("scopes".into(), json!(scopes));
        }
    }
    // refresh token 的期限取新舊較早者（Claude Code 的規則）。
    if let Some(sec) = r["refresh_token_expires_in"].as_i64() {
        let new = now + sec * 1000;
        if let Some(old) = o.get("refreshTokenExpiresAt").and_then(Value::as_i64).filter(|v| *v > 0) {
            o.insert("refreshTokenExpiresAt".into(), json!(old.min(new)));
        }
    }
    Ok(access)
}

/// 先寫暫存檔再取代，Claude Code 不會讀到寫一半的檔案。
///
/// 換 token 成功後舊的 refresh token 就失效了，所以這一步不能輕易放棄：
/// 取代失敗（例如防毒軟體暫時鎖住檔案）會重試幾次；最後仍失敗就保留暫存檔，
/// 新的登入至少還在硬碟上，可以手動救回。
fn save(dir: &Path, root: &Value) -> Result<(), FetchError> {
    let path = creds_path(dir);
    let tmp = dir.join(format!(".credentials.json.tmp.aiquota{}", std::process::id()));
    let data = serde_json::to_vec(root).map_err(|e| FetchError::Other(e.to_string()))?;
    let mut last = None;
    for i in 0..10 {
        if i > 0 {
            std::thread::sleep(Duration::from_millis(300));
        }
        if let Err(e) = fs::write(&tmp, &data) {
            last = Some(e);
            continue;
        }
        match fs::rename(&tmp, &path) {
            Ok(()) => return Ok(()),
            Err(e) => last = Some(e),
        }
    }
    Err(FetchError::Other(format!(
        "無法儲存更新後的 Claude 登入（{}），新登入暫存在 {}",
        last.map(|e| e.to_string()).unwrap_or_default(),
        tmp.display()
    )))
}

/// 正在換 token 的數量；程式結束前要等它歸零，不然可能換到一半就被關掉。
static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

struct InFlight;
impl InFlight {
    fn start() -> Self {
        IN_FLIGHT.fetch_add(1, Ordering::SeqCst);
        InFlight
    }
}
impl Drop for InFlight {
    fn drop(&mut self) {
        IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
    }
}

/// 等進行中的更新做完（最多 `max`）。
pub fn wait_idle(max: Duration) {
    let start = std::time::Instant::now();
    while IN_FLIGHT.load(Ordering::SeqCst) > 0 && start.elapsed() < max {
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// 換新 token 用的 HTTP 呼叫，測試時可以換掉。
pub type PostFn<'a> = &'a dyn Fn(&str) -> Result<(u32, String), String>;

pub fn post_token(body: &str) -> Result<(u32, String), String> {
    http::post_json(TOKEN_HOST, TOKEN_PATH, body, TOKEN_TIMEOUT_MS)
}

/// 更新登入。`rejected` 是伺服器剛拒絕的 token：檔案裡已經是別的 token 就直接用，
/// 否則即使看起來沒過期也強制更新。
pub fn refresh(dir: &Path, rejected: Option<&str>, post: PostFn) -> Result<String, FetchError> {
    let _busy = InFlight::start();
    let Some(_locks) = lock(dir) else {
        return Err(FetchError::Other("Claude Code 正在更新登入，稍後自動重試".into()));
    };
    let mut c = load(dir)?;
    match rejected {
        Some(bad) if c.access != bad => return Ok(c.access),
        None if !needs_refresh(&c, now_ms()) => return Ok(c.access),
        _ => {}
    }
    let o = &c.root["claudeAiOauth"];
    let refresh_token = o["refreshToken"].as_str().filter(|s| !s.is_empty()).ok_or_else(expired_msg)?;
    let scopes: Vec<&str> = o["scopes"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    let mut body = json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": CLIENT_ID,
    });
    if !scopes.is_empty() {
        body["scope"] = json!(scopes.join(" "));
    }
    let (status, text) = post(&body.to_string()).map_err(FetchError::Other)?;
    match status {
        200 => {}
        400 | 401 | 403 => return Err(expired_msg()),
        429 => return Err(FetchError::Other("Claude 換 token 太頻繁，稍後再試".into())),
        s => return Err(FetchError::Other(format!("Claude 換 token 失敗（HTTP {s}）"))),
    }
    let r: Value =
        serde_json::from_str(&text).map_err(|_| FetchError::Other("Claude 換 token 的回應格式無法辨識".into()))?;
    let access = apply_token_response(&mut c.root, &r, now_ms())?;
    save(dir, &c.root)?;
    Ok(access)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!("aiquota-test-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
            let _ = fs::remove_dir(legacy_lock_path(&self.0));
        }
    }

    fn write_creds(dir: &Path, access: &str, expires_at: i64) {
        let v = json!({
            "mcpOAuth": {"keep": "me"},
            "claudeAiOauth": {
                "accessToken": access, "refreshToken": "rt-old", "expiresAt": expires_at,
                "refreshTokenExpiresAt": 5_000_000, "scopes": ["user:inference", "user:profile"],
                "subscriptionType": "pro", "rateLimitTier": "default_claude_ai",
            },
            "trustedDeviceToken": "keep-too",
        });
        fs::write(creds_path(dir), v.to_string()).unwrap();
    }

    #[test]
    fn applies_response_and_keeps_other_fields() {
        let mut root = json!({"other": 1, "claudeAiOauth": {"accessToken": "a", "refreshToken": "r",
            "refreshTokenExpiresAt": 5_000, "subscriptionType": "pro"}});
        let r = json!({"access_token": "a2", "refresh_token": "r2", "expires_in": 3600,
            "scope": "user:inference user:profile", "refresh_token_expires_in": 10});
        assert_eq!(apply_token_response(&mut root, &r, 1_000).unwrap(), "a2");
        let o = &root["claudeAiOauth"];
        assert_eq!(o["accessToken"], "a2");
        assert_eq!(o["refreshToken"], "r2");
        assert_eq!(o["expiresAt"], 3_601_000);
        assert_eq!(o["scopes"], json!(["user:inference", "user:profile"]));
        assert_eq!(o["refreshTokenExpiresAt"], 5_000, "取新舊較早者");
        assert_eq!(o["subscriptionType"], "pro");
        assert_eq!(root["other"], 1);
    }

    #[test]
    fn keeps_refresh_token_when_not_returned() {
        let mut root = json!({"claudeAiOauth": {"accessToken": "a", "refreshToken": "r"}});
        apply_token_response(&mut root, &json!({"access_token": "a2", "expires_in": 60}), 0).unwrap();
        assert_eq!(root["claudeAiOauth"]["refreshToken"], "r");
        assert!(apply_token_response(&mut root, &json!({"error": "x"}), 0).is_err());
    }

    #[test]
    fn refreshes_and_writes_file_atomically() {
        let t = TempDir::new("ok");
        write_creds(&t.0, "old", 1);
        let sent = RefCell::new(String::new());
        let post = |b: &str| {
            *sent.borrow_mut() = b.to_string();
            Ok((200, r#"{"access_token":"new","refresh_token":"rt-new","expires_in":28800}"#.to_string()))
        };
        assert_eq!(refresh(&t.0, None, &post).unwrap(), "new");
        let body: Value = serde_json::from_str(&sent.borrow()).unwrap();
        assert_eq!(body["grant_type"], "refresh_token");
        assert_eq!(body["refresh_token"], "rt-old");
        assert_eq!(body["client_id"], CLIENT_ID);
        assert_eq!(body["scope"], "user:inference user:profile");
        let saved: Value = serde_json::from_str(&fs::read_to_string(creds_path(&t.0)).unwrap()).unwrap();
        assert_eq!(saved["claudeAiOauth"]["accessToken"], "new");
        assert_eq!(saved["claudeAiOauth"]["refreshToken"], "rt-new");
        assert_eq!(saved["mcpOAuth"]["keep"], "me");
        assert_eq!(saved["trustedDeviceToken"], "keep-too");
        // 鎖與暫存檔都要清掉。
        assert!(!t.0.join(".oauth_refresh.lock").exists());
        assert!(!legacy_lock_path(&t.0).exists());
        let leftovers: Vec<_> = fs::read_dir(&t.0).unwrap().filter_map(|e| e.ok()).collect();
        assert_eq!(leftovers.len(), 1, "只剩 .credentials.json");
    }

    #[test]
    fn skips_when_someone_else_already_refreshed() {
        let t = TempDir::new("skip");
        write_creds(&t.0, "fresh-from-claude", now_ms() + 3_600_000);
        let post = |_: &str| -> Result<(u32, String), String> { panic!("不該送出") };
        // 伺服器拒絕的是舊 token，但檔案裡已經是新的。
        assert_eq!(refresh(&t.0, Some("stale"), &post).unwrap(), "fresh-from-claude");
        // 沒被拒絕而且還沒快到期：也不更新。
        assert_eq!(refresh(&t.0, None, &post).unwrap(), "fresh-from-claude");
    }

    #[test]
    fn rejected_refresh_token_leaves_file_untouched() {
        let t = TempDir::new("bad");
        write_creds(&t.0, "old", 1);
        let before = fs::read_to_string(creds_path(&t.0)).unwrap();
        let post = |_: &str| Ok((400, r#"{"error":"invalid_grant"}"#.to_string()));
        assert!(matches!(refresh(&t.0, None, &post), Err(FetchError::LoginExpired(_))));
        assert_eq!(fs::read_to_string(creds_path(&t.0)).unwrap(), before);
        assert!(!t.0.join(".oauth_refresh.lock").exists());
    }

    #[test]
    fn respects_lock_held_by_claude_code() {
        let t = TempDir::new("locked");
        write_creds(&t.0, "old", 1);
        fs::create_dir(t.0.join(".oauth_refresh.lock")).unwrap();
        let post = |_: &str| -> Result<(u32, String), String> { panic!("拿不到鎖不該送出") };
        assert!(matches!(refresh(&t.0, None, &post), Err(FetchError::Other(_))));
        assert!(t.0.join(".oauth_refresh.lock").exists(), "別人的鎖不能刪");
        fs::remove_dir(t.0.join(".oauth_refresh.lock")).unwrap();
    }

    #[test]
    fn missing_file_is_not_set_up() {
        let t = TempDir::new("none");
        assert!(matches!(load(&t.0), Err(FetchError::NotSetUp(_))));
    }
}
