//! Codex Switcher - 账号管理模块
//!
//! 处理多个 Codex 账号的存储、切换和管理
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 应用全局设置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSettings {
    /// 是否在切换账号后自动重载 IDE
    #[serde(default)]
    pub auto_reload_ide: bool,

    /// 主力 IDE: "Windsurf" | "Antigravity" | "Cursor" | "VSCode"
    #[serde(default = "default_primary_ide")]
    pub primary_ide: String,

    /// 是否使用杀进程方式重启（Windsurf 推荐）
    #[serde(default)]
    pub use_pkill_restart: bool,

    /// 后台自动刷新 Token
    #[serde(default = "default_false")]
    pub background_refresh: bool,

    /// 刷新间隔（分钟）
    #[serde(default = "default_refresh_interval")]
    pub refresh_interval_minutes: u32,

    /// 非活跃账号在距离失效前多少天开始保活刷新
    #[serde(default = "default_inactive_refresh_days")]
    pub inactive_refresh_days: u32,

    /// 界面配色方案
    #[serde(default = "default_theme_palette")]
    pub theme_palette: String,

    /// 是否允许智能切号自动切换到免费账号
    #[serde(default = "default_false")]
    pub allow_auto_switch_to_free: bool,

    /// 是否启用本地代理服务器
    #[serde(default = "default_false")]
    pub proxy_enabled: bool,

    /// 代理服务器端口
    #[serde(default = "default_proxy_port")]
    pub proxy_port: u16,

    /// 允许局域网设备访问代理
    #[serde(default)]
    pub proxy_allow_lan: bool,

    /// 5h 配额预防性切号阈值（0=仅429触发，10=剩余<10%时切）
    #[serde(default)]
    pub proxy_threshold_5h: u8,

    /// 周配额预防性切号阈值（0=仅429触发，5=剩余<5%时切）
    #[serde(default)]
    pub proxy_threshold_weekly: u8,

    /// Free 账号保护线（0=不特殊处理，35=剩余<35%时切）
    #[serde(default)]
    pub proxy_free_guard: u8,

    /// 切号时发送 macOS 系统通知
    #[serde(default)]
    pub notify_on_switch: bool,

    /// 切号模式：auto（代理开=热切，代理关=冷切）/ cold（强制冷切）
    /// 热切 = 只改 store.current + 失效代理缓存，不写 ~/.codex/auth.json
    #[serde(default = "default_switch_mode")]
    pub switch_mode: String,

    /// 切号时注入消息到 Codex 对话（实验性）
    #[serde(default)]
    pub inject_switch_message: bool,

    /// 定时刷新账号额度
    #[serde(default)]
    pub quota_refresh_enabled: bool,

    /// 每个账号刷新间隔（分钟）
    #[serde(default = "default_quota_refresh_interval")]
    pub quota_refresh_interval: u32,

    /// 每轮刷新几个账号
    #[serde(default = "default_quota_refresh_batch")]
    pub quota_refresh_batch: u32,

    // ===== Remote Mode（private-lan 功能，LAN 代理 + token 中心化）=====
    /// 远程模式：off / server / client
    #[serde(default = "default_remote_mode")]
    pub remote_mode: String,

    /// server 模式下 HTTP API 绑定端口
    #[serde(default = "default_remote_server_port")]
    pub remote_server_port: u16,

    /// server 模式下 HTTP API 绑定地址 (e.g. "0.0.0.0")
    #[serde(default = "default_remote_server_bind")]
    pub remote_server_bind: String,

    /// client 模式下 Server 地址 (e.g. "http://192.168.2.14:18081")
    #[serde(default)]
    pub remote_server_url: String,

    /// client 模式下的回退地址（primary 不通时尝试），一般放 ZeroTier URL
    #[serde(default)]
    pub remote_server_url_fallback: String,

    /// 两端共用的认证密钥（X-Auth-Token 头）
    #[serde(default)]
    pub remote_shared_secret: String,

    /// client 模式下，同步到 Server 时要跳过的 skill 目录名
    #[serde(default)]
    pub skills_sync_blacklist: Vec<String>,

    /// solo 模式：心跳时自动把本机 current 对齐到 Server 的 current
    /// 关掉后允许两端 current 不一致；但手工一键同号仍可用。
    #[serde(default = "default_true")]
    pub solo_auto_sync_current: bool,

    /// SSE bootstrap 的缓冲字节上限（拦截 mid-stream 限额错误的窗口大小）。
    /// 正常请求几 KB 就过窗，配大点不会有副作用，反而能在慢启动模型上有更多嗅探机会。
    #[serde(default = "default_bootstrap_byte_cap")]
    pub proxy_bootstrap_byte_cap: usize,

    /// SSE bootstrap 的时间上限（毫秒）。配合 SSE keep-alive 心跳可以放心拉大。
    #[serde(default = "default_bootstrap_time_cap_ms")]
    pub proxy_bootstrap_time_cap_ms: u64,

    /// Relay 账号"切回来"：current 是 Relay 时遇到 401/429/quota 是否允许自动切到其它（订阅）号
    /// 默认 true —— Relay 出问题别卡死，可以救回订阅号
    #[serde(default = "default_true")]
    pub relay_auto_switch_out: bool,

    /// "切到 Relay"：自动选号 / 切号 / affinity 是否允许选中 Relay 作为目标
    /// 默认 false —— 用订阅号时不会偷偷把请求路由到 Relay 扣余额
    #[serde(default = "default_false")]
    pub relay_auto_switch_in: bool,

    /// 是否压缩 shell tool（exec_command / Bash / write_stdin 等）的输出，
    /// 在 proxy 把上游响应转发回客户端前做截断，省 token。
    #[serde(default = "default_true")]
    pub output_compression_enabled: bool,

    /// 输出压缩触发阈值（字节）。低于该值的 tool 输出原样透传。
    #[serde(default = "default_compress_threshold")]
    pub output_compression_threshold_bytes: usize,
}

