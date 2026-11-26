//! 檢查 GitHub Releases 新版本與安全自我更新。
//!
//! 流程：
//! 1. 查詢 `https://api.github.com/repos/cornming/ai-quota-bar/releases/latest`。
//! 2. 比較語意化版本（只接受正式版，跳過 draft 與 prerelease）。
//! 3. 使用者確認更新後，以嚴格的 HTTPS＋GitHub 主機白名單下載執行檔。
//! 4. 驗證 PE 檔頭（MZ）、檔案大小與 SHA-256 校驗碼（取自 GitHub asset `digest` 或
//!    Release 內文）；驗證不過絕不取代。
//! 5. 利用 Windows 允許更名執行中 EXE 的特性，先將目前 EXE 改名為 `.old` 再換入新檔
//!    （若失敗立即還原），重啟後由新程式背景清掉 `.old`。

use crate::http;
use serde_json::Value;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const RELEASES_PATH: &str = "/repos/cornming/ai-quota-bar/releases/latest";
pub const INITIAL_DELAY: Duration = Duration::from_secs(15);
pub const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 3600);
const MAX_EXE_BYTES: usize = 16 << 20;
const MIN_EXE_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct ReleaseInfo {
    /// 不含 `v` 前綴的版本號，例如 "0.7.0"。
    pub version: String,
    /// GitHub tag 名稱，例如 "v0.7.0"。
    pub tag: String,
    pub html_url: String,
    pub download_url: String,
    pub asset_name: String,
    pub expected_size: Option<u64>,
    /// 64 碼小寫十六進位 SHA-256。
    pub sha256: Option<String>,
    /// 給確認對話框顯示的精簡更新摘要。
    pub summary: String,
}

