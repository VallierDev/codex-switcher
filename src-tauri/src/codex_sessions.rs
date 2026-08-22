//! Codex 本地 session jsonl 扫描器。
//!
//! 目录布局：`~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`
//! 第一行类型 `session_meta`，payload 含 id / cwd / cli_version / timestamp / model_provider。
//! 后续行混杂 event_msg / response_item / turn_context；其中：
//!   - event_msg + payload.type=user_message 含 payload.message（首条用户输入）
//!   - turn_context payload 含 model 字段
//!
//! 这里只读首行 + 前 N 行（默认 60），避免把整个 jsonl（动辄几 MB）读进内存。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// 单个 Codex 本地 session 的精简元信息
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CodexSession {
    pub session_id: String,
    pub rollout_path: String,
    pub started_at: DateTime<Utc>,
    pub cwd: Option<String>,
    pub cli_version: Option<String>,
    pub first_user_text: Option<String>,
    pub model: Option<String>,
}

/// 扫描 `~/.codex/sessions`，按 started_at desc 返回。
///
/// 过滤条件：
/// - `limit`：截断到前 N 条（默认 50）
/// - `project_filter`：cwd 的子串大小写不敏感匹配
/// - `days_back`：只保留 started_at >= now-days_back 的（默认 14 天）
pub fn list_codex_sessions(
    limit: Option<usize>,
    project_filter: Option<String>,
    days_back: Option<u32>,
) -> Result<Vec<CodexSession>, String> {
    let limit = limit.unwrap_or(50);
    let days_back = days_back.unwrap_or(14);
    let filter_lower = project_filter.map(|s| s.to_lowercase());

    let root = sessions_root();
    if !root.exists() {
        return Ok(Vec::new());
    }

    let cutoff = Utc::now() - chrono::Duration::days(days_back as i64);

    // 收集所有候选 jsonl 路径
    let files = walk_jsonl(&root);

    // 解析每个文件的首行（+ 前几行抓 first_user_text/model）
    let mut sessions: Vec<CodexSession> = Vec::new();
    for path in files {
        match parse_session_head(&path) {
            Ok(Some(s)) => {
                if s.started_at < cutoff {
                    continue;
                }
                if let Some(ref f) = filter_lower {
                    let cwd_l = s.cwd.as_deref().unwrap_or("").to_lowercase();
                    if !cwd_l.contains(f) {
                        continue;
                    }
                }
                sessions.push(s);
            }
            Ok(None) => continue,
            Err(e) => {
                eprintln!("[CodexSessions] 解析 {} 失败: {}", path.display(), e);
                continue;
            }
        }
    }

    // 按 started_at desc 排序
    sessions.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    sessions.truncate(limit);
    Ok(sessions)
}

fn sessions_root() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".codex").join("sessions"))
        .unwrap_or_else(|| PathBuf::from(".codex/sessions"))
}

/// 检测"当前活跃的 codex 会话"：
/// 扫 `~/.codex/sessions` 下所有 rollout-*.jsonl，按文件 mtime 选最新一条；
/// 若该文件在 `window_secs` 秒内被更新过则返回该 session 的元信息，否则返回 None。
///
/// 用于 UI "绑定到当前活跃会话" 按钮：用户在 codex 里刚发完 / 正在发消息时，
/// 对应的 rollout 文件刚被写过，mtime 在窗口内 → 自动定位到那个 session_id。
pub fn detect_active_session(window_secs: u64) -> Option<CodexSession> {
    use std::time::SystemTime;
    let root = sessions_root();
    if !root.exists() {
        return None;
    }
    let files = walk_jsonl(&root);
    // 找 mtime 最新的一条
    let now = SystemTime::now();
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for p in files {
        let mtime = match fs::metadata(&p).and_then(|m| m.modified()) {
            Ok(t) => t,
            Err(_) => continue,
        };
        // 窗口外的直接跳
        if let Ok(age) = now.duration_since(mtime) {
            if age.as_secs() > window_secs {
                continue;
            }
        }
        match &best {
            Some((b, _)) if &mtime <= b => {}
            _ => best = Some((mtime, p)),
        }
    }
    let (_, path) = best?;
    parse_session_head(&path).ok().flatten()
}

/// 递归收集 root 下所有 `rollout-*.jsonl`（YYYY/MM/DD 三层目录结构）。
/// 用标准库 read_dir 实现，避免引入 walkdir 依赖。
fn walk_jsonl(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    // YYYY
    let year_dirs = match fs::read_dir(root) {
        Ok(d) => d,
        Err(_) => return out,
    };
    for y in year_dirs.flatten() {
        if !y.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let month_dirs = match fs::read_dir(y.path()) {
            Ok(d) => d,
            Err(_) => continue,
        };
        for m in month_dirs.flatten() {
            if !m.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let day_dirs = match fs::read_dir(m.path()) {
                Ok(d) => d,
                Err(_) => continue,
            };
            for d in day_dirs.flatten() {
                if !d.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    continue;
                }
                let files = match fs::read_dir(d.path()) {
                    Ok(f) => f,
                    Err(_) => continue,
                };
                for f in files.flatten() {
                    let p = f.path();
                    if p.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                        continue;
                    }
                    if let Some(name) = p.file_name().and_then(|s| s.to_str()) {
                        if name.starts_with("rollout-") {
                            out.push(p);
                        }
                    }
                }
            }
        }
    }
    out
}