fn default_bootstrap_byte_cap() -> usize {
    32 * 1024
}

fn default_compress_threshold() -> usize {
    8192
}

fn default_bootstrap_time_cap_ms() -> u64 {
    8000
}

fn default_theme_palette() -> String {
    "midnight".to_string()
}

fn default_primary_ide() -> String {
    "Windsurf".to_string()
}

fn default_refresh_interval() -> u32 {
    30
}

fn default_inactive_refresh_days() -> u32 {
    7
}

fn default_false() -> bool {
    false
}

fn default_true() -> bool {
    true
}

fn default_proxy_port() -> u16 {
    18080
}

fn default_quota_refresh_interval() -> u32 {
    5
}

fn default_quota_refresh_batch() -> u32 {
    1
}

fn default_remote_mode() -> String {
    "off".to_string()
}

fn default_switch_mode() -> String {
    "auto".to_string()
}

/// 决定本次切号是否使用热切：
/// - switch_mode="cold" 永远冷切
/// - switch_mode="auto"（默认）代理开=热切；代理关=冷切（热切此时没意义）
pub fn should_hot_switch(settings: &AppSettings, proxy_running: bool) -> bool {
    match settings.switch_mode.as_str() {
        "cold" => false,
        _ => proxy_running,
    }
}

/// remote_mode="client"：本机不持 token，读/切全走 Server
pub fn is_remote_client(mode: &str) -> bool {
    mode == "client"
}

/// remote_mode="solo"：本机自治但把 refresh/switch push 给 Server 做归档
pub fn is_remote_solo(mode: &str) -> bool {
    mode == "solo"
}

/// 需要把本机账号变更推给 Server 的模式（client 登录新号时也要推；solo 每次都推）
pub fn pushes_to_server(mode: &str) -> bool {
    matches!(mode, "client" | "solo")
}

/// solo 模式心跳间隔（秒）
pub const SOLO_HEARTBEAT_INTERVAL_SECS: u64 = 120;
/// solo 模式心跳在 Server 侧的 TTL（秒）。Server 超过这个时间没收到心跳 → 恢复保活
pub const SOLO_HEARTBEAT_TTL_SECS: i64 = 300;

fn default_remote_server_port() -> u16 {
    18081
}

fn default_remote_server_bind() -> String {
    "0.0.0.0".to_string()
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            auto_reload_ide: false,
            primary_ide: default_primary_ide(),
            use_pkill_restart: false,
            background_refresh: false,
            refresh_interval_minutes: default_refresh_interval(),
            inactive_refresh_days: default_inactive_refresh_days(),
            theme_palette: default_theme_palette(),
            allow_auto_switch_to_free: false,
            proxy_enabled: false,
            proxy_port: default_proxy_port(),
            proxy_allow_lan: false,
            proxy_threshold_5h: 0,
            proxy_threshold_weekly: 0,
            proxy_free_guard: 0,
            notify_on_switch: false,
            inject_switch_message: false,
            switch_mode: default_switch_mode(),
            quota_refresh_enabled: false,
            quota_refresh_interval: default_quota_refresh_interval(),
            quota_refresh_batch: default_quota_refresh_batch(),
            remote_mode: default_remote_mode(),
            remote_server_port: default_remote_server_port(),
            remote_server_bind: default_remote_server_bind(),
            remote_server_url: String::new(),
            remote_server_url_fallback: String::new(),
            remote_shared_secret: String::new(),
            skills_sync_blacklist: Vec::new(),
            solo_auto_sync_current: true,
            proxy_bootstrap_byte_cap: default_bootstrap_byte_cap(),
            proxy_bootstrap_time_cap_ms: default_bootstrap_time_cap_ms(),
            relay_auto_switch_out: true,
            relay_auto_switch_in: false,
            output_compression_enabled: true,
            output_compression_threshold_bytes: default_compress_threshold(),
        }
    }
}

/// 账号类型
///
/// `Legacy` = 旧 store 里没显式标注的账号；运行时按 `auth_json` 里的 token 前缀派生
/// （`eyJ...` JWT → ChatgptOauth；其它 → OpenaiKey）。新建账号必须显式给 kind。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountKind {
    /// 旧账号未标注，运行时派生
    Legacy,
    /// ChatGPT 订阅 OAuth（access_token JWT）
    ChatgptOauth,
    /// 官方 OpenAI API key（sk-...，上游 api.openai.com）
    OpenaiKey,
    /// 第三方中转站（sk-...，上游 = relay_base_url）
    Relay,
}

impl Default for AccountKind {
    fn default() -> Self {
        Self::Legacy
    }
}

