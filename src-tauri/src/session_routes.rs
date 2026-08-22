//! Session routes —— 用户级"硬路由"：把某个 Codex session_id 强制绑定到某个账号。
//!
//! 与 `session_affinity` 的关系：
//! - `session_affinity` 是基于"看到上游 cached_tokens>0"的软记忆，TTL 1h，自动失效。
//! - `session_routes` 是用户主动定义的"硬路由"，无 TTL；强制覆盖软记忆；即使账号被标
//!   banned/logged_out 也照样把流量送过去（让上游返回 401/403，用户能据此判断该号
//!   需要重新登录），不会触发自动切号。
//!
//! 持久化：`~/.codex-switcher/session_routes.json`，结构见下。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use uuid::Uuid;

/// 单条 session 硬路由
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SessionRoute {
    pub id: String,
    pub session_id: String,
    pub account_id: String,
    pub enabled: bool,
    pub label: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_hit_at: Option<DateTime<Utc>>,
    pub hit_count: u64,
}

impl SessionRoute {
    pub fn new(session_id: String, account_id: String, label: Option<String>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            session_id,
            account_id,
            enabled: true,
            label,
            created_at: Utc::now(),
            last_hit_at: None,
            hit_count: 0,
        }
    }
}

/// 磁盘上的持久化结构
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct SessionRoutesStore {
    /// route_id → SessionRoute
    pub routes: HashMap<String, SessionRoute>,
    pub version: u32,
}

impl SessionRoutesStore {
    /// 磁盘路径：`~/.codex-switcher/session_routes.json`
    pub fn config_path() -> PathBuf {
        dirs::home_dir()
            .expect("无法获取用户目录")
            .join(".codex-switcher")
            .join("session_routes.json")
    }

    /// 从磁盘读；任何错误都降级成空 store
    pub fn load() -> Self {
        let path = Self::config_path();
        if !path.exists() {
            return Self::default_with_version();
        }
        match fs::read_to_string(&path) {
            Ok(content) => match serde_json::from_str::<Self>(&content) {
                Ok(mut store) => {
                    if store.version == 0 {
                        store.version = 1;
                    }
                    store
                }
                Err(e) => {
                    eprintln!(
                        "[SessionRoutes] 解析 {} 失败 ({}), 使用空 store",
                        path.display(),
                        e
                    );
                    Self::default_with_version()
                }
            },
            Err(e) => {
                eprintln!(
                    "[SessionRoutes] 读取 {} 失败 ({}), 使用空 store",
                    path.display(),
                    e
                );
                Self::default_with_version()
            }
        }
    }

    fn default_with_version() -> Self {
        Self {
            routes: HashMap::new(),
            version: 1,
        }
    }

