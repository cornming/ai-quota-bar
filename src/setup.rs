//! 各家 AI 工具的環境偵測、官方安裝／登入指令，以及開啟可見 PowerShell 視窗引導設定。
//!
//! 設計原則：
//! - 不在背景靜默下載或執行腳本：一律開啟可見的 PowerShell 視窗，先列出官方指令，
//!   等使用者按 Enter 確認後才執行安裝與登入。
//! - 找執行檔時同時檢查登錄檔裡最新的 PATH 與各工具預設安裝位置，所以使用者在程式
//!   執行期間剛裝完 CLI，不必重開 AI Quota Bar 就能立刻偵測到。

use crate::http::wide;
use crate::model::{FetchError, Quota};
use crate::providers::Provider;
use std::path::PathBuf;
use std::process::Command;
use std::ptr::null_mut;
use windows_sys::Win32::System::Registry::{
    RegGetValueW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ,
};

/// 某家服務的設定指引資料（全為編譯期常數，無任何使用者輸入串接）。
#[derive(Clone, Copy, Debug)]
pub struct Spec {
    /// 工具顯示名稱，例如 "Claude Code (claude)"。
    pub tool: &'static str,
    /// 沒裝桌面 App 圖示時用的簡短徽章文字。
    pub badge: &'static str,
    /// 一句說明（需求與方案）。
    pub summary: &'static str,
    /// 官方安裝指令（PowerShell）。
    pub install_cmd: &'static str,
    /// 登入指令。
    pub login_cmd: &'static str,
    /// 登入時的額外提示（例如如何結束互動式 CLI）。
    pub login_hint: Option<&'static str>,
    /// 官方安裝／說明網頁。
    pub docs_url: &'static str,
}

pub fn spec(p: Provider) -> Spec {
    match p {
        Provider::Claude => Spec {
            tool: "Claude Code (claude)",
            badge: "CL",
            summary: "需安裝並登入 Claude 訂閱帳號（Pro / Max / Team）",
            install_cmd: "irm https://claude.ai/install.ps1 | iex",
            login_cmd: "claude auth login",
            login_hint: Some("瀏覽器完成授權後會自動返回，接著按 Enter 即可完成。"),
            docs_url: "https://code.claude.com/docs/en/overview",
        },
        Provider::Codex => Spec {
            tool: "OpenAI Codex CLI (codex)",
            badge: "CX",
            summary: "需安裝並用 ChatGPT 訂閱帳號登入 Codex CLI",
            install_cmd: "irm https://chatgpt.com/codex/install.ps1 | iex",
            login_cmd: "codex login",
            login_hint: Some("瀏覽器會開啟 ChatGPT 登入頁，授權完成後就會自動返回。"),
            docs_url: "https://learn.chatgpt.com/docs/codex/cli",
        },
        Provider::Copilot => Spec {
            tool: "GitHub CLI (gh)",
            badge: "CP",
            summary: "需安裝 GitHub CLI 並登入擁有 Copilot 的 GitHub 帳號",
            install_cmd: "winget install -e --id GitHub.cli",
            login_cmd: "gh auth login --hostname github.com --web",
            login_hint: Some("依畫面提示開啟瀏覽器輸入驗證碼即可完成登入。"),
            docs_url: "https://cli.github.com/",
        },
        Provider::Antigravity => Spec {
            tool: "Antigravity CLI (agy)",
            badge: "AG",
            summary: "需安裝 Antigravity CLI 並完成 Google 帳號登入",
            install_cmd: "irm https://antigravity.google/cli/install.ps1 | iex",
            login_cmd: "agy",
            login_hint: Some("首次啟動 agy 會開啟瀏覽器登入；登入完成後輸入 /exit 或按兩次 Ctrl+D 即可離開。"),
            docs_url: "https://antigravity.google/docs/getting-started",
        },
    }
}

/// 單一服務在環境設定精靈裡的狀態。
#[derive(Clone, PartialEq, Debug)]
pub enum State {
    /// 剛啟動或剛按重新檢查，尚未回報結果。
    Checking,
    /// 已安裝且已成功取得額度。
    Ready,
    /// 找不到 CLI 工具，需要先安裝再登入。
    NotInstalled,
    /// CLI 已安裝，但尚未登入或登入已過期。
    NeedsLogin,
    /// 其他錯誤（網路、無方案、已停用等）。
    Error,
}

