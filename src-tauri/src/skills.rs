//! Skills 管理模块
//!
//! SSOT 目录: ~/.codex/skills/
//! 同步到: ~/.claude/skills/, ~/.gemini/skills/, ~/.config/opencode/skills/, ~/.agents/skills/
//! 数据存储: ~/.codex-switcher/skills.json

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// ────────────────────────────────────────────────────────────────
// 数据结构
// ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillApps {
    pub codex: bool,
    pub claude: bool,
    pub gemini: bool,
    pub opencode: bool,
    #[serde(default)]
    pub zcode: bool,
}

impl Default for SkillApps {
    fn default() -> Self {
        Self {
            codex: true,
            claude: false,
            gemini: false,
            opencode: false,
            zcode: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledSkill {
    pub id: String,
    pub name: String,
    pub description: String,
    pub directory: String,
    pub source: String, // "local" | "github"
    pub repo_owner: Option<String>,
    pub repo_name: Option<String>,
    pub apps: SkillApps,
    pub installed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillRepo {
    pub owner: String,
    pub name: String,
    pub branch: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoverableSkill {
    pub key: String,
    pub name: String,
    pub description: String,
    pub directory: String,
    pub repo_owner: String,
    pub repo_name: String,
    pub repo_branch: String,
    pub installed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillData {
    #[serde(default)]
    pub skills: Vec<InstalledSkill>,
    #[serde(default = "default_repos")]
    pub repos: Vec<SkillRepo>,
}

fn default_repos() -> Vec<SkillRepo> {
    vec![
        SkillRepo {
            owner: "anthropics".into(),
            name: "skills".into(),
            branch: "main".into(),
            enabled: true,
        },
        SkillRepo {
            owner: "ComposioHQ".into(),
            name: "awesome-claude-skills".into(),
            branch: "master".into(),
            enabled: true,
        },
    ]
}

impl Default for SkillData {
    fn default() -> Self {
        Self {
            skills: Vec::new(),
            repos: default_repos(),
        }
    }
}

// ────────────────────────────────────────────────────────────────
// 路径
// ────────────────────────────────────────────────────────────────

/// SSOT 目录：~/.codex-switcher/skills/
fn ssot_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap()
        .join(".codex-switcher")
        .join("skills")
}

/// 各 CLI 的 skills 目录（跨平台）
fn app_skills_dir(app: &str) -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    match app {
        "codex" => Some(home.join(".codex").join("skills")),
        "claude" => Some(home.join(".claude").join("skills")),
        "gemini" => Some(home.join(".gemini").join("skills")),
        "zcode" => Some(home.join(".agents").join("skills")),
        "opencode" => {
            // Windows: %APPDATA%\opencode\skills, Unix: ~/.config/opencode/skills
            #[cfg(windows)]
            {
                dirs::config_dir().map(|c| c.join("opencode").join("skills"))
            }
            #[cfg(not(windows))]
            {
                Some(home.join(".config").join("opencode").join("skills"))
            }
        }
        _ => None,
    }
}

/// 初始化 SSOT：如果 ~/.codex/skills/ 是真实目录（非 symlink），迁移到 SSOT
pub fn init_ssot() -> Result<(), String> {
    let ssot = ssot_dir();
    let codex_skills = dirs::home_dir().unwrap().join(".codex").join("skills");

    // SSOT 已存在且 codex 已经是 symlink → 不需要迁移
    if ssot.exists() && codex_skills.is_symlink() {
        return Ok(());
    }

    // SSOT 不存在 → 需要创建
    if !ssot.exists() {
        std::fs::create_dir_all(&ssot).map_err(|e| format!("创建 SSOT 目录失败: {}", e))?;

        // 如果 ~/.codex/skills/ 是真实目录（有内容），迁移过来
        if codex_skills.exists() && codex_skills.is_dir() && !codex_skills.is_symlink() {
            println!("[Skills] 迁移 ~/.codex/skills/ → SSOT...");
            let entries: Vec<_> = std::fs::read_dir(&codex_skills)
                .map_err(|e| format!("读取目录失败: {}", e))?
                .flatten()
                .collect();

            for entry in entries {
                let src = entry.path();
                let name = match src.file_name().and_then(|n| n.to_str()) {
                    Some(n) => n.to_string(),
                    None => continue,
                };
                let dst = ssot.join(&name);
                // 移动（同一文件系统用 rename，跨文件系统用 copy+delete）
                if std::fs::rename(&src, &dst).is_err() {
                    copy_dir_recursive(&src, &dst)?;
                    let _ = std::fs::remove_dir_all(&src);
                }
            }
            println!("[Skills] 迁移完成");

            // 删除原目录
            let _ = std::fs::remove_dir_all(&codex_skills);
        }
    }

    // 确保各 CLI 的 skills 目录是指向 SSOT 的 symlink
    let apps = ["codex", "claude", "gemini", "opencode", "zcode"];
    for app in &apps {
        link_app_to_ssot(app)?;
    }

    Ok(())
}

/// 将某个 CLI 的 skills 目录 symlink 到 SSOT
fn link_app_to_ssot(app: &str) -> Result<(), String> {
    let target = match app_skills_dir(app) {
        Some(d) => d,
        None => return Ok(()),
    };
    let ssot = ssot_dir();

    // 已经是正确的 symlink → 跳过
    if target.is_symlink() {
        if let Ok(link_target) = std::fs::read_link(&target) {
            if link_target == ssot {
                return Ok(());
            }
        }
        // symlink 指向了错误目标，删掉重建
        let _ = std::fs::remove_file(&target);
    } else if target.exists() {
        // 是一个真实目录但是空的 → 删掉
        if target.is_dir() {
            let is_empty = std::fs::read_dir(&target)
                .map(|mut d| d.next().is_none())
                .unwrap_or(false);
            if is_empty {
                let _ = std::fs::remove_dir(&target);
            } else if app == "zcode" {
                migrate_existing_app_skills_to_ssot(app, &target, &ssot)?;
                let _ = std::fs::remove_dir_all(&target);
            } else {
                // 非空真实目录 → 不覆盖，用户可能有独立内容
                println!("[Skills] {} skills 目录非空且不是 symlink，跳过", app);
                return Ok(());
            }
        }
    }

    // 确保父目录存在
    if let Some(parent) = target.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    // 创建 symlink / junction
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&ssot, &target)
            .map_err(|e| format!("创建 symlink 失败: {}", e))?;
        println!("[Skills] {} → SSOT (symlink)", app);
    }

    #[cfg(windows)]
    {
        // Windows: 用 junction（不需要管理员权限）
        let status = std::process::Command::new("cmd")
            .args([
                "/C",
                "mklink",
                "/J",
                &target.to_string_lossy(),
                &ssot.to_string_lossy(),
            ])
            .output();

        match status {
            Ok(out) if out.status.success() => {
                println!("[Skills] {} → SSOT (junction)", app);
            }
            _ => {
                // junction 失败，fallback 到 copy
                println!("[Skills] junction 失败，使用 copy 模式");
                copy_dir_recursive(&ssot, &target)?;
                println!("[Skills] {} → SSOT (copy)", app);
            }
        }
    }

    Ok(())
}

/// ZCode 的共享目录 ~/.agents/skills 可能已被独立安装过 skill。
/// 启用共享时先并入 SSOT，再把目录替换成 symlink，避免丢失原有内容。
fn migrate_existing_app_skills_to_ssot(
    app: &str,
    target: &std::path::Path,
    ssot: &std::path::Path,
) -> Result<(), String> {
    let backup_root = dirs::home_dir()
        .unwrap()
        .join(".codex-switcher")
        .join("skills-backup")
        .join(format!("{}-{}", app, Utc::now().format("%Y%m%d%H%M%S")));

    for entry in std::fs::read_dir(target).map_err(|e| format!("读取目录失败: {}", e))? {
        let entry = entry.map_err(|e| format!("读取条目失败: {}", e))?;
        let name = entry.file_name();
        let src = entry.path();
        let dst = ssot.join(&name);

        if !dst.exists() {
            if src.is_dir() {
                copy_dir_recursive(&src, &dst)?;
            } else {
                if let Some(parent) = dst.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                std::fs::copy(&src, &dst).map_err(|e| format!("复制文件失败: {}", e))?;
            }
            continue;
        }

        let backup = backup_root.join(&name);
        if src.is_dir() {
            copy_dir_recursive(&src, &backup)?;
        } else {
            if let Some(parent) = backup.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            std::fs::copy(&src, &backup).map_err(|e| format!("备份文件失败: {}", e))?;
        }
    }

    println!("[Skills] 已迁移 {} 现有 skills 到 SSOT", app);
    Ok(())
}

fn data_path() -> PathBuf {
    dirs::home_dir()
        .unwrap()
        .join(".codex-switcher")
        .join("skills.json")
}

// ────────────────────────────────────────────────────────────────
// SkillStore
// ────────────────────────────────────────────────────────────────

pub struct SkillStore;

impl SkillStore {
    pub fn load() -> SkillData {
        let path = data_path();
        if let Ok(content) = std::fs::read_to_string(&path) {
            serde_json::from_str(&content).unwrap_or_default()
        } else {
            SkillData::default()
        }
    }

    pub fn save(data: &SkillData) -> Result<(), String> {
        let path = data_path();
        let json = serde_json::to_string_pretty(data).map_err(|e| format!("序列化失败: {}", e))?;
        std::fs::write(&path, json).map_err(|e| format!("写入失败: {}", e))
    }

    /// 扫描 SSOT 目录，补录未记录的 skills
    pub fn scan_existing(data: &mut SkillData) -> usize {
        let ssot = ssot_dir();
        if !ssot.exists() {
            return 0;
        }

        let existing_dirs: std::collections::HashSet<String> =
            data.skills.iter().map(|s| s.directory.clone()).collect();

        let mut imported = 0;

        if let Ok(entries) = std::fs::read_dir(&ssot) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let dir_name = match path.file_name().and_then(|n| n.to_str()) {
                    Some(n) => n.to_string(),
                    None => continue,
                };

                // 跳过隐藏目录
                if dir_name.starts_with('.') {
                    continue;
                }

                // 已记录则跳过
                if existing_dirs.contains(&dir_name) {
                    continue;
                }

                // 解析 SKILL.md
                let (name, description) = parse_skill_md(&path);

                data.skills.push(InstalledSkill {
                    id: format!("local:{}", dir_name),
                    name: name.unwrap_or_else(|| dir_name.clone()),
                    description: description.unwrap_or_default(),
                    directory: dir_name,
                    source: "local".into(),
                    repo_owner: None,
                    repo_name: None,
                    apps: SkillApps::default(), // codex=true, others=false
                    installed_at: Utc::now(),
                });

                imported += 1;
            }
        }

        imported
    }

