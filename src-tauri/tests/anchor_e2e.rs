//! Anchor 端到端集成测试 (v0.7.1)
//!
//! 跑真实的 `AccountStore::switch_to` + `restore_disk_real_expiry_for_anchor`
//! 流程，验证 `~/.codex/auth.json` 的实际内容。通过 debug-only 的
//! `CODEX_SWITCHER_TEST_HOME` 环境变量
//! 把磁盘路径重定向到 tempdir，不污染用户真实状态。
//!
//! 一个进程内多次 setenv 不安全（其他线程同时读会 race），所以本文件全部
//! 测试通过单个 #[test] 串行跑完，避免 `cargo test` 默认的并行执行。
//!
//! 跑法：`cargo test --test anchor_e2e -- --nocapture`

use base64::Engine;
use codex_switcher_lib::account::AccountStore;
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock};

fn lock_test_home() -> MutexGuard<'static, ()> {
    static TEST_HOME_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    TEST_HOME_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 测试 home 重定向 RAII 守卫：drop 时还原原值。
struct TestHomeGuard {
    original: Option<String>,
    _tmp: PathBuf,
}

impl TestHomeGuard {
    fn redirect_to(tmp: PathBuf) -> Self {
        let original = std::env::var("CODEX_SWITCHER_TEST_HOME").ok();
        std::env::set_var("CODEX_SWITCHER_TEST_HOME", &tmp);
        Self {
            original,
            _tmp: tmp,
        }
    }
}

impl Drop for TestHomeGuard {
    fn drop(&mut self) {
        match self.original.take() {
            Some(v) => std::env::set_var("CODEX_SWITCHER_TEST_HOME", v),
            None => std::env::remove_var("CODEX_SWITCHER_TEST_HOME"),
        }
    }
}

fn make_tmpdir(label: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("codex-switcher-anchor-e2e-{}-{}", label, stamp));
    fs::create_dir_all(&dir).unwrap();
    fs::create_dir_all(dir.join(".codex")).unwrap();
    dir
}

fn jwt_with_exp(exp_secs_from_now: i64) -> String {
    let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"alg":"none"}"#);
    let now = chrono::Utc::now().timestamp();
    let payload = json!({
        "iat": now,
        "exp": now + exp_secs_from_now,
        "https://api.openai.com/auth": {
            "chatgpt_account_id": "test-acct",
        },
    });
    let payload_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&payload).unwrap());
    format!("{header}.{payload_b64}.sig")
}

fn make_oauth_auth(email: &str, account_id: &str, refresh_token: &str, at_exp_secs: i64) -> Value {
    let id_token_header =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"alg":"none"}"#);
    let id_token_payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!(
        r#"{{"email":"{}","https://api.openai.com/auth":{{"chatgpt_account_id":"{}"}}}}"#,
        email, account_id
    ));
    let id_token = format!("{id_token_header}.{id_token_payload}.sig");
    json!({
        "tokens": {
            "account_id": account_id,
            "refresh_token": refresh_token,
            "id_token": id_token,
            "access_token": jwt_with_exp(at_exp_secs),
            "expires_at": (chrono::Utc::now() + chrono::Duration::seconds(at_exp_secs))
                .to_rfc3339(),
        },
        "last_refresh": chrono::Utc::now().to_rfc3339(),
    })
}