/// 单个账号信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    /// 唯一标识符
    pub id: String,
    /// 账号名称（用户自定义）
    pub name: String,
    /// auth.json 内容
    pub auth_json: serde_json::Value,
    /// OpenAI refresh_token (用于生成新的 auth_json)
    pub refresh_token: Option<String>,
    /// 创建时间
    pub created_at: DateTime<Utc>,
    /// 上次使用时间
    pub last_used: Option<DateTime<Utc>>,
    /// 备注
    pub notes: Option<String>,
    /// 缓存的配额信息
    #[serde(default)]
    pub cached_quota: Option<CachedQuota>,

    /// 非活跃账号保活状态
    #[serde(default)]
    pub keepalive: KeepaliveState,

    /// 该账号是否已被 OpenAI 封禁
    #[serde(default)]
    pub is_banned: bool,

    /// 该账号授权是否已失效（需重新登录）
    #[serde(default)]
    pub is_token_invalid: bool,

    /// 该账号是否已登出
    #[serde(default)]
    pub is_logged_out: bool,

    /// 账号类型；默认 `Legacy` 由 `effective_kind()` 按 token 派生（向后兼容旧 store）
    #[serde(default)]
    pub kind: AccountKind,

    /// 中转站基址，仅 `Relay` 类型用，例 `"https://unity2.ai"`（不带尾斜杠）
    #[serde(default)]
    pub relay_base_url: Option<String>,

    /// 中转站主页 URL（展示/打开用，可选）
    #[serde(default)]
    pub relay_homepage: Option<String>,

    /// usage 拉取策略 preset 名（"openai_compat" 等内置 fetcher 名），None=不拉
    #[serde(default)]
    pub relay_usage_preset: Option<String>,

    /// 中转站余额缓存
    #[serde(default)]
    pub relay_usage_cache: Option<RelayUsageCache>,

    /// 模型名映射：客户端发的 model（如 `gpt-5.5`）→ 中转站实际 model（如 `glm-5.1`）。
    /// 仅 Relay 类型生效；空映射 = 透传不替换。
    #[serde(default)]
    pub relay_model_map: Option<std::collections::HashMap<String, String>>,

    /// 模型映射兜底：当 `relay_model_map` 不命中时统一替换成此值；None=透传。
    #[serde(default)]
    pub relay_model_fallback: Option<String>,
}

impl Account {
    /// 解析有效 kind：`Legacy` 时按 token 前缀派生
    pub fn effective_kind(&self) -> AccountKind {
        match self.kind {
            AccountKind::Legacy => match AccountStore::extract_access_token(&self.auth_json) {
                Some(tok) if tok.starts_with("eyJ") => AccountKind::ChatgptOauth,
                Some(_) => AccountKind::OpenaiKey,
                None => AccountKind::OpenaiKey,
            },
            other => other,
        }
    }

    /// 是否走 ChatGPT 订阅那条路径（chatgpt.com/backend-api/codex）
    pub fn is_chatgpt_oauth(&self) -> bool {
        self.effective_kind() == AccountKind::ChatgptOauth
    }

    /// 是否中转站账号
    pub fn is_relay(&self) -> bool {
        self.effective_kind() == AccountKind::Relay
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeepaliveState {
    /// 是否允许调度器为该账号执行“非活跃保活刷新”
    #[serde(default = "default_true")]
    pub inactive_refresh_enabled: bool,
    /// 最近一次保活尝试时间
    #[serde(default)]
    pub last_attempt_at: Option<DateTime<Utc>>,
    /// 最近一次保活成功时间
    #[serde(default)]
    pub last_success_at: Option<DateTime<Utc>>,
    /// 最近一次保活错误
    #[serde(default)]
    pub last_error: Option<String>,
}

impl Default for KeepaliveState {
    fn default() -> Self {
        Self {
            inactive_refresh_enabled: true,
            last_attempt_at: None,
            last_success_at: None,
            last_error: None,
        }
    }
}

/// 中转站账号的余额缓存（与 `CachedQuota` 平行；语义上一个是 USD 余额，一个是 5h+周窗口）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayUsageCache {
    /// 剩余额度（原始数值；单位看 `unit`）
    pub remaining: f64,
    /// 单位字符串（"USD" / "CNY" / "USDcent" / "tokens" 等，由上游决定）
    pub unit: String,
    /// 上游报告的账号是否仍然可用
    pub is_active: bool,
    /// 下次重置时间（Unix 秒；GLM 端是 nextResetTime/1000；None=无重置概念）
    #[serde(default)]
    pub next_reset_at: Option<i64>,
    /// 抓取时刻
    pub updated_at: DateTime<Utc>,
}

/// 缓存的配额信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedQuota {
    pub five_hour_left: f64,
    pub five_hour_reset: String,
    pub five_hour_reset_at: Option<i64>,
    #[serde(default = "default_five_hour_label")]
    pub five_hour_label: String,
    pub weekly_left: f64,
    pub weekly_reset: String,
    pub weekly_reset_at: Option<i64>,
    #[serde(default = "default_weekly_label")]
    pub weekly_label: String,
    pub plan_type: String,
    #[serde(default = "default_true")]
    pub is_valid_for_cli: bool,
    pub updated_at: DateTime<Utc>,
}

fn default_five_hour_label() -> String {
    "5H 限额".to_string()
}

fn default_weekly_label() -> String {
    "周限额".to_string()
}

/// 账号存储结构
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AccountStore {
    /// 所有账号
    pub accounts: HashMap<String, Account>,
    /// 当前激活的账号 ID
    pub current: Option<String>,
    /// 版本号（用于迁移）
    pub version: u32,
    /// 全局设置
    #[serde(default)]
    pub settings: AppSettings,
}

#[cfg(unix)]
fn ensure_private_file_permissions(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let perms = fs::Permissions::from_mode(0o600);
    fs::set_permissions(path, perms).map_err(|e| format!("设置文件权限失败: {}", e))
}