    /// 切换某个 app 的整个 skills 目录 symlink
    /// 新架构：整个目录是 symlink，不需要 per-skill 同步
    pub fn toggle_app_link(app: &str, enabled: bool) -> Result<(), String> {
        let target = app_skills_dir(app).ok_or_else(|| format!("未知 app: {}", app))?;

        if enabled {
            link_app_to_ssot(app)?;
        } else {
            // 移除 symlink / junction
            if target.is_symlink() {
                let _ = std::fs::remove_file(&target);
                println!("[Skills] 已断开 {} 的 skills 链接", app);
            } else if target.is_dir() {
                // Windows junction 或 copy 模式
                #[cfg(windows)]
                {
                    // junction 用 rmdir 移除
                    let _ = std::process::Command::new("cmd")
                        .args(["/C", "rmdir", &target.to_string_lossy()])
                        .output();
                }
                #[cfg(not(windows))]
                {
                    let _ = std::fs::remove_dir_all(&target);
                }
                println!("[Skills] 已断开 {} 的 skills 链接", app);
            }
        }
        Ok(())
    }

    /// 获取各 app 的链接状态
    pub fn get_app_link_status() -> std::collections::HashMap<String, bool> {
        let mut status = std::collections::HashMap::new();
        let ssot = ssot_dir();

        for app in &["codex", "claude", "gemini", "opencode", "zcode"] {
            let linked = if let Some(target) = app_skills_dir(app) {
                if target.is_symlink() {
                    std::fs::read_link(&target)
                        .map(|t| t == ssot)
                        .unwrap_or(false)
                } else {
                    // codex 可能直接是 SSOT 本身
                    target == ssot || (target.exists() && target.is_dir())
                }
            } else {
                false
            };
            status.insert(app.to_string(), linked);
        }
        status
    }

