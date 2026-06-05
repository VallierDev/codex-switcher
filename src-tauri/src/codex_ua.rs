//! 构造与官方 codex CLI 完全同形态的 `originator` / `User-Agent`。
//!
//! 背景：我们几个**自建**的、直连 OpenAI / ChatGPT 的请求（quota 查询、OAuth
//! token 兑换 / 刷新）必须看起来像真 codex，否则就是指纹。主代理转发链路不需要
//! 这里——它逐字透传 codex 自己的 header（见 proxy.rs `build_upstream_headers`）。
//!
//! 官方实现见 `codex-rs/login/src/auth/default_client.rs` 的 `get_codex_user_agent()`：
//!   `{originator}/{version} ({os_type} {os_version}; {arch}) {terminal}`
//! 例：`codex_cli_rs/0.137.0 (Mac OS 15.5; arm64) iTerm.app`
//! - version  = codex 自己的 CARGO_PKG_VERSION（这里改成探测本机安装的 codex 版本）
//! - os_type  = os_info::os_type()，macOS 上是 "Mac OS"
//! - arch     = os_info::architecture()，Apple Silicon 是 "arm64"（不是 std 的 aarch64）
//! - terminal = codex_terminal_detection::user_agent()，终端程序名，缺省 "unknown"

use std::process::Command;
use std::sync::OnceLock;

/// 官方默认 originator（codex-rs `DEFAULT_ORIGINATOR`）。
pub const CODEX_ORIGINATOR: &str = "codex_cli_rs";

/// 探测不到本机 codex 版本时的兜底（取最近一次已知的官方发布版）。
const DEFAULT_CODEX_VERSION: &str = "0.137.0";

/// 完整 User-Agent，进程内只算一次。
pub fn codex_user_agent() -> &'static str {
    static UA: OnceLock<String> = OnceLock::new();
    UA.get_or_init(|| {
        let raw = format!(
            "{}/{} ({} {}; {}) {}",
            CODEX_ORIGINATOR,
            codex_version(),
            os_type(),
            os_version(),
            arch(),
            terminal_token(),
        );
        sanitize_header(raw)
    })
    .as_str()
}

/// 本机安装的 codex 版本：先 `codex --version`，失败回退常量。
fn codex_version() -> String {
    static VER: OnceLock<String> = OnceLock::new();
    VER.get_or_init(|| detect_codex_version().unwrap_or_else(|| DEFAULT_CODEX_VERSION.to_string()))
        .clone()
}

fn detect_codex_version() -> Option<String> {
    // GUI（.app）进程 PATH 很窄，手动补常见安装目录后再找 codex。
    let mut path = std::env::var("PATH").unwrap_or_default();
    for extra in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"] {
        path.push(':');
        path.push_str(extra);
    }
    if let Ok(home) = std::env::var("HOME") {
        for sub in [".local/bin", ".bun/bin", ".npm-global/bin", ".volta/bin"] {
            path.push(':');
            path.push_str(&format!("{home}/{sub}"));
        }
    }
    let out = Command::new("codex")
        .arg("--version")
        .env("PATH", path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // 输出形如 "codex-cli 0.137.0" / "codex 0.137.0"；抓第一个 x.y.z。
    parse_semver(&String::from_utf8_lossy(&out.stdout))
}

fn parse_semver(s: &str) -> Option<String> {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"\d+\.\d+\.\d+[A-Za-z0-9.\-+]*").unwrap());
    re.find(s).map(|m| m.as_str().to_string())
}

fn os_type() -> &'static str {
    // 对齐 os_info::os_type() 的取值。
    if cfg!(target_os = "macos") {
        "Mac OS"
    } else if cfg!(target_os = "windows") {
        "Windows"
    } else {
        "Linux"
    }
}

fn os_version() -> String {
    static V: OnceLock<String> = OnceLock::new();
    V.get_or_init(|| detect_os_version().unwrap_or_else(|| "unknown".to_string()))
        .clone()
}

#[cfg(target_os = "macos")]
fn detect_os_version() -> Option<String> {
    let out = Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!v.is_empty()).then_some(v)
}

#[cfg(not(target_os = "macos"))]
fn detect_os_version() -> Option<String> {
    None
}

fn arch() -> &'static str {
    // os_info 的命名：Apple Silicon = "arm64"（std 的 ARCH 是 "aarch64"）。
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        other => other,
    }
}

/// 复刻 `codex_terminal_detection::user_agent()` 的主路径（覆盖常见终端，
/// 兜底 "unknown"）。GUI 进程一般无 TERM_PROGRAM → 返回 "unknown"，与真
/// codex 在非终端上下文一致。
fn terminal_token() -> String {
    if let Ok(tp) = std::env::var("TERM_PROGRAM") {
        let tp = tp.trim();
        if !tp.is_empty() && !tp.eq_ignore_ascii_case("tmux") {
            return match std::env::var("TERM_PROGRAM_VERSION") {
                Ok(v) if !v.trim().is_empty() => format!("{tp}/{}", v.trim()),
                _ => tp.to_string(),
            };
        }
    }
    if std::env::var("WEZTERM_VERSION").is_ok() {
        return "WezTerm".to_string();
    }
    if std::env::var("ITERM_SESSION_ID").is_ok() || std::env::var("ITERM_PROFILE").is_ok() {
        return "iTerm.app".to_string();
    }
    if std::env::var("KITTY_WINDOW_ID").is_ok() {
        return "kitty".to_string();
    }
    if std::env::var("ALACRITTY_SOCKET").is_ok() {
        return "Alacritty".to_string();
    }
    "unknown".to_string()
}

/// 把非可打印 ASCII 替换成 '_'，保证能塞进 HeaderValue（对齐 codex 的 sanitize）。
fn sanitize_header(s: String) -> String {
    if s.bytes().all(|b| (b' '..=b'~').contains(&b)) {
        s
    } else {
        s.chars()
            .map(|c| if (' '..='~').contains(&c) { c } else { '_' })
            .collect()
    }
}