#[cfg(not(unix))]
fn ensure_private_file_permissions(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(unix)]
fn ensure_private_dir_permissions(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let perms = fs::Permissions::from_mode(0o700);
    fs::set_permissions(path, perms).map_err(|e| format!("设置目录权限失败: {}", e))
}

#[cfg(not(unix))]
fn ensure_private_dir_permissions(_path: &Path) -> Result<(), String> {
    Ok(())
}

fn write_text_secure(path: &Path, content: &str) -> Result<(), String> {
    fs::write(path, content).map_err(|e| format!("写入文件失败: {}", e))?;
    ensure_private_file_permissions(path)?;
    Ok(())
}

impl AccountStore {
    /// 配置文件路径
    pub fn config_path() -> PathBuf {
        dirs::home_dir()
            .expect("无法获取用户目录")
            .join(".codex-switcher")
            .join("accounts.json")
    }

    /// Codex auth.json 路径
    pub fn codex_auth_path() -> PathBuf {
        dirs::home_dir()
            .expect("无法获取用户目录")
            .join(".codex")
            .join("auth.json")
    }

    /// 加载账号存储
    pub fn load() -> Self {
        let path = Self::config_path();
        let mut store = if path.exists() {
            let content = fs::read_to_string(&path).unwrap_or_default();
            match serde_json::from_str::<Self>(&content) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[AccountStore] 关键错误：无法解析 accounts.json ({}). \n内容可能损坏，为保护数据已回退到内存状态。错误内容：{}", path.display(), e);
                    Self::default()
                }
            }
        } else {
            Self::default()
        };

        if store.backfill_refresh_tokens() {
            let _ = store.save();
        }
        if store.migrate_glm_usage_preset() {
            let _ = store.save();
        }

        store
    }

    /// 一次性迁移：把已导入的 GLM 账号（base_url 含 `bigmodel.cn`）的
    /// `relay_usage_preset` 从 `openai_compat` 改成 `glm_zhipu`，并补上 model 映射兜底
    /// （codex 端发的 gpt-* 模型 GLM 不认识，需要替换成 glm-* 系列）。
    fn migrate_glm_usage_preset(&mut self) -> bool {
        let mut changed = false;
        for acc in self.accounts.values_mut() {
            if !matches!(acc.kind, AccountKind::Relay) {
                continue;
            }
            let base = acc.relay_base_url.as_deref().unwrap_or("");
            if !base.contains("bigmodel.cn") {
                continue;
            }
            if acc.relay_usage_preset.as_deref() == Some("openai_compat") {
                acc.relay_usage_preset = Some("glm_zhipu".to_string());
                acc.relay_usage_cache = None; // 清掉旧错值
                changed = true;
                println!(
                    "[Migration] GLM 账号 {} usage_preset: openai_compat → glm_zhipu",
                    acc.name
                );
            }
            // 补默认模型映射：旧版本没这个字段，导致 codex 发 gpt-5.5 → GLM 直接 404
            if acc.relay_model_fallback.is_none() && acc.relay_model_map.is_none() {
                acc.relay_model_fallback = Some("glm-5.1".to_string());
                changed = true;
                println!(
                    "[Migration] GLM 账号 {} 补默认 model_fallback=glm-5.1",
                    acc.name
                );
            }
        }
        changed
    }

    /// 保存账号存储
    pub fn save(&self) -> Result<(), String> {
        let path = Self::config_path();

        // 确保目录存在
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("创建目录失败: {}", e))?;
            ensure_private_dir_permissions(parent)?;
        }

        let content =
            serde_json::to_string_pretty(self).map_err(|e| format!("序列化失败: {}", e))?;

        write_text_secure(&path, &content)?;

        Ok(())
    }

    /// 读取当前 Codex auth.json
    pub fn read_codex_auth() -> Result<serde_json::Value, String> {
        let path = Self::codex_auth_path();
        if !path.exists() {
            return Err("未找到 Codex auth.json，请先登录 Codex".to_string());
        }

        let content =
            fs::read_to_string(&path).map_err(|e| format!("读取 auth.json 失败: {}", e))?;

        serde_json::from_str(&content).map_err(|e| format!("解析 auth.json 失败: {}", e))
    }

    /// 写入 Codex auth.json
    /// 写 auth.json，但把 tokens.expires_at 字段顶到 24 小时后，让 codex CLI 永远看到"还很新鲜"，
    /// 不主动触发本地 refresh —— 真正的 token 过期由 proxy 接管处理。
    /// 适合 client 模式（Server 是 RT 轮换的唯一权威，本机 codex 自己 refresh 必撞）。
    pub fn write_codex_auth_extended_expiry(auth: &serde_json::Value) -> Result<(), String> {
        let mut patched = auth.clone();
        let new_exp = chrono::Utc::now() + chrono::Duration::hours(24);
        if let Some(tokens) = patched.get_mut("tokens") {
            if let Some(obj) = tokens.as_object_mut() {
                obj.insert(
                    "expires_at".to_string(),
                    serde_json::Value::String(new_exp.to_rfc3339()),
                );
            }
        }
        Self::write_codex_auth(&patched)
    }

    pub fn write_codex_auth(auth: &serde_json::Value) -> Result<(), String> {
        let path = Self::codex_auth_path();
        println!("写入 auth.json 到路径: {:?}", path);

        // 确保目录存在
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("创建目录失败: {}", e))?;
            ensure_private_dir_permissions(parent)?;
        }

        let content =
            serde_json::to_string_pretty(auth).map_err(|e| format!("序列化失败: {}", e))?;

        // 原子写入：先写临时文件，再重命名
        let tmp_path = path.with_extension("tmp");
        write_text_secure(&tmp_path, &content).map_err(|e| format!("写入临时文件失败: {}", e))?;

        fs::rename(&tmp_path, &path)
            .map_err(|e| format!("重命名文件失败 (Atomic Write): {}", e))?;
        ensure_private_file_permissions(&path)?;

        Ok(())
    }

    /// 添加新账号
    pub fn add_account(
        &mut self,
        name: String,
        auth_json: serde_json::Value,
        notes: Option<String>,
    ) -> Account {
        let id = uuid::Uuid::new_v4().to_string();
        let refresh_token = Self::extract_refresh_token(&auth_json);
        let account = Account {
            id: id.clone(),
            name,
            auth_json,
            refresh_token, // 从 auth_json 尝试提取
            created_at: Utc::now(),
            last_used: None,
            notes,
            cached_quota: None,
            keepalive: KeepaliveState::default(),
            is_banned: false,
            is_token_invalid: false,
            is_logged_out: false,
            kind: AccountKind::Legacy,
            relay_base_url: None,
            relay_homepage: None,
            relay_usage_preset: None,
            relay_usage_cache: None,
            relay_model_map: None,
            relay_model_fallback: None,
        };

        self.accounts.insert(id.clone(), account.clone());

        // 如果是第一个账号，设为当前
        if self.current.is_none() {
            self.current = Some(id);
        }

        account
    }

    /// 添加中转站账号（Relay 类型）。
    ///
    /// 不同于 OAuth/官方 API key：sk- 永久有效、不可 refresh、上游打 base_url。
    pub fn add_relay_account(
        &mut self,
        name: String,
        base_url: String,
        api_key: String,
        homepage: Option<String>,
        usage_preset: Option<String>,
        notes: Option<String>,
        model_map: Option<std::collections::HashMap<String, String>>,
        model_fallback: Option<String>,
    ) -> Account {
        let id = uuid::Uuid::new_v4().to_string();
        let normalized_base = base_url.trim().trim_end_matches('/').to_string();
        let auth_json = serde_json::json!({
            "tokens": {
                "access_token": api_key,
                // account_id 仅做内部唯一性占位，UI 显示用 name
                "account_id": format!("relay:{}", id),
            },
            "last_refresh": Utc::now().to_rfc3339(),
        });

        let account = Account {
            id: id.clone(),
            name,
            auth_json,
            refresh_token: None,
            created_at: Utc::now(),
            last_used: None,
            notes,
            cached_quota: None,
            keepalive: KeepaliveState::default(),
            is_banned: false,
            is_token_invalid: false,
            is_logged_out: false,
            kind: AccountKind::Relay,
            relay_base_url: Some(normalized_base),
            relay_homepage: homepage,
            relay_usage_preset: usage_preset,
            relay_usage_cache: None,
            relay_model_map: model_map,
            relay_model_fallback: model_fallback,
        };

        self.accounts.insert(id.clone(), account.clone());
        if self.current.is_none() {
            self.current = Some(id);
        }
        account
    }

    /// 切换到指定账号
    /// 切号：改 store.current + 写 ~/.codex/auth.json。
    ///
    /// 历史上 `hot` 参数控制"是否跳过写 auth.json"——目的是代理在跑时省一次 IO。
    /// 但实测发现：hot 模式下虽然 proxy 注入新号 token 让 codex 拿到 200，但
    /// **disk auth.json 没同步会让 codex 端的某些状态（IDE 显示、UnauthorizedRecovery
    /// 触发时的校验、以及"账号同步状态"UI 提示）感到不一致**，用户要手动"继续"
    /// codex 才肯往下跑——这违背了 hot 的初衷。
    ///
    /// 现在 always 写 disk：写盘几毫秒 IO 几乎免费，但能保证 store ↔ disk 永远一致。
    /// `hot` 参数保留但不再影响行为，避免改太多调用点。
    pub fn switch_to(&mut self, id: &str, _hot_legacy: bool) -> Result<(), String> {
        let account = self
            .accounts
            .get_mut(id)
            .ok_or_else(|| format!("账号不存在: {}", id))?;

        account.last_used = Some(Utc::now());

        println!("正在切换账号: {}", id);
        Self::write_codex_auth(&account.auth_json)?;
        println!("账号切换成功: auth.json 已更新");

        self.current = Some(id.to_string());
        Ok(())
    }

    /// 删除账号
    pub fn delete_account(&mut self, id: &str) -> Result<(), String> {
        if !self.accounts.contains_key(id) {
            return Err(format!("账号不存在: {}", id));
        }

        self.accounts.remove(id);

        // 如果删除的是当前账号，清空 current
        if self.current.as_deref() == Some(id) {
            self.current = self.accounts.keys().next().cloned();
        }

        Ok(())
    }

    /// 更新账号信息
    pub fn update_account(
        &mut self,
        id: &str,
        name: Option<String>,
        notes: Option<String>,
    ) -> Result<(), String> {
        let account = self
            .accounts
            .get_mut(id)
            .ok_or_else(|| format!("账号不存在: {}", id))?;

        if let Some(n) = name {
            account.name = n;
        }
        if notes.is_some() {
            account.notes = notes;
        }

        Ok(())
    }

    /// 设置某账号是否允许“非活跃保活刷新”
    pub fn set_inactive_refresh_enabled(&mut self, id: &str, enabled: bool) -> Result<(), String> {
        let account = self
            .accounts
            .get_mut(id)
            .ok_or_else(|| format!("账号不存在: {}", id))?;
        account.keepalive.inactive_refresh_enabled = enabled;
        Ok(())
    }

    /// 获取所有账号列表
    pub fn list_accounts(&self) -> Vec<&Account> {
        let mut accounts: Vec<_> = self.accounts.values().collect();
        accounts.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        accounts
    }

    /// 导出配置
    pub fn export(&self) -> Result<String, String> {
        serde_json::to_string_pretty(self).map_err(|e| format!("导出失败: {}", e))
    }

    /// 导入配置
    pub fn import(json: &str) -> Result<Self, String> {
        let mut store: Self = serde_json::from_str(json).map_err(|e| format!("导入失败: {}", e))?;
        store.backfill_refresh_tokens();
        Ok(store)
    }
    /// 从 auth_json 中提取 refresh_token（兼容 tokens.refresh_token 或根级 refresh_token）
    pub fn extract_refresh_token(auth_json: &Value) -> Option<String> {
        auth_json
            .get("tokens")
            .and_then(|t| t.get("refresh_token"))
            .or_else(|| auth_json.get("refresh_token"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
    }

    /// 从 auth_json 中提取 access_token
    pub fn extract_access_token(auth_json: &Value) -> Option<String> {
        // 优先从 tokens 对象取
        let from_tokens = auth_json.get("tokens").and_then(|t| {
            // tokens 可能是对象或字符串（历史数据兼容）
            if t.is_object() {
                t.get("access_token").and_then(|v| v.as_str())
            } else if let Some(s) = t.as_str() {
                // tokens 被存为 Python repr 字符串，尝试提取
                extract_token_from_str(s, "access_token")
            } else {
                None
            }
        });

        from_tokens
            .or_else(|| auth_json.get("access_token").and_then(|v| v.as_str()))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
    }
}

/// 从 Python repr 格式的字符串中提取 token 值
/// 如: "{'access_token': 'eyJ...', 'refresh_token': '...'}"
fn extract_token_from_str<'a>(s: &'a str, key: &str) -> Option<&'a str> {
    let pattern = format!("'{}': '", key);
    if let Some(start) = s.find(&pattern) {
        let value_start = start + pattern.len();
        if let Some(end) = s[value_start..].find('\'') {
            return Some(&s[value_start..value_start + end]);
        }
    }
    None
}