    /// 从 GitHub 仓库发现可用 skills
    pub async fn discover_skills(repos: &[SkillRepo]) -> Vec<DiscoverableSkill> {
        let mut result = Vec::new();
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .unwrap();

        for repo in repos.iter().filter(|r| r.enabled) {
            let url = format!(
                "https://github.com/{}/{}/archive/refs/heads/{}.zip",
                repo.owner, repo.name, repo.branch
            );

            println!("[Skills] 扫描仓库 {}/{}...", repo.owner, repo.name);

            let resp = match client.get(&url).send().await {
                Ok(r) if r.status().is_success() => r,
                _ => {
                    eprintln!("[Skills] 下载仓库 {}/{} 失败", repo.owner, repo.name);
                    continue;
                }
            };

            let bytes = match resp.bytes().await {
                Ok(b) => b,
                Err(_) => continue,
            };

            // 解压到临时目录
            let tmp =
                std::env::temp_dir().join(format!("codex-skills-{}-{}", repo.owner, repo.name));
            let _ = std::fs::remove_dir_all(&tmp);
            let _ = std::fs::create_dir_all(&tmp);

            if let Err(e) = extract_zip(&bytes, &tmp) {
                eprintln!("[Skills] 解压失败: {}", e);
                continue;
            }

            // 递归扫描 SKILL.md
            let skills = scan_for_skills(&tmp, &repo.owner, &repo.name, &repo.branch);
            result.extend(skills);

            let _ = std::fs::remove_dir_all(&tmp);
        }

        result
    }