/// 只读前 ~20 行（首行 session_meta 必须，后续行机会主义抓 first_user_text/model）。
/// 返回 `Ok(None)` 表示文件不像合法 session（首行不是 session_meta，跳过）。
fn parse_session_head(path: &Path) -> Result<Option<CodexSession>, String> {
    let f = fs::File::open(path).map_err(|e| format!("open: {}", e))?;
    let mut reader = BufReader::new(f);

    // 首行
    let mut first_line = String::new();
    if reader
        .read_line(&mut first_line)
        .map_err(|e| format!("read first line: {}", e))?
        == 0
    {
        return Ok(None);
    }
    let first: serde_json::Value = match serde_json::from_str(first_line.trim()) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    if first.get("type").and_then(|v| v.as_str()) != Some("session_meta") {
        return Ok(None);
    }
    let payload = first
        .get("payload")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let session_id = payload
        .get("id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let session_id = match session_id {
        Some(id) if !id.is_empty() => id,
        _ => return Ok(None),
    };
    let cwd = payload
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let cli_version = payload
        .get("cli_version")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    // started_at 优先用 payload.timestamp，回退到顶层 timestamp
    let ts_str = payload
        .get("timestamp")
        .and_then(|v| v.as_str())
        .or_else(|| first.get("timestamp").and_then(|v| v.as_str()));
    let started_at: DateTime<Utc> = match ts_str.and_then(|s| {
        chrono::DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|dt| dt.with_timezone(&Utc))
    }) {
        Some(t) => t,
        None => Utc::now(), // 兜底
    };

    // 后续 20 行：抓 first_user_text 和 model
    let mut first_user_text: Option<String> = None;
    let mut model: Option<String> = None;
    for _ in 0..20 {
        let mut line = String::new();
        let n = reader
            .read_line(&mut line)
            .map_err(|e| format!("read line: {}", e))?;
        if n == 0 {
            break;
        }
        let v: serde_json::Value = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let ty = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
        let p = v.get("payload").cloned().unwrap_or(serde_json::Value::Null);
        let p_ty = p.get("type").and_then(|x| x.as_str()).unwrap_or("");

        // user_message：event_msg payload.type=user_message
        if first_user_text.is_none() && ty == "event_msg" && p_ty == "user_message" {
            if let Some(msg) = p.get("message").and_then(|x| x.as_str()) {
                first_user_text = Some(clean_user_text(msg));
            }
        }
        // model：通常在 turn_context payload.model 里出现
        if model.is_none() {
            if let Some(m) = p.get("model").and_then(|x| x.as_str()) {
                if !m.is_empty() {
                    model = Some(m.to_string());
                }
            }
        }
        if first_user_text.is_some() && model.is_some() {
            break;
        }
    }

    Ok(Some(CodexSession {
        session_id,
        rollout_path: path.to_string_lossy().to_string(),
        started_at,
        cwd,
        cli_version,
        first_user_text,
        model,
    }))
}

/// 把首条 user message 压成单行 ≤200 字符。
fn clean_user_text(s: &str) -> String {
    let collapsed: String = s
        .chars()
        .map(|c| {
            if c == '\n' || c == '\r' || c == '\t' {
                ' '
            } else {
                c
            }
        })
        .collect();
    // 压连续空格
    let mut out = String::with_capacity(collapsed.len().min(220));
    let mut prev_space = false;
    for ch in collapsed.chars() {
        if ch == ' ' {
            if prev_space {
                continue;
            }
            prev_space = true;
            out.push(' ');
        } else {
            prev_space = false;
            out.push(ch);
        }
    }
    let out = out.trim().to_string();
    // 按 char 截断，避免切坏 UTF-8 多字节
    let truncated: String = out.chars().take(200).collect();
    truncated
}

#[cfg(test)]
mod tests {
    use super::clean_user_text;

    #[test]
    fn clean_user_text_collapses_whitespace_and_truncates() {
        let s = "hello\n\nworld\t   foo";
        assert_eq!(clean_user_text(s), "hello world foo");
        let long = "a".repeat(300);
        assert_eq!(clean_user_text(&long).len(), 200);
    }

    #[test]
    fn clean_user_text_handles_multibyte() {
        // 200 个汉字应被原样保留（每个汉字算 1 char，不会被切坏）
        let zh = "中".repeat(250);
        let cleaned = clean_user_text(&zh);
        assert_eq!(cleaned.chars().count(), 200);
    }
}