impl AccountStore {
    /// 从 auth_json 中提取 account_id
    pub fn extract_account_id(auth_json: &Value) -> Option<String> {
        auth_json
            .get("tokens")
            .and_then(|t| t.get("account_id"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
    }

    /// 账号身份是否一致（优先 account_id，其次 openai user id）
    pub fn auth_identity_matches(local_auth: &Value, external_auth: &Value) -> bool {
        let local_account_id = Self::extract_account_id(local_auth);
        let external_account_id = Self::extract_account_id(external_auth);
        if let (Some(local), Some(external)) =
            (local_account_id.as_deref(), external_account_id.as_deref())
        {
            return local == external;
        }

        let local_uid = Self::extract_openai_user_id(local_auth);
        let external_uid = Self::extract_openai_user_id(external_auth);
        if let (Some(local), Some(external)) = (local_uid.as_deref(), external_uid.as_deref()) {
            return local == external;
        }

        false
    }

    fn extract_jwt_claims_from_auth(auth_json: &Value, token_key: &str) -> Option<Value> {
        let token = auth_json
            .get("tokens")
            .and_then(|t| t.get(token_key))
            .or_else(|| auth_json.get(token_key))
            .and_then(|v| v.as_str())?;

        let parts: Vec<&str> = token.split('.').collect();
        if parts.len() != 3 {
            return None;
        }

        use base64::Engine;
        let payload_part = parts[1];
        let mut padded = payload_part.to_string();
        while !padded.len().is_multiple_of(4) {
            padded.push('=');
        }

        let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload_part)
            .or_else(|_| base64::engine::general_purpose::STANDARD.decode(&padded))
            .ok()?;
        let json_str = String::from_utf8(decoded).ok()?;
        serde_json::from_str(&json_str).ok()
    }

    /// 从原始 Token 字符串提取 JWT Claims
    pub fn extract_jwt_claims_from_token(token: &str) -> Result<Value, String> {
        let parts: Vec<&str> = token.split('.').collect();
        if parts.len() != 3 {
            return Err("无效的 Token 格式".to_string());
        }

        use base64::Engine;
        let payload_part = parts[1];
        let mut padded = payload_part.to_string();
        while !padded.len().is_multiple_of(4) {
            padded.push('=');
        }

        let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload_part)
            .or_else(|_| base64::engine::general_purpose::STANDARD.decode(&padded))
            .map_err(|e| format!("Base64 解码失败: {}", e))?;
        let json_str = String::from_utf8(decoded).map_err(|e| format!("UTF-8 转换失败: {}", e))?;
        serde_json::from_str(&json_str).map_err(|e| format!("JSON 解析失败: {}", e))
    }