/// 精靈畫面裡一家服務要顯示的完整狀態。
#[derive(Clone, PartialEq, Debug)]
pub struct ItemStatus {
    pub provider: Provider,
    pub state: State,
    /// 狀態標籤文字，例如「已就緒」「尚未安裝」「尚未登入」。
    pub badge: String,
    /// 狀態詳細說明。
    pub detail: String,
    /// 要展示／複製給使用者看的建議指令（Ready 時為空）。
    pub command: String,
    /// 主按鈕文字；None 表示不強調主操作。
    pub primary_label: Option<&'static str>,
    /// 主按鈕對應的操作模式（安裝＋登入，或僅登入）。
    pub mode: RunMode,
    /// 此服務的設定 PowerShell 視窗是否正在執行中。
    pub running: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RunMode {
    InstallAndLogin,
    LoginOnly,
}

/// 依查詢結果與本機 CLI 偵測，整理出精靈卡片的顯示內容。
pub fn diagnose(
    p: Provider,
    done: bool,
    quota: Option<&Quota>,
    error: Option<&FetchError>,
    cli_installed: bool,
    running: bool,
) -> ItemStatus {
    let sp = spec(p);
    if running {
        return ItemStatus {
            provider: p,
            state: State::Checking,
            badge: "設定中".into(),
            detail: "已開啟 PowerShell 視窗，完成後會自動重新檢查".into(),
            command: if cli_installed { sp.login_cmd.into() } else { sp.install_cmd.into() },
            primary_label: Some("設定中…"),
            mode: if cli_installed { RunMode::LoginOnly } else { RunMode::InstallAndLogin },
            running: true,
        };
    }

    // 有取得額度且沒有登入失效，就是已就緒。
    if let Some(q) = quota {
        if !matches!(error, Some(FetchError::LoginExpired(_))) {
            let detail = match (&q.plan, error) {
                (Some(plan), Some(FetchError::RateLimited(_))) => format!("已連線（{plan}，目前暫時限流）"),
                (None, Some(FetchError::RateLimited(_))) => "已連線（目前暫時限流）".into(),
                (Some(plan), _) => format!("已連線（{plan}）"),
                (None, _) => "已連線，正在工作列顯示額度".into(),
            };
            return ItemStatus {
                provider: p,
                state: State::Ready,
                badge: "已就緒".into(),
                detail,
                command: String::new(),
                primary_label: Some("重新登入"),
                mode: RunMode::LoginOnly,
                running: false,
            };
        }
    }

    if !done {
        return ItemStatus {
            provider: p,
            state: State::Checking,
            badge: "檢查中".into(),
            detail: "正在偵測本機環境與登入狀態…".into(),
            command: sp.summary.into(),
            primary_label: None,
            mode: if cli_installed { RunMode::LoginOnly } else { RunMode::InstallAndLogin },
            running: false,
        };
    }

    match error {
        Some(FetchError::NotSetUp(msg)) => {
            if cli_installed {
                let detail = if msg.contains("沒有 Copilot") || msg.contains("訂閱已結束") {
                    format!("{msg}（可改登入其他帳號）")
                } else {
                    format!("已安裝 {}，尚未登入帳號", sp.tool)
                };
                ItemStatus {
                    provider: p,
                    state: State::NeedsLogin,
                    badge: "尚未登入".into(),
                    detail,
                    command: sp.login_cmd.into(),
                    primary_label: Some("前往登入"),
                    mode: RunMode::LoginOnly,
                    running: false,
                }
            } else {
                ItemStatus {
                    provider: p,
                    state: State::NotInstalled,
                    badge: "尚未安裝".into(),
                    detail: sp.summary.into(),
                    command: sp.install_cmd.into(),
                    primary_label: Some("安裝並登入"),
                    mode: RunMode::InstallAndLogin,
                    running: false,
                }
            }
        }
        Some(FetchError::LoginExpired(_)) => {
            let mode = if cli_installed { RunMode::LoginOnly } else { RunMode::InstallAndLogin };
            let (label, cmd) = if cli_installed {
                ("重新登入", sp.login_cmd)
            } else {
                ("安裝並登入", sp.install_cmd)
            };
            ItemStatus {
                provider: p,
                state: State::NeedsLogin,
                badge: "登入過期".into(),
                detail: "登入憑證已過期，請重新登入以恢復顯示".into(),
                command: cmd.into(),
                primary_label: Some(label),
                mode,
                running: false,
            }
        }
        Some(FetchError::Disabled(msg)) => ItemStatus {
            provider: p,
            state: State::Error,
            badge: "已停用".into(),
            detail: msg.clone(),
            command: sp.login_cmd.into(),
            primary_label: Some(if cli_installed { "前往登入" } else { "安裝並登入" }),
            mode: if cli_installed { RunMode::LoginOnly } else { RunMode::InstallAndLogin },
            running: false,
        },
        Some(FetchError::RateLimited(msg) | FetchError::Other(msg)) => {
            let mode = if cli_installed { RunMode::LoginOnly } else { RunMode::InstallAndLogin };
            let (label, cmd) = if cli_installed {
                ("前往登入", sp.login_cmd)
            } else {
                ("安裝並登入", sp.install_cmd)
            };
            ItemStatus {
                provider: p,
                state: State::Error,
                badge: "異常".into(),
                detail: msg.clone(),
                command: cmd.into(),
                primary_label: Some(label),
                mode,
                running: false,
            }
        }
        None => ItemStatus {
            provider: p,
            state: if cli_installed { State::NeedsLogin } else { State::NotInstalled },
            badge: if cli_installed { "尚未登入".into() } else { "尚未安裝".into() },
            detail: sp.summary.into(),
            command: if cli_installed { sp.login_cmd.into() } else { sp.install_cmd.into() },
            primary_label: Some(if cli_installed { "前往登入" } else { "安裝並登入" }),
            mode: if cli_installed { RunMode::LoginOnly } else { RunMode::InstallAndLogin },
            running: false,
        },
    }
}

// ---------- 本機執行檔偵測（含登錄檔最新 PATH） ----------

fn reg_env_path(root: HKEY, subkey: &str) -> Option<String> {
    let mut len = 0u32;
    let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ;
    let r = unsafe { RegGetValueW(root, wide(subkey).as_ptr(), wide("Path").as_ptr(), flags, null_mut(), null_mut(), &mut len) };
    if r != 0 || len < 2 {
        return None;
    }
    let mut buf = vec![0u16; (len as usize + 1) / 2];
    let r = unsafe {
        RegGetValueW(root, wide(subkey).as_ptr(), wide("Path").as_ptr(), flags, null_mut(), buf.as_mut_ptr() as *mut _, &mut len)
    };
    if r != 0 {
        return None;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..end]))
}