    /// 原子写：先写 *.tmp，再 rename 覆盖。
    pub fn save(&self) -> Result<(), String> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("创建目录失败: {}", e))?;
        }
        let tmp_path = path.with_extension("json.tmp");
        let content =
            serde_json::to_string_pretty(self).map_err(|e| format!("序列化失败: {}", e))?;
        {
            let mut f =
                fs::File::create(&tmp_path).map_err(|e| format!("创建临时文件失败: {}", e))?;
            f.write_all(content.as_bytes())
                .map_err(|e| format!("写入临时文件失败: {}", e))?;
            f.sync_all().ok();
        }
        fs::rename(&tmp_path, &path).map_err(|e| format!("重命名临时文件失败: {}", e))?;
        Ok(())
    }

    /// 返回所有 routes，按 created_at desc。
    pub fn list(&self) -> Vec<SessionRoute> {
        let mut v: Vec<SessionRoute> = self.routes.values().cloned().collect();
        v.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        v
    }

    /// 添加或 upsert（按 session_id 去重）。
    /// 若同 session_id 已存在：复用旧 route_id、保留 hit_count + last_hit_at，
    /// 但 account_id / label 用新值覆盖、enabled 重置为 true、created_at 刷新。
    pub fn add(
        &mut self,
        session_id: String,
        account_id: String,
        label: Option<String>,
    ) -> SessionRoute {
        let existing = self
            .routes
            .values()
            .find(|r| r.session_id == session_id)
            .cloned();
        let (route_id, hit_count, last_hit_at) = match existing.as_ref() {
            Some(old) => (old.id.clone(), old.hit_count, old.last_hit_at),
            None => (Uuid::new_v4().to_string(), 0, None),
        };
        // 删除旧条目（无论 id 是否相同，都重写一遍）
        if let Some(old) = existing {
            self.routes.remove(&old.id);
        }
        let route = SessionRoute {
            id: route_id,
            session_id,
            account_id,
            enabled: true,
            label,
            created_at: Utc::now(),
            last_hit_at,
            hit_count,
        };
        self.routes.insert(route.id.clone(), route.clone());
        route
    }

    pub fn delete(&mut self, id: &str) -> bool {
        self.routes.remove(id).is_some()
    }

    pub fn toggle(&mut self, id: &str, enabled: bool) -> bool {
        if let Some(r) = self.routes.get_mut(id) {
            r.enabled = enabled;
            true
        } else {
            false
        }
    }

    pub fn update_label(&mut self, id: &str, label: Option<String>) -> bool {
        if let Some(r) = self.routes.get_mut(id) {
            r.label = label;
            true
        } else {
            false
        }
    }

    /// 找到某个 session_id 对应的 enabled route（只看 enabled=true 的）。
    pub fn find_enabled_by_session(&self, session_id: &str) -> Option<SessionRoute> {
        self.routes
            .values()
            .find(|r| r.enabled && r.session_id == session_id)
            .cloned()
    }

    /// 路由命中：递增 hit_count 并刷新 last_hit_at。
    pub fn record_hit(&mut self, route_id: &str) {
        if let Some(r) = self.routes.get_mut(route_id) {
            r.hit_count = r.hit_count.saturating_add(1);
            r.last_hit_at = Some(Utc::now());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_route_serde_roundtrip() {
        let route = SessionRoute::new(
            "019df60d-92e2-7d80-9ba6-3ce1b2ab9b90".to_string(),
            "acc-abc".to_string(),
            Some("写注释".to_string()),
        );
        let s = serde_json::to_string(&route).expect("serialize");
        let parsed: SessionRoute = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(parsed.id, route.id);
        assert_eq!(parsed.session_id, route.session_id);
        assert_eq!(parsed.account_id, route.account_id);
        assert_eq!(parsed.enabled, true);
        assert_eq!(parsed.label.as_deref(), Some("写注释"));
        assert_eq!(parsed.hit_count, 0);
        assert!(parsed.last_hit_at.is_none());
    }

    #[test]
    fn add_upserts_same_session_id_and_preserves_hit_count() {
        let mut store = SessionRoutesStore::default_with_version();
        let r1 = store.add(
            "session-A".to_string(),
            "acc-1".to_string(),
            Some("旧 label".to_string()),
        );
        // 模拟若干次命中
        store.record_hit(&r1.id);
        store.record_hit(&r1.id);
        store.record_hit(&r1.id);
        let r1_after = store.routes.get(&r1.id).cloned().unwrap();
        assert_eq!(r1_after.hit_count, 3);
        assert!(r1_after.last_hit_at.is_some());

        // upsert：相同 session_id、新 account_id、新 label
        let r2 = store.add(
            "session-A".to_string(),
            "acc-2".to_string(),
            Some("新 label".to_string()),
        );
        // 同一 route_id（复用）
        assert_eq!(r2.id, r1.id);
        // hit_count 和 last_hit_at 保留
        assert_eq!(r2.hit_count, 3);
        assert_eq!(r2.last_hit_at, r1_after.last_hit_at);
        // 其他字段被覆盖
        assert_eq!(r2.account_id, "acc-2");
        assert_eq!(r2.label.as_deref(), Some("新 label"));
        // store 里只有 1 条
        assert_eq!(store.routes.len(), 1);
    }

    #[test]
    fn add_creates_new_route_for_different_session_id() {
        let mut store = SessionRoutesStore::default_with_version();
        let _r1 = store.add("session-A".to_string(), "acc-1".to_string(), None);
        let _r2 = store.add("session-B".to_string(), "acc-2".to_string(), None);
        assert_eq!(store.routes.len(), 2);
    }

    #[test]
    fn delete_removes() {
        let mut store = SessionRoutesStore::default_with_version();
        let r = store.add("session-A".to_string(), "acc-1".to_string(), None);
        assert!(store.delete(&r.id));
        assert!(!store.delete(&r.id)); // 第二次 false
        assert_eq!(store.routes.len(), 0);
    }

    #[test]
    fn toggle_changes_enabled() {
        let mut store = SessionRoutesStore::default_with_version();
        let r = store.add("session-A".to_string(), "acc-1".to_string(), None);
        assert!(store.routes.get(&r.id).unwrap().enabled);
        assert!(store.toggle(&r.id, false));
        assert!(!store.routes.get(&r.id).unwrap().enabled);
        assert!(store.toggle(&r.id, true));
        assert!(store.routes.get(&r.id).unwrap().enabled);
        // 不存在的 id
        assert!(!store.toggle("nope", true));
    }

    #[test]
    fn find_enabled_by_session_ignores_disabled() {
        let mut store = SessionRoutesStore::default_with_version();
        let r = store.add("session-A".to_string(), "acc-1".to_string(), None);
        assert!(store.find_enabled_by_session("session-A").is_some());
        store.toggle(&r.id, false);
        assert!(store.find_enabled_by_session("session-A").is_none());
    }

    #[test]
    fn update_label_updates() {
        let mut store = SessionRoutesStore::default_with_version();
        let r = store.add("session-A".to_string(), "acc-1".to_string(), None);
        assert!(store.update_label(&r.id, Some("new".to_string())));
        assert_eq!(
            store.routes.get(&r.id).unwrap().label.as_deref(),
            Some("new")
        );
        assert!(store.update_label(&r.id, None));
        assert!(store.routes.get(&r.id).unwrap().label.is_none());
        assert!(!store.update_label("nope", None));
    }
}