    /// 从 auth_json 中提取邮箱（优先 id_token claims）
    pub fn extract_email(auth_json: &Value) -> Option<String> {
        let claims = Self::extract_jwt_claims_from_auth(auth_json, "id_token")
            .or_else(|| Self::extract_jwt_claims_from_auth(auth_json, "access_token"))?;

        claims
            .get("email")
            .and_then(|v| v.as_str())
            .or_else(|| {
                claims
                    .get("https://api.openai.com/profile")
                    .and_then(|v| v.get("email"))
                    .and_then(|v| v.as_str())
            })
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
    }

    /// 从 auth_json 中提取 last_refresh（RFC3339 或时间戳）
    pub fn extract_last_refresh(auth_json: &Value) -> Option<DateTime<Utc>> {
        let raw = auth_json.get("last_refresh")?;
        if let Some(s) = raw.as_str() {
            return chrono::DateTime::parse_from_rfc3339(s)
                .map(|dt| dt.with_timezone(&Utc))
                .ok();
        }
        if let Some(ts) = raw.as_i64() {
            let secs = if ts > 1_000_000_000_000 {
                ts / 1000
            } else {
                ts
            };
            return chrono::DateTime::<Utc>::from_timestamp(secs, 0);
        }
        None
    }

    /// 是否需要按间隔触发本地刷新（已停用，统一交由 Codex 按需维护）
    pub fn needs_refresh_by_interval(_auth_json: &Value) -> bool {
        false
    }