/// 解析 "v0.6.0" 或 "0.6.0" 為 (major, minor, patch)。
pub fn parse_semver(s: &str) -> Option<(u32, u32, u32)> {
    let clean = s.trim().trim_start_matches(['v', 'V']);
    // 忽略含 pre-release 後綴（例如 0.7.0-rc1）的字串。
    if clean.contains('-') || clean.contains('+') {
        return None;
    }
    let mut parts = clean.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_semver(latest), parse_semver(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

/// 下載更新檔時允許連線與重新導向的 GitHub 主機白名單。
pub fn allowed_download_host(host: &str) -> bool {
    matches!(
        host.to_ascii_lowercase().as_str(),
        "github.com"
            | "api.github.com"
            | "objects.githubusercontent.com"
            | "release-assets.githubusercontent.com"
            | "github-releases.githubusercontent.com"
    )
}

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// 從 GitHub asset 的 `digest`（`sha256:...`）或 Release 說明內文擷取 SHA-256。
fn extract_sha256(asset: &Value, body: &str) -> Option<String> {
    if let Some(d) = asset["digest"].as_str() {
        let hex = d.strip_prefix("sha256:").or_else(|| d.strip_prefix("SHA256:")).unwrap_or(d).trim();
        if is_sha256_hex(hex) {
            return Some(hex.to_ascii_lowercase());
        }
    }
    // 後備：從 Release 內文的「SHA-256」那一行找出 64 碼 hex。
    for line in body.lines() {
        if line.to_ascii_uppercase().contains("SHA-256") {
            for token in line.split(|c: char| !c.is_ascii_hexdigit()) {
                if is_sha256_hex(token) {
                    return Some(token.to_ascii_lowercase());
                }
            }
        }
    }
    None
}

/// 擷取 Release 內文的更新重點（略過「## 下載」之後的說明，避免對話框過長）。
fn summarize_body(body: &str) -> String {
    let mut lines = Vec::new();
    for raw in body.lines() {
        let line = raw.trim();
        if line.starts_with("## 下載") || line.starts_with("## Download") {
            break;
        }
        if line.is_empty() || line.starts_with("## ") {
            continue;
        }
        // 去掉常見的 Markdown 粗體符號，讓純文字對話框好讀。
        let clean = line.replace("**", "").replace('`', "");
        lines.push(clean);
        if lines.len() >= 6 {
            break;
        }
    }
    let joined = lines.join("\n");
    if joined.chars().count() > 360 {
        let mut s: String = joined.chars().take(360).collect();
        s.push('…');
        s
    } else {
        joined
    }
}

/// 解析 GitHub `/releases/latest` 回應；沒有較新正式版時回傳 `Ok(None)`。
pub fn parse_release_json(json: &str, current: &str) -> Result<Option<ReleaseInfo>, String> {
    let v: Value = serde_json::from_str(json).map_err(|_| "無法解析 GitHub 更新資訊".to_string())?;
    if v["draft"].as_bool() == Some(true) || v["prerelease"].as_bool() == Some(true) {
        return Ok(None);
    }
    let tag = v["tag_name"].as_str().ok_or_else(|| "GitHub 更新資訊缺少版本標籤".to_string())?;
    if !is_newer(tag, current) {
        return Ok(None);
    }
    let version = tag.trim().trim_start_matches(['v', 'V']).to_string();
    let html_url = v["html_url"]
        .as_str()
        .filter(|u| u.starts_with("https://github.com/"))
        .unwrap_or("https://github.com/cornming/ai-quota-bar/releases/latest")
        .to_string();
    let assets = v["assets"].as_array().ok_or_else(|| "新版本尚未附上執行檔".to_string())?;
    let exe_asset = assets
        .iter()
        .find(|a| {
            a["name"]
                .as_str()
                .is_some_and(|n| n.starts_with("ai-quota-bar") && n.to_ascii_lowercase().ends_with(".exe"))
        })
        .or_else(|| assets.iter().find(|a| a["name"].as_str().is_some_and(|n| n.to_ascii_lowercase().ends_with(".exe"))))
        .ok_or_else(|| "新版本找不到可下載的 .exe 執行檔".to_string())?;
    let download_url = exe_asset["browser_download_url"]
        .as_str()
        .filter(|u| u.starts_with("https://"))
        .ok_or_else(|| "新版本下載連結無效".to_string())?
        .to_string();
    let asset_name = exe_asset["name"].as_str().unwrap_or("ai-quota-bar.exe").to_string();
    let expected_size = exe_asset["size"].as_u64();
    let body = v["body"].as_str().unwrap_or("");
    let sha256 = extract_sha256(exe_asset, body);
    let summary = summarize_body(body);
    Ok(Some(ReleaseInfo {
        version,
        tag: tag.to_string(),
        html_url,
        download_url,
        asset_name,
        expected_size,
        sha256,
        summary,
    }))
}

/// 查詢 GitHub 是否有比目前更新的正式版。
pub fn check_latest() -> Result<Option<ReleaseInfo>, String> {
    let (status, body) = http::get(
        "api.github.com",
        RELEASES_PATH,
        &[
            ("Accept", "application/vnd.github+json"),
            ("X-GitHub-Api-Version", "2022-11-28"),
        ],
    )?;
    match status {
        200 => parse_release_json(&body, CURRENT_VERSION),
        403 | 429 => Err("GitHub API 查詢太頻繁，稍後再試".into()),
        404 => Ok(None),
        s => Err(format!("檢查更新失敗（HTTP {s}）")),
    }
}

/// 驗證下載回來的執行檔（大小、PE 檔頭 MZ、SHA-256）。
pub fn verify_exe(bytes: &[u8], info: &ReleaseInfo) -> Result<(), String> {
    if bytes.len() < MIN_EXE_BYTES || !bytes.starts_with(b"MZ") {
        return Err("下載的檔案不是有效的 Windows 執行檔".into());
    }
    if let Some(sz) = info.expected_size {
        if bytes.len() as u64 != sz {
            return Err(format!("檔案大小不符（預期 {sz} bytes，實際 {} bytes）", bytes.len()));
        }
    }
    let expected = info
        .sha256
        .as_deref()
        .ok_or_else(|| "此版本缺少 SHA-256 校驗碼，為安全起見請改由瀏覽器手動下載".to_string())?;
    let actual = sha256_hex(bytes);
    if actual != expected {
        return Err("SHA-256 校驗碼不符，已中止更新".into());
    }
    Ok(())
}

/// 產生同目錄下的旁置檔路徑（例如 `ai-quota-bar.exe.old`）。
pub fn sidecar_path(exe: &Path, suffix: &str) -> PathBuf {
    let mut os = exe.as_os_str().to_os_string();
    os.push(suffix);
    PathBuf::from(os)
}

/// 以更名方式原地取代執行檔；若第二步驟失敗會自動還原舊檔，不會留下空窗。
pub fn swap_executable(current_exe: &Path, new_bytes: &[u8]) -> Result<PathBuf, String> {
    let tmp_new = sidecar_path(current_exe, ".new");
    let backup_old = sidecar_path(current_exe, ".old");

    {
        let mut f = File::create(&tmp_new).map_err(|e| {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                "目前執行檔所在資料夾沒有寫入權限，請改由瀏覽器手動下載取代".to_string()
            } else {
                format!("無法寫入更新暫存檔：{e}")
            }
        })?;
        f.write_all(new_bytes).map_err(|e| format!("寫入更新暫存檔失敗：{e}"))?;
        let _ = f.sync_all();
    }

    let _ = fs::remove_file(&backup_old);
    if let Err(e) = fs::rename(current_exe, &backup_old) {
        let _ = fs::remove_file(&tmp_new);
        return Err(format!("無法備份舊版執行檔：{e}"));
    }
    if let Err(e) = fs::rename(&tmp_new, current_exe) {
        // 還原舊版執行檔，確保使用者不會失去程式。
        let _ = fs::rename(&backup_old, current_exe);
        let _ = fs::remove_file(&tmp_new);
        return Err(format!("無法取代執行檔：{e}"));
    }
    Ok(backup_old)
}

/// 下載、驗證並原地取代目前的執行檔；成功時回傳要重新啟動的執行檔路徑。
pub fn download_and_stage(info: &ReleaseInfo) -> Result<PathBuf, String> {
    let current_exe = std::env::current_exe().map_err(|e| format!("找不到目前執行檔路徑：{e}"))?;
    let bytes = http::download_https(&info.download_url, allowed_download_host, MAX_EXE_BYTES, 30_000)?;
    verify_exe(&bytes, info)?;
    swap_executable(&current_exe, &bytes)?;
    Ok(current_exe)
}

/// 啟動時在背景清理上次更新留下的 `.old` 與 `.new` 暫存檔。
pub fn cleanup_old_exe() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let old = sidecar_path(&exe, ".old");
    let tmp = sidecar_path(&exe, ".new");
    if !old.exists() && !tmp.exists() {
        return;
    }
    std::thread::spawn(move || {
        for _ in 0..15 {
            let old_gone = !old.exists() || fs::remove_file(&old).is_ok();
            let tmp_gone = !tmp.exists() || fs::remove_file(&tmp).is_ok();
            if old_gone && tmp_gone {
                return;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    });
}

// ---------- 純 Rust SHA-256（FIPS 180-4，無額外相依套件） ----------

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

pub fn sha256_hex(data: &[u8]) -> String {
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut padded = data.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in padded.chunks_exact(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([chunk[i * 4], chunk[i * 4 + 1], chunk[i * 4 + 2], chunk[i * 4 + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (dst, add) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *dst = dst.wrapping_add(add);
        }
    }

    let mut out = String::with_capacity(64);
    for word in h {
        use std::fmt::Write;
        let _ = write!(&mut out, "{word:08x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_semver_versions() {
        assert!(is_newer("v0.7.0", "0.6.0"));
        assert!(is_newer("v1.0.0", "0.9.9"));
        assert!(is_newer("0.6.1", "0.6.0"));
        assert!(!is_newer("v0.6.0", "0.6.0"));
        assert!(!is_newer("v0.5.0", "0.6.0"));
        assert!(!is_newer("v0.7.0-rc1", "0.6.0"));
        assert!(!is_newer("garbage", "0.6.0"));
    }

    #[test]
    fn enforces_github_download_host_allowlist() {
        assert!(allowed_download_host("github.com"));
        assert!(allowed_download_host("release-assets.githubusercontent.com"));
        assert!(allowed_download_host("OBJECTS.GITHUBUSERCONTENT.COM"));
        assert!(!allowed_download_host("evil-github.com"));
        assert!(!allowed_download_host("github.com.evil.example"));
    }

    #[test]
    fn sha256_matches_nist_vectors() {
        assert_eq!(sha256_hex(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn parses_release_json_and_extracts_digest() {
        let json = r###"{
            "tag_name": "v0.7.0",
            "draft": false,
            "prerelease": false,
            "html_url": "https://github.com/cornming/ai-quota-bar/releases/tag/v0.7.0",
            "body": "## 新增\n\n- **自動更新**：支援一鍵更新\n\n## 下載\n\nSHA-256：`aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa`",
            "assets": [{
                "name": "ai-quota-bar-v0.7.0.exe",
                "size": 512000,
                "digest": "sha256:2221d17d2ded343d464478feadd643e2dbb4389b090f9921d2b1b2ac7373a360",
                "browser_download_url": "https://github.com/cornming/ai-quota-bar/releases/download/v0.7.0/ai-quota-bar-v0.7.0.exe"
            }]
        }"###;
        let info = parse_release_json(json, "0.6.0").unwrap().expect("應偵測到新版本");
        assert_eq!(info.version, "0.7.0");
        assert_eq!(info.sha256.as_deref(), Some("2221d17d2ded343d464478feadd643e2dbb4389b090f9921d2b1b2ac7373a360"));
        assert!(info.summary.contains("自動更新"));
        assert!(!info.summary.contains("下載"));
        // 相同或較舊版本回傳 None。
        assert!(parse_release_json(json, "0.7.0").unwrap().is_none());
        assert!(parse_release_json(json, "0.8.0").unwrap().is_none());
    }

    /// 實測對 GitHub Releases API 查詢並走完整 HTTPS 重新導向下載＋SHA-256 驗證：
    /// `cargo test live_github_release_download -- --ignored`
    #[test]
    #[ignore]
    fn live_github_release_download() {
        let (status, body) = http::get(
            "api.github.com",
            RELEASES_PATH,
            &[("Accept", "application/vnd.github+json"), ("X-GitHub-Api-Version", "2022-11-28")],
        )
        .unwrap();
        assert_eq!(status, 200);
        let info = parse_release_json(&body, "0.0.1").unwrap().expect("應找到最新版本");
        let bytes = http::download_https(&info.download_url, allowed_download_host, MAX_EXE_BYTES, 30_000).unwrap();
        verify_exe(&bytes, &info).unwrap();
    }

    #[test]
    fn verifies_exe_and_swaps_atomically() {
        let mut fake_exe = vec![0u8; 70 * 1024];
        fake_exe[0] = b'M';
        fake_exe[1] = b'Z';
        let digest = sha256_hex(&fake_exe);
        let info = ReleaseInfo {
            version: "0.7.0".into(),
            tag: "v0.7.0".into(),
            html_url: "https://github.com/cornming/ai-quota-bar/releases/tag/v0.7.0".into(),
            download_url: "https://github.com/cornming/ai-quota-bar/releases/download/v0.7.0/a.exe".into(),
            asset_name: "a.exe".into(),
            expected_size: Some(fake_exe.len() as u64),
            sha256: Some(digest),
            summary: String::new(),
        };
        assert!(verify_exe(&fake_exe, &info).is_ok());

        let mut tampered = fake_exe.clone();
        tampered[100] ^= 1;
        assert!(verify_exe(&tampered, &info).is_err());

        let dir = std::env::temp_dir().join(format!("aiquota-update-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("ai-quota-bar.exe");
        fs::write(&exe, b"OLD-VERSION").unwrap();
        let old = swap_executable(&exe, &fake_exe).unwrap();
        assert_eq!(fs::read(&exe).unwrap(), fake_exe);
        assert_eq!(fs::read(&old).unwrap(), b"OLD-VERSION");
        assert!(!sidecar_path(&exe, ".new").exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