    /// 安装一个 skill（从 GitHub）
    pub async fn install_skill(
        data: &mut SkillData,
        skill: &DiscoverableSkill,
    ) -> Result<(), String> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .map_err(|e| e.to_string())?;

        let url = format!(
            "https://github.com/{}/{}/archive/refs/heads/{}.zip",
            skill.repo_owner, skill.repo_name, skill.repo_branch
        );

        let resp = client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("下载失败: {}", e))?;

        if !resp.status().is_success() {
            return Err(format!("下载失败: HTTP {}", resp.status()));
        }

        let bytes = resp.bytes().await.map_err(|e| format!("读取失败: {}", e))?;

        let tmp = std::env::temp_dir().join(format!("codex-skill-install-{}", skill.directory));
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::create_dir_all(&tmp);

        extract_zip(&bytes, &tmp)?;

        // 找到 skill 目录
        let skill_src = find_skill_dir(&tmp, &skill.directory)
            .ok_or_else(|| format!("在仓库中未找到 skill: {}", skill.directory))?;

        // 复制到 SSOT
        let target = ssot_dir().join(&skill.directory);
        if target.exists() {
            let _ = std::fs::remove_dir_all(&target);
        }
        copy_dir_recursive(&skill_src, &target)?;

        // 记录到数据
        let installed = InstalledSkill {
            id: skill.key.clone(),
            name: skill.name.clone(),
            description: skill.description.clone(),
            directory: skill.directory.clone(),
            source: "github".into(),
            repo_owner: Some(skill.repo_owner.clone()),
            repo_name: Some(skill.repo_name.clone()),
            apps: SkillApps::default(),
            installed_at: Utc::now(),
        };

        // 去重
        data.skills.retain(|s| s.directory != skill.directory);
        data.skills.push(installed);

        let _ = std::fs::remove_dir_all(&tmp);
        Ok(())
    }

    /// 卸载 skill
    pub fn uninstall_skill(data: &mut SkillData, skill_id: &str) -> Result<(), String> {
        let skill = data
            .skills
            .iter()
            .find(|s| s.id == skill_id)
            .ok_or_else(|| format!("skill 不存在: {}", skill_id))?
            .clone();

        // 从 SSOT 删除（所有 app 通过 symlink 指向 SSOT，自动同步）
        let ssot_path = ssot_dir().join(&skill.directory);
        if ssot_path.exists() {
            let _ = std::fs::remove_dir_all(&ssot_path);
        }

        // 从数据中移除
        data.skills.retain(|s| s.id != skill_id);

        Ok(())
    }

    /// 确保所有 app 的 symlink 正确
    pub fn sync_all() {
        let _ = init_ssot();
    }
}