    /// 为缺失 refresh_token 的账号做一次回填
    fn backfill_refresh_tokens(&mut self) -> bool {
        let mut changed = false;
        for account in self.accounts.values_mut() {
            if account
                .refresh_token
                .as_deref()
                .map(|s| s.trim().is_empty())
                .unwrap_or(false)
            {
                account.refresh_token = None;
                changed = true;
            }
            if account.refresh_token.is_none() {
                if let Some(rt) = Self::extract_refresh_token(&account.auth_json) {
                    account.refresh_token = Some(rt);
                    changed = true;
                }
            }
        }
        changed
    }

    /// 列出缺失 refresh_token 的账号（用于导入校验）
    pub fn accounts_missing_refresh_token(&self) -> Vec<String> {
        self.accounts
            .values()
            .filter(|account| account.refresh_token.is_none())
            .map(|account| account.name.clone())
            .collect()
    }

    pub fn accounts_missing_last_refresh(&self) -> Vec<String> {
        self.accounts
            .values()
            .filter(|account| Self::extract_last_refresh(&account.auth_json).is_none())
            .map(|account| account.name.clone())
            .collect()
    }

    /// 记录保活刷新尝试结果（失败）
    pub fn mark_keepalive_attempt_failed(&mut self, id: &str, reason: String) {
        if let Some(account) = self.accounts.get_mut(id) {
            account.keepalive.last_attempt_at = Some(Utc::now());
            account.keepalive.last_error = Some(reason);
        }
    }

    /// 记录保活刷新成功
    pub fn mark_keepalive_attempt_success(&mut self, id: &str) {
        if let Some(account) = self.accounts.get_mut(id) {
            let now = Utc::now();
            account.keepalive.last_attempt_at = Some(now);
            account.keepalive.last_success_at = Some(now);
            account.keepalive.last_error = None;
        }
    }

    /// 对非当前账号：是否应触发保活刷新
    pub fn should_refresh_inactive_account(account: &Account, inactive_refresh_days: u32) -> bool {
        if !account.keepalive.inactive_refresh_enabled {
            return false;
        }
        let refresh_days = i64::from(inactive_refresh_days.max(1));
        match Self::extract_last_refresh(&account.auth_json) {
            Some(last) => last <= Utc::now() - chrono::Duration::days(refresh_days),
            None => true,
        }
    }

    /// 应用 refresh token 成功返回的新令牌（原子更新账号结构）
    pub fn apply_refreshed_tokens(
        account: &mut Account,
        access_token: String,
        refresh_token: Option<String>,
        id_token: Option<String>,
        expires_in: Option<u64>,
    ) {
        let now = Utc::now();

        if let Some(obj) = account.auth_json.as_object_mut() {
            // 如果 tokens 不存在或不是对象（如被存为字符串），重建为空对象
            let needs_reset = obj.get("tokens").map(|v| !v.is_object()).unwrap_or(true);
            if needs_reset {
                obj.insert("tokens".to_string(), serde_json::json!({}));
            }
            if let Some(tokens_obj) = obj.get_mut("tokens").and_then(|v| v.as_object_mut()) {
                tokens_obj.insert("access_token".to_string(), serde_json::json!(access_token));

                if let Some(rt) = refresh_token.as_ref() {
                    tokens_obj.insert("refresh_token".to_string(), serde_json::json!(rt));
                } else if let Some(existing_rt) = account.refresh_token.as_deref() {
                    if tokens_obj.get("refresh_token").is_none() {
                        tokens_obj
                            .insert("refresh_token".to_string(), serde_json::json!(existing_rt));
                    }
                }

                if let Some(idt) = id_token {
                    tokens_obj.insert("id_token".to_string(), serde_json::json!(idt));
                }

                if let Some(expires_secs) = expires_in {
                    let expires_at =
                        (now + chrono::Duration::seconds(expires_secs as i64)).to_rfc3339();
                    tokens_obj.insert("expires_at".to_string(), serde_json::json!(expires_at));
                }
            }
            obj.insert(
                "last_refresh".to_string(),
                serde_json::json!(now.to_rfc3339()),
            );
        }

        if let Some(rt) = refresh_token {
            account.refresh_token = Some(rt);
        } else if account.refresh_token.is_none() {
            account.refresh_token = Self::extract_refresh_token(&account.auth_json);
        }
    }