fn read_disk_auth(home: &PathBuf) -> Option<Value> {
    let path = home.join(".codex/auth.json");
    if !path.exists() {
        return None;
    }
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn account_id_of(auth: &Value) -> Option<&str> {
    auth.pointer("/tokens/account_id")?.as_str()
}

/// 串行跑完 4 个子场景：
///   1. 无 anchor 时切号 → disk 跟随 target
///   2. 设了 anchor 切到非 anchor → disk 保持 anchor 不变
///   3. 切回 anchor → disk 同步到 anchor
///   4. 退出兜底 → disk expires_at 切到 JWT 真实 exp（不是 OAuth 字段值）
#[test]
fn anchor_e2e_disk_routing() {
    let _test_home_lock = lock_test_home();
    let tmp = make_tmpdir("disk-routing");
    let _guard = TestHomeGuard::redirect_to(tmp.clone());

    let mut store = AccountStore::default();
    let pro_id = store
        .add_account(
            "pro@example.com".to_string(),
            make_oauth_auth("pro@example.com", "acct-pro", "rt-pro", 864_000),
            None,
        )
        .id;
    let other_id = store
        .add_account(
            "other@example.com".to_string(),
            make_oauth_auth("other@example.com", "acct-other", "rt-other", 864_000),
            None,
        )
        .id;

    // ---- 场景 1: 无 anchor + switch_to(other) → disk 是 other 的 ----
    store.switch_to(&other_id, false).expect("switch ok");
    let disk = read_disk_auth(&tmp).expect("disk written");
    assert_eq!(
        account_id_of(&disk),
        Some("acct-other"),
        "无 anchor 时 disk 应跟随 target"
    );

    // ---- 场景 2: 设 anchor=pro，切到 other → disk 保持 pro 不变 ----
    store
        .set_session_anchor(&pro_id, true)
        .expect("set anchor ok");
    // 先把 disk 强制写成 pro 的（模拟"设 anchor 时立刻落盘"那一步）
    AccountStore::write_codex_auth(&store.accounts.get(&pro_id).unwrap().to_codex_auth_value())
        .expect("seed disk with anchor");
    let disk_before = read_disk_auth(&tmp).expect("disk has anchor seed");
    assert_eq!(account_id_of(&disk_before), Some("acct-pro"));

    store.switch_to(&other_id, false).expect("switch ok");
    let disk_after = read_disk_auth(&tmp).expect("disk still exists");
    assert_eq!(
        account_id_of(&disk_after),
        Some("acct-pro"),
        "anchor 生效时 switch 不应碰盘"
    );
    assert_eq!(
        store.current.as_deref(),
        Some(other_id.as_str()),
        "store.current 仍要切到 other"
    );
    assert!(
        !store.should_write_disk_for(&other_id),
        "non-anchor target 不允许写盘"
    );
    assert!(store.should_write_disk_for(&pro_id), "anchor 自己仍可写盘");

    // ---- 场景 3: 切回 anchor → disk 应被更新（即便内容跟 disk 已有的一样） ----
    let before_mtime = fs::metadata(tmp.join(".codex/auth.json"))
        .unwrap()
        .modified()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    store.switch_to(&pro_id, false).expect("switch back ok");
    let after_mtime = fs::metadata(tmp.join(".codex/auth.json"))
        .unwrap()
        .modified()
        .unwrap();
    assert!(
        after_mtime > before_mtime,
        "anchor=current 时应触发写盘（mtime 应该更新）"
    );
    let disk_after_back = read_disk_auth(&tmp).expect("ok");
    assert_eq!(account_id_of(&disk_after_back), Some("acct-pro"));

    // ---- 场景 4: 退出兜底 → 把 disk expires_at 写成 JWT 真实 exp ----
    // anchor 当前是 pro，access_token JWT exp 是 +864000s（10 天）
    // 当前 store 内 expires_at 是 OAuth 字段值（+864000s 也是 10 天，因为我们 fixture
    // 故意把两者对齐到一致）。为了验证 restore 真的用 JWT，我们手工把 store 内的
    // expires_at 改成一个奇怪值，看 restore 落盘的是 JWT exp 还是 store 字段。
    {
        let acc = store.accounts.get_mut(&pro_id).unwrap();
        let tokens = acc
            .auth_json
            .get_mut("tokens")
            .and_then(|v| v.as_object_mut())
            .unwrap();
        tokens.insert(
            "expires_at".to_string(),
            Value::String("1999-01-01T00:00:00+00:00".to_string()),
        );
    }
    let wrote = store
        .restore_disk_real_expiry_for_anchor()
        .expect("restore ok");
    assert!(wrote, "有 anchor 时 restore 应返回 true");
    let disk_restored = read_disk_auth(&tmp).expect("disk re-written");
    let restored_exp = disk_restored
        .pointer("/tokens/expires_at")
        .and_then(|v| v.as_str())
        .expect("expires_at present");
    let restored_dt = chrono::DateTime::parse_from_rfc3339(restored_exp).unwrap();
    let now = chrono::Utc::now();
    let delta_hours = (restored_dt.with_timezone(&chrono::Utc) - now).num_hours();
    assert!(
        delta_hours > 200 && delta_hours < 250,
        "restore 应写 JWT exp（约 240h 后），实际 delta={}h",
        delta_hours
    );
    assert_ne!(
        restored_exp, "1999-01-01T00:00:00+00:00",
        "不应是 store 内被污染的字段值"
    );
}

/// 非 OAuth 账号被拒绝当 anchor，且不污染 store 状态。
#[test]
fn anchor_rejected_on_non_oauth_does_not_corrupt_state() {
    let _test_home_lock = lock_test_home();
    let tmp = make_tmpdir("relay-reject");
    let _guard = TestHomeGuard::redirect_to(tmp);

    let mut store = AccountStore::default();
    let relay = store.add_relay_account(
        "Test Relay".to_string(),
        "https://example.com".to_string(),
        "sk-fake".to_string(),
        None,
        None,
        None,
        None,
        None,
        None,
        Some("responses".to_string()),
        None,
    );
    let oauth_id = store
        .add_account(
            "pro@example.com".to_string(),
            make_oauth_auth("pro@example.com", "acct-pro", "rt-pro", 864_000),
            None,
        )
        .id;

    store
        .set_session_anchor(&oauth_id, true)
        .expect("OAuth 设 anchor ok");
    let err = store
        .set_session_anchor(&relay.id, true)
        .expect_err("relay 不能当 anchor");
    assert!(err.contains("ChatGPT 订阅号"));

    assert_eq!(
        store.session_anchor_id().as_deref(),
        Some(oauth_id.as_str()),
        "失败后原 anchor 应保留"
    );
    assert!(!store.accounts.get(&relay.id).unwrap().is_session_anchor);
}