// ────────────────────────────────────────────────────────────────
// 辅助函数
// ────────────────────────────────────────────────────────────────

/// 解析 SKILL.md 的 YAML frontmatter
fn parse_skill_md(dir: &std::path::Path) -> (Option<String>, Option<String>) {
    let md_path = dir.join("SKILL.md");
    let content = match std::fs::read_to_string(&md_path) {
        Ok(c) => c,
        Err(_) => return (None, None),
    };

    let mut name = None;
    let mut description = None;
    let mut in_frontmatter = false;

    for line in content.lines() {
        if line.trim() == "---" {
            if in_frontmatter {
                break;
            }
            in_frontmatter = true;
            continue;
        }
        if in_frontmatter {
            if let Some(val) = line.strip_prefix("name:") {
                name = Some(val.trim().to_string());
            } else if let Some(val) = line.strip_prefix("description:") {
                description = Some(val.trim().to_string());
            }
        }
    }

    (name, description)
}

/// 递归扫描目录中的 SKILL.md
fn scan_for_skills(
    root: &std::path::Path,
    owner: &str,
    repo: &str,
    branch: &str,
) -> Vec<DiscoverableSkill> {
    let mut result = Vec::new();

    fn walk(
        dir: &std::path::Path,
        owner: &str,
        repo: &str,
        branch: &str,
        result: &mut Vec<DiscoverableSkill>,
    ) {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let skill_md = path.join("SKILL.md");
                if skill_md.exists() {
                    let (name, description) = parse_skill_md(&path);
                    let dir_name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("")
                        .to_string();

                    if dir_name.is_empty() || dir_name.starts_with('.') {
                        continue;
                    }

                    let key = format!("{}/{}:{}", owner, repo, dir_name);

                    result.push(DiscoverableSkill {
                        key,
                        name: name.unwrap_or_else(|| dir_name.clone()),
                        description: description.unwrap_or_default(),
                        directory: dir_name,
                        repo_owner: owner.to_string(),
                        repo_name: repo.to_string(),
                        repo_branch: branch.to_string(),
                        installed: false,
                    });
                } else {
                    // 继续递归
                    walk(&path, owner, repo, branch, result);
                }
            }
        }
    }

    walk(root, owner, repo, branch, &mut result);
    result
}

/// 解压 ZIP 文件
fn extract_zip(data: &[u8], target: &std::path::Path) -> Result<(), String> {
    use std::io::{Cursor, Read, Write};

    let reader = Cursor::new(data);
    let mut archive = zip::ZipArchive::new(reader).map_err(|e| format!("打开 ZIP 失败: {}", e))?;

    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| format!("读取 ZIP 条目失败: {}", e))?;

        let name = file.name().to_string();
        let out_path = target.join(&name);

        if file.is_dir() {
            let _ = std::fs::create_dir_all(&out_path);
        } else {
            if let Some(parent) = out_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let mut out =
                std::fs::File::create(&out_path).map_err(|e| format!("创建文件失败: {}", e))?;
            let mut buf = Vec::new();
            file.read_to_end(&mut buf)
                .map_err(|e| format!("读取失败: {}", e))?;
            out.write_all(&buf)
                .map_err(|e| format!("写入失败: {}", e))?;
        }
    }

    Ok(())
}