    /// 使用提供的 auth.json 同步指定账号
    /// 返回是否发生了更新
    pub fn sync_account_from_auth_json(&mut self, id: &str, auth_json: Value) -> bool {
        if let Some(account) = self.accounts.get_mut(id) {
            return Self::sync_account_from_auth_json_inner(account, auth_json);
        }
        false
    }

    fn sync_account_from_auth_json_inner(account: &mut Account, auth_json: Value) -> bool {
        // 安全检查：必须满足“身份一致（account_id/uid）”
        let local_account_id = Self::extract_account_id(&account.auth_json);
        let external_account_id = Self::extract_account_id(&auth_json);
        let local_uid = Self::extract_openai_user_id(&account.auth_json);
        let external_uid = Self::extract_openai_user_id(&auth_json);

        if !Self::auth_identity_matches(&account.auth_json, &auth_json) {
            eprintln!(
                "拒绝同步：身份不匹配 (外部 account_id: {:?}, 本地 account_id: {:?}, 外部 uid: {:?}, 本地 uid: {:?})",
                external_account_id, local_account_id, external_uid, local_uid
            );
            return false;
        }

        let local_name = account.name.trim().to_lowercase();
        let external_email = Self::extract_email(&auth_json).map(|s| s.to_lowercase());
        if local_name.contains('@') {
            if let Some(email) = external_email {
                if email != local_name {
                    eprintln!(
                        "拒绝同步：账号名与 token 邮箱不一致 (name: {:?}, token email: {:?})",
                        account.name, email
                    );
                    return false;
                }
            }
        }

        Self::sync_account_auth(account, auth_json);
        true
    }

    fn sync_account_auth(account: &mut Account, mut auth_json: Value) {
        if auth_json.get("last_refresh").is_none() {
            if let Some(existing) = account.auth_json.get("last_refresh") {
                if let Some(obj) = auth_json.as_object_mut() {
                    obj.insert("last_refresh".to_string(), existing.clone());
                }
            }
        }

        let new_rt = Self::extract_refresh_token(&auth_json);
        let fallback_rt = new_rt
            .clone()
            .or_else(|| account.refresh_token.clone())
            .or_else(|| Self::extract_refresh_token(&account.auth_json));

        if let Some(rt) = fallback_rt.as_deref() {
            if let Some(obj) = auth_json.as_object_mut() {
                if let Some(tokens_obj) = obj.get_mut("tokens").and_then(|v| v.as_object_mut()) {
                    if tokens_obj.get("refresh_token").is_none() {
                        tokens_obj.insert("refresh_token".to_string(), serde_json::json!(rt));
                    }
                }
            }
        }

        if let Some(rt) = new_rt {
            account.refresh_token = Some(rt);
        }

        account.auth_json = auth_json;
    }

    pub fn extract_openai_user_id(auth_json: &Value) -> Option<String> {
        let claims = Self::extract_jwt_claims_from_auth(auth_json, "access_token")?;

        // 1. 尝试特定的 profile 嵌套路径 (从 cat 输出看有这种结构)
        if let Some(profile) = claims.get("https://api.openai.com/profile") {
            if let Some(uid) = profile.get("user_id").and_then(|v| v.as_str()) {
                return Some(uid.to_string());
            }
        }

        // 2. 尝试常见 claim
        claims
            .get("https://api.openai.com/auth/user_id")
            .and_then(|v| v.as_str())
            .or_else(|| claims.get("user_id").and_then(|v| v.as_str()))
            .or_else(|| claims.get("sub").and_then(|v| v.as_str()))
            .map(|s| s.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn make_id_token(email: &str, account_id: &str) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"alg":"none"}"#);
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!(
            r#"{{"email":"{}","https://api.openai.com/auth":{{"chatgpt_account_id":"{}"}}}}"#,
            email, account_id
        ));
        format!("{header}.{payload}.sig")
    }

    fn auth_with_identity(email: &str, account_id: &str, refresh_token: &str) -> Value {
        serde_json::json!({
            "tokens": {
                "account_id": account_id,
                "refresh_token": refresh_token,
                "id_token": make_id_token(email, account_id),
                "access_token": "at.test.token"
            }
        })
    }

    #[test]
    fn test_add_account() {
        let mut store = AccountStore::default();
        let account = store.add_account(
            "测试账号".to_string(),
            serde_json::json!({"token": "test"}),
            None,
        );

        assert_eq!(store.accounts.len(), 1);
        assert_eq!(store.current, Some(account.id));
    }

    #[test]
    fn sync_rejects_when_email_mismatch_even_if_identity_matches() {
        let mut store = AccountStore::default();
        let local = auth_with_identity("hasbfarthoucapi@mail.com", "acct-1", "rt-a");
        let external = auth_with_identity("xtftbwvfp2025@outlook.com", "acct-1", "rt-b");
        let account = store.add_account("hasbfarthoucapi@mail.com".to_string(), local, None);

        let changed = store.sync_account_from_auth_json(&account.id, external);
        assert!(!changed, "email mismatch must reject sync");
    }

    #[test]
    fn sync_rejects_when_only_refresh_token_matches_but_identity_differs() {
        let mut store = AccountStore::default();
        let local = auth_with_identity("a@example.com", "acct-local", "rt-same");
        let external = auth_with_identity("a@example.com", "acct-other", "rt-same");
        let account = store.add_account("a@example.com".to_string(), local, None);

        let changed = store.sync_account_from_auth_json(&account.id, external);
        assert!(!changed, "refresh token equality must not be enough");
    }
}