fn split_path_list(raw: &str) -> Vec<PathBuf> {
    raw.split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.trim_matches('"'))
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect()
}

fn fresh_path_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(p) = reg_env_path(HKEY_CURRENT_USER, "Environment") {
        dirs.extend(split_path_list(&p));
    }
    if let Some(p) = reg_env_path(HKEY_LOCAL_MACHINE, r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment") {
        dirs.extend(split_path_list(&p));
    }
    if let Some(p) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&p));
    }
    dirs
}

fn env_join(var: &str, parts: &[&str]) -> Option<PathBuf> {
    let mut p = PathBuf::from(std::env::var_os(var)?);
    for seg in parts {
        p.push(seg);
    }
    Some(p)
}

/// 在指定目錄清單裡找第一個存在的執行檔（可純單元測試）。
pub fn search_in_dirs(names: &[&str], dirs: &[PathBuf]) -> Option<PathBuf> {
    for dir in dirs {
        for name in names {
            let cand = dir.join(name);
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

fn find_exe(extra_files: Vec<Option<PathBuf>>, names: &[&str]) -> Option<PathBuf> {
    for p in extra_files.into_iter().flatten() {
        if p.is_file() {
            return Some(p);
        }
    }
    search_in_dirs(names, &fresh_path_dirs())
}

pub fn find_claude() -> Option<PathBuf> {
    find_exe(
        vec![
            env_join("USERPROFILE", &[".local", "bin", "claude.exe"]),
            env_join("LOCALAPPDATA", &["Microsoft", "WinGet", "Links", "claude.exe"]),
            env_join("APPDATA", &["npm", "claude.cmd"]),
        ],
        &["claude.exe", "claude.cmd"],
    )
}

pub fn find_codex() -> Option<PathBuf> {
    find_exe(
        vec![
            env_join("LOCALAPPDATA", &["Microsoft", "WinGet", "Links", "codex.exe"]),
            env_join("USERPROFILE", &[".local", "bin", "codex.exe"]),
            env_join("APPDATA", &["npm", "codex.cmd"]),
        ],
        &["codex.exe", "codex.cmd"],
    )
}

pub fn find_gh() -> Option<PathBuf> {
    find_exe(
        vec![
            env_join("ProgramFiles", &["GitHub CLI", "gh.exe"]),
            env_join("ProgramFiles(x86)", &["GitHub CLI", "gh.exe"]),
            env_join("LOCALAPPDATA", &["Programs", "GitHub CLI", "gh.exe"]),
            env_join("LOCALAPPDATA", &["Microsoft", "WinGet", "Links", "gh.exe"]),
        ],
        &["gh.exe"],
    )
}

pub fn find_agy() -> Option<PathBuf> {
    find_exe(
        vec![
            env_join("LOCALAPPDATA", &["agy", "bin", "agy.exe"]),
            env_join("USERPROFILE", &[".local", "bin", "agy.exe"]),
        ],
        &["agy.exe"],
    )
}

pub fn cli_installed(p: Provider) -> bool {
    match p {
        Provider::Claude => find_claude().is_some(),
        Provider::Codex => find_codex().is_some(),
        Provider::Copilot => find_gh().is_some(),
        Provider::Antigravity => find_agy().is_some(),
    }
}

// ---------- PowerShell 引導腳本 ----------

/// 產生互動式 PowerShell 腳本內容：先清楚列出官方指令，使用者按 Enter 確認後才執行。
/// 裝完工具會自動從登錄檔刷新 `$env:PATH`，接著直接進入登入流程。
pub fn build_script(p: Provider, mode: RunMode) -> String {
    let sp = spec(p);
    let title = match mode {
        RunMode::InstallAndLogin => format!("AI Quota Bar - 安裝並登入 {}", p.name()),
        RunMode::LoginOnly => format!("AI Quota Bar - 登入 {}", p.name()),
    };
    let mut s = String::new();
    s.push_str("[Console]::OutputEncoding = [System.Text.Encoding]::UTF8\r\n");
    s.push_str(&format!("$Host.UI.RawUI.WindowTitle = '{}'\r\n", title));
    s.push_str("function Refresh-Path {\r\n");
    s.push_str("    $m = [Environment]::GetEnvironmentVariable('Path', 'Machine')\r\n");
    s.push_str("    $u = [Environment]::GetEnvironmentVariable('Path', 'User')\r\n");
    s.push_str("    $extra = @(\r\n");
    s.push_str("        (Join-Path $env:USERPROFILE '.local\\bin'),\r\n");
    s.push_str("        (Join-Path $env:LOCALAPPDATA 'agy\\bin'),\r\n");
    s.push_str("        (Join-Path $env:LOCALAPPDATA 'Microsoft\\WinGet\\Links'),\r\n");
    s.push_str("        (Join-Path $env:APPDATA 'npm'),\r\n");
    s.push_str("        (Join-Path $env:ProgramFiles 'GitHub CLI')\r\n");
    s.push_str("    ) | Where-Object { $_ -and (Test-Path $_) }\r\n");
    s.push_str("    $env:PATH = (($extra + @($u, $m, $env:PATH)) -join ';')\r\n");
    s.push_str("}\r\n\r\n");

    s.push_str("Write-Host '============================================================' -ForegroundColor Cyan\r\n");
    s.push_str(&format!(
        "Write-Host '  AI Quota Bar — 環境設定：{} ({})' -ForegroundColor Cyan\r\n",
        p.name(),
        sp.tool
    ));
    s.push_str("Write-Host '============================================================' -ForegroundColor Cyan\r\n");
    s.push_str("Write-Host ''\r\n");

    match mode {
        RunMode::InstallAndLogin => {
            s.push_str("Write-Host '接下來將依序執行以下官方指令：' -ForegroundColor White\r\n");
            s.push_str(&format!("Write-Host '  1. 安裝工具：{}' -ForegroundColor Yellow\r\n", sp.install_cmd));
            s.push_str(&format!("Write-Host '  2. 登入帳號：{}' -ForegroundColor Yellow\r\n", sp.login_cmd));
        }
        RunMode::LoginOnly => {
            s.push_str("Write-Host '接下來將執行以下官方登入指令：' -ForegroundColor White\r\n");
            s.push_str(&format!("Write-Host '  {}' -ForegroundColor Yellow\r\n", sp.login_cmd));
        }
    }
    s.push_str(&format!("Write-Host '官方說明：{}' -ForegroundColor DarkGray\r\n", sp.docs_url));
    if let Some(hint) = sp.login_hint {
        s.push_str(&format!("Write-Host '提示：{}' -ForegroundColor Gray\r\n", hint));
    }
    s.push_str("Write-Host ''\r\n");
    s.push_str("$null = Read-Host '按 Enter 開始執行（若要取消請直接按 Ctrl+C 或關閉視窗）'\r\n");
    s.push_str("Write-Host ''\r\n");

    if mode == RunMode::InstallAndLogin {
        s.push_str(&format!("Write-Host '[1/2] 正在安裝 {}...' -ForegroundColor Cyan\r\n", sp.tool));
        if p == Provider::Copilot {
            s.push_str("if (-not (Get-Command winget -ErrorAction SilentlyContinue)) {\r\n");
            s.push_str("    Write-Host '找不到 winget，請改由官網下載 GitHub CLI 安裝檔：https://cli.github.com/' -ForegroundColor Red\r\n");
            s.push_str("    $null = Read-Host '按 Enter 關閉視窗'\r\n");
            s.push_str("    exit 1\r\n");
            s.push_str("}\r\n");
        }
        s.push_str("try {\r\n");
        s.push_str("    $global:LASTEXITCODE = 0\r\n");
        s.push_str(&format!("    {}\r\n", sp.install_cmd));
        s.push_str("    if ($LASTEXITCODE -and $LASTEXITCODE -ne 0) {\r\n");
        s.push_str("        Write-Host \"`n安裝指令回傳錯誤碼 $LASTEXITCODE，請檢查上方訊息。\" -ForegroundColor Red\r\n");
        s.push_str("        $null = Read-Host '按 Enter 關閉視窗'\r\n");
        s.push_str("        exit $LASTEXITCODE\r\n");
        s.push_str("    }\r\n");
        s.push_str("} catch {\r\n");
        s.push_str("    Write-Host \"`n安裝失敗：$($_.Exception.Message)\" -ForegroundColor Red\r\n");
        s.push_str("    $null = Read-Host '按 Enter 關閉視窗'\r\n");
        s.push_str("    exit 1\r\n");
        s.push_str("}\r\n");
        s.push_str("Refresh-Path\r\n");
        s.push_str("Write-Host ''\r\n");
        s.push_str(&format!("Write-Host '[2/2] 正在啟動 {} 登入流程...' -ForegroundColor Cyan\r\n", p.name()));
    } else {
        s.push_str("Refresh-Path\r\n");
        s.push_str(&format!("Write-Host '正在啟動 {} 登入流程...' -ForegroundColor Cyan\r\n", p.name()));
    }

    s.push_str("try {\r\n");
    s.push_str(&format!("    {}\r\n", sp.login_cmd));
    s.push_str("} catch {\r\n");
    s.push_str("    Write-Host \"`n執行登入失敗：$($_.Exception.Message)\" -ForegroundColor Red\r\n");
    s.push_str("    Write-Host '若剛安裝完工具仍找不到指令，請關閉此視窗後重新點一次「前往登入」。' -ForegroundColor Yellow\r\n");
    s.push_str("}\r\n");

    s.push_str("Write-Host ''\r\n");
    s.push_str("Write-Host '============================================================' -ForegroundColor Green\r\n");
    s.push_str("Write-Host '  完成！關閉此視窗後，AI Quota Bar 會自動重新檢查並顯示額度。' -ForegroundColor Green\r\n");
    s.push_str("Write-Host '============================================================' -ForegroundColor Green\r\n");
    s.push_str("$null = Read-Host '按 Enter 關閉此視窗'\r\n");
    s
}

/// 寫出暫存 UTF-8 (含 BOM) `.ps1` 並以獨立可見的 PowerShell 視窗執行；
/// 視窗關閉後自動刪暫存檔並呼叫 `on_done`（在背景執行緒執行）。
pub fn launch(p: Provider, mode: RunMode, on_done: impl FnOnce() + Send + 'static) -> Result<(), String> {
    let tag = match p {
        Provider::Claude => "claude",
        Provider::Codex => "codex",
        Provider::Copilot => "copilot",
        Provider::Antigravity => "antigravity",
    };
    let script_path = std::env::temp_dir().join(format!("ai-quota-bar-setup-{tag}-{}.ps1", std::process::id()));
    let content = build_script(p, mode);
    let mut bytes = Vec::with_capacity(content.len() + 3);
    // Windows PowerShell 5.1 需要 UTF-8 BOM 才能正確讀取正體中文字串。
    bytes.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
    bytes.extend_from_slice(content.as_bytes());
    std::fs::write(&script_path, &bytes).map_err(|e| format!("無法建立設定腳本：{e}"))?;

    use std::os::windows::process::CommandExt;
    const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
    let mut child = Command::new("powershell.exe")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(&script_path)
        .creation_flags(CREATE_NEW_CONSOLE)
        .spawn()
        .map_err(|e| {
            let _ = std::fs::remove_file(&script_path);
            format!("無法開啟 PowerShell：{e}")
        })?;

    std::thread::spawn(move || {
        let _ = child.wait();
        let _ = std::fs::remove_file(&script_path);
        on_done();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Window;

    #[test]
    fn diagnoses_not_installed_vs_needs_login_vs_ready() {
        let not_installed = diagnose(
            Provider::Claude,
            true,
            None,
            Some(&FetchError::NotSetUp("找不到 Claude Code 登入資料".into())),
            false,
            false,
        );
        assert_eq!(not_installed.state, State::NotInstalled);
        assert_eq!(not_installed.primary_label, Some("安裝並登入"));
        assert_eq!(not_installed.mode, RunMode::InstallAndLogin);
        assert!(not_installed.command.contains("claude.ai/install.ps1"));

        let needs_login = diagnose(
            Provider::Claude,
            true,
            None,
            Some(&FetchError::NotSetUp("找不到 Claude Code 登入資料".into())),
            true,
            false,
        );
        assert_eq!(needs_login.state, State::NeedsLogin);
        assert_eq!(needs_login.primary_label, Some("前往登入"));
        assert_eq!(needs_login.mode, RunMode::LoginOnly);
        assert_eq!(needs_login.command, "claude auth login");

        let q = Quota {
            plan: Some("pro".into()),
            windows: vec![Window {
                short: "5H".into(),
                name: "5 小時".into(),
                used: 20.0,
                resets_at: None,
                detail: None,
                period: None,
            }],
            extra: vec![],
        };
        let ready = diagnose(Provider::Claude, true, Some(&q), None, true, false);
        assert_eq!(ready.state, State::Ready);
        assert!(ready.detail.contains("pro"));

        // 有舊額度但憑證過期時，仍提示重新登入。
        let expired = diagnose(
            Provider::Claude,
            true,
            Some(&q),
            Some(&FetchError::LoginExpired("過期".into())),
            true,
            false,
        );
        assert_eq!(expired.state, State::NeedsLogin);
        assert_eq!(expired.badge, "登入過期");
        assert_eq!(expired.primary_label, Some("重新登入"));
    }

    #[test]
    fn script_lists_commands_and_waits_for_confirmation() {
        for p in crate::providers::ALL {
            let sp = spec(p);
            let s1 = build_script(p, RunMode::InstallAndLogin);
            assert!(s1.contains(sp.install_cmd), "{} 腳本應包含安裝指令", p.name());
            assert!(s1.contains(sp.login_cmd), "{} 腳本應包含登入指令", p.name());
            assert!(s1.contains("Read-Host '按 Enter 開始執行"), "執行前必須等待使用者確認");
            assert!(s1.contains("Refresh-Path"), "安裝後必須刷新 PATH");

            let s2 = build_script(p, RunMode::LoginOnly);
            assert!(!s2.contains("[1/2]"), "僅登入模式不應執行安裝步驟");
            assert!(s2.contains(sp.login_cmd));
            assert!(s2.contains("Read-Host '按 Enter 開始執行"));
        }
    }

    #[test]
    fn searches_executable_in_dirs() {
        let dir = std::env::temp_dir().join(format!("aiquota-setup-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("gh.exe");
        std::fs::write(&fake, b"").unwrap();
        assert_eq!(search_in_dirs(&["gh.exe"], &[dir.clone()]), Some(fake));
        assert_eq!(search_in_dirs(&["missing.exe"], &[dir.clone()]), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn splits_registry_path_safely() {
        let dirs = split_path_list(r#" C:\Tools\bin ; "C:\Program Files\GitHub CLI" ; ; "#);
        assert_eq!(dirs, vec![PathBuf::from(r"C:\Tools\bin"), PathBuf::from(r"C:\Program Files\GitHub CLI")]);
    }
}