/// 递归复制目录
fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(dst).map_err(|e| format!("创建目录失败: {}", e))?;

    for entry in std::fs::read_dir(src).map_err(|e| format!("读取目录失败: {}", e))? {
        let entry = entry.map_err(|e| format!("读取条目失败: {}", e))?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path).map_err(|e| format!("复制文件失败: {}", e))?;
        }
    }

    Ok(())
}

/// 在解压后的仓库目录中找到特定 skill
fn find_skill_dir(root: &std::path::Path, directory: &str) -> Option<PathBuf> {
    fn walk(dir: &std::path::Path, target: &str) -> Option<PathBuf> {
        for entry in std::fs::read_dir(dir).ok()?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = path.file_name()?.to_str()?;
                if name == target && path.join("SKILL.md").exists() {
                    return Some(path);
                }
                if let Some(found) = walk(&path, target) {
                    return Some(found);
                }
            }
        }
        None
    }

    walk(root, directory)
}

// ────────────────────────────────────────────────────────────────
// Remote 同步（打包/解包）
// ────────────────────────────────────────────────────────────────

fn should_skip_entry(name: &str) -> bool {
    matches!(
        name,
        "__pycache__" | ".DS_Store" | "node_modules" | ".git" | ".venv" | "target"
    )
}

fn should_skip_file(name: &str) -> bool {
    name.ends_with(".pyc") || name.ends_with(".pyo") || name == ".DS_Store"
}

/// 列出 SSOT 目录下的所有 skill 子目录名（不含 SKILL.md 校验，仅目录）
pub fn list_skill_dirs_at(root: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(root) else {
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if should_skip_entry(name) {
            continue;
        }
        out.push(name.to_string());
    }
    out.sort();
    out
}

pub fn list_local_skill_dirs() -> Vec<String> {
    list_skill_dirs_at(&ssot_dir())
}

/// 把 SSOT 下的一个 skill 目录打成 zip，返回字节流。过滤缓存文件。
pub fn zip_skill_dir(name: &str) -> Result<Vec<u8>, String> {
    use std::io::{Cursor, Read, Write};
    use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};

    let root = ssot_dir().join(name);
    if !root.is_dir() {
        return Err(format!("skill 目录不存在: {}", name));
    }

    let buf: Vec<u8> = Vec::new();
    let mut zip = ZipWriter::new(Cursor::new(buf));
    let opts = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(0o644);
    let dir_opts = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Stored)
        .unix_permissions(0o755);

    fn walk(
        dir: &std::path::Path,
        prefix: &std::path::Path,
        zip: &mut ZipWriter<Cursor<Vec<u8>>>,
        opts: &SimpleFileOptions,
        dir_opts: &SimpleFileOptions,
    ) -> Result<(), String> {
        let rd = std::fs::read_dir(dir).map_err(|e| format!("读取目录失败 {:?}: {}", dir, e))?;
        for entry in rd.flatten() {
            let path = entry.path();
            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            if path.is_dir() {
                if should_skip_entry(&name) {
                    continue;
                }
                let rel = path.strip_prefix(prefix).unwrap_or(&path);
                let rel_str = rel.to_string_lossy().replace('\\', "/");
                let entry_name = format!("{}/", rel_str);
                zip.add_directory(entry_name, *dir_opts)
                    .map_err(|e| format!("添加目录失败: {}", e))?;
                walk(&path, prefix, zip, opts, dir_opts)?;
            } else if path.is_file() {
                if should_skip_file(&name) {
                    continue;
                }
                let rel = path.strip_prefix(prefix).unwrap_or(&path);
                let rel_str = rel.to_string_lossy().replace('\\', "/");
                let mut file_opts = *opts;
                if let Ok(meta) = std::fs::metadata(&path) {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        file_opts = file_opts.unix_permissions(meta.permissions().mode() & 0o777);
                    }
                    let _ = meta;
                }
                zip.start_file(rel_str, file_opts)
                    .map_err(|e| format!("开始文件失败: {}", e))?;
                let mut f = std::fs::File::open(&path)
                    .map_err(|e| format!("打开文件失败 {:?}: {}", path, e))?;
                let mut data = Vec::new();
                f.read_to_end(&mut data)
                    .map_err(|e| format!("读取文件失败: {}", e))?;
                zip.write_all(&data)
                    .map_err(|e| format!("写入 zip 失败: {}", e))?;
            }
        }
        Ok(())
    }

    walk(&root, &ssot_dir(), &mut zip, &opts, &dir_opts)?;
    let cursor = zip
        .finish()
        .map_err(|e| format!("finish zip 失败: {}", e))?;
    Ok(cursor.into_inner())
}

/// 从 zip 字节流原子恢复一个 skill 目录到 SSOT。已存在则先备份再覆盖。
pub fn extract_skill_zip(name: &str, bytes: &[u8]) -> Result<(), String> {
    use std::io::{Cursor, Read, Write};
    use zip::ZipArchive;

    if name.is_empty() || name.contains('/') || name.contains('\\') || name == "." || name == ".." {
        return Err(format!("非法 skill 名: {}", name));
    }

    let ssot = ssot_dir();
    std::fs::create_dir_all(&ssot).map_err(|e| format!("创建 SSOT 失败: {}", e))?;

    let target = ssot.join(name);
    let ts = chrono::Utc::now().format("%Y%m%d%H%M%S");
    let staging = ssot.join(format!(".{}.staging.{}", name, ts));
    let backup = ssot.join(format!(".{}.backup.{}", name, ts));

    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| format!("创建 staging 失败: {}", e))?;

    let mut archive =
        ZipArchive::new(Cursor::new(bytes)).map_err(|e| format!("打开 zip 失败: {}", e))?;

    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| format!("读取 zip 条目失败: {}", e))?;
        let raw_name = match entry.enclosed_name() {
            Some(p) => p.to_path_buf(),
            None => return Err(format!("zip 条目名非法: {}", entry.name())),
        };
        // 顶层必须是 <name>/...
        let mut comps = raw_name.components();
        let first = comps
            .next()
            .ok_or_else(|| "zip 条目缺少顶层目录".to_string())?;
        if first.as_os_str() != std::ffi::OsStr::new(name) {
            return Err(format!("zip 顶层目录 {:?} 与期望 {} 不一致", first, name));
        }
        let rel: std::path::PathBuf = comps.collect();
        let dest = staging.join(&rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&dest).map_err(|e| format!("创建目录失败: {}", e))?;
        } else {
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent).map_err(|e| format!("创建父目录失败: {}", e))?;
            }
            let mut out = std::fs::File::create(&dest)
                .map_err(|e| format!("创建文件失败 {:?}: {}", dest, e))?;
            let mut data = Vec::new();
            entry
                .read_to_end(&mut data)
                .map_err(|e| format!("读取 zip 内容失败: {}", e))?;
            out.write_all(&data)
                .map_err(|e| format!("写入文件失败: {}", e))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Some(mode) = entry.unix_mode() {
                    let _ = std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(mode));
                }
            }
        }
    }

    // 原子替换：如果已有 target，先 rename 到 backup，再 rename staging 到 target
    if target.exists() {
        std::fs::rename(&target, &backup).map_err(|e| format!("备份原目录失败: {}", e))?;
    }
    match std::fs::rename(&staging, &target) {
        Ok(_) => {
            let _ = std::fs::remove_dir_all(&backup);
            Ok(())
        }
        Err(e) => {
            // 回滚
            if backup.exists() {
                let _ = std::fs::rename(&backup, &target);
            }
            let _ = std::fs::remove_dir_all(&staging);
            Err(format!("替换 skill 失败: {}", e))
        }
    }
}
