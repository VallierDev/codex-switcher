use std::process::Command;

/// IDE 配置：名称和对应的 Bundle ID
const IDE_CONFIGS: &[(&str, &str)] = &[
    ("Visual Studio Code", "com.microsoft.VSCode"),
    ("Cursor", "com.todesktop.230313mzl4w4u92"),
    ("Windsurf", "com.exafunction.windsurf"),
    ("Antigravity", "com.google.antigravity"),
    ("Codex", "com.openai.codex"), // 暂定，用户确认后修正
];

/// 检测运行中的 IDE
pub fn detect_running_ides() -> Vec<String> {
    if !cfg!(target_os = "macos") {
        return Vec::new();
    }
    let mut running = Vec::new();

    for &(name, bundle_id) in IDE_CONFIGS {
        let script = format!(
            r#"
            tell application "System Events"
                if exists (every application process whose bundle identifier is "{}") then
                    return "true"
                else
                    return "false"
                end if
            end tell
            "#,
            bundle_id
        );

        if let Ok(output) = run_applescript(&script) {
            if output.trim() == "true" {
                running.push(name.to_string());
            }
        }
    }
    running
}

/// 重载指定 IDE
pub fn reload_ide(name: &str, use_window_reload: bool) -> Result<(), String> {
    if !cfg!(target_os = "macos") {
        return Err("IDE 自动重载仅支持 macOS，请手动重载 IDE".into());
    }
    // 杀死所有 codex 进程（排除 Codex Switcher 自身）
    let script = r#"
        for pid in $(pgrep -f codex 2>/dev/null); do
            cmd=$(ps -p "$pid" -o command= 2>/dev/null || true)
            case "$cmd" in
                *codex-switcher*|*Codex\ Switcher*|*codex_switcher*) continue ;;
            esac
            kill -9 "$pid" 2>/dev/null
        done
    "#;
    let output = Command::new("sh").arg("-c").arg(script).output();

    if let Ok(o) = output {
        if o.status.success() {
            println!("已杀死所有 codex 进程");
        }
    }

    // 可选：还可以继续保留原来的 AppleScript 快捷键刷新机制以防万一，或者直接返回
    // 这里保留后续逻辑，让 IDE 也能执行 Reload Window / Restart Extension Host 确保前端视图也刷新
    // 如果用户只想要 pkill，我们可以直接返回 Ok(())。但根据语义 "切换帐号后 如果自动重载 IDE 直接 调用 pkill -9 -f codex"，
    // 我们可以把它作为主要操作。这里保留原有的模拟按键操作，让它更彻底。

    // 优先尝试模拟按键指令
    let bundle_id = IDE_CONFIGS
        .iter()
        .find(|&&(n, _)| n == name)
        .map(|&(_, b)| b)
        .ok_or_else(|| format!("未找到 IDE {} 的配置", name))?;

    let command_text = if use_window_reload {
        "Reload Window"
    } else {
        "Restart Extension Host"
    };

    // AppleScript 脚本：使用 bundle id 激活并发送指令
    let script = format!(
        r#"
        tell application id "{}"
            activate
            delay 0.5
            tell application "System Events"
                keystroke "p" using {{command down, shift down}}
                delay 0.5
                keystroke "{}"
                delay 0.5
                keystroke return
            end tell
        end tell
        "#,
        bundle_id, command_text
    );

    match run_applescript(&script) {
        Ok(_) => Ok(()),
        Err(e) if e.contains("1002") || e.contains("不由自主") || e.contains("不允许发送按键") =>
        {
            // 捕获权限错误，返回一个友好的提示，而不是直接报错
            Err(
                "PERMISSION_DENIED:需要“辅助功能”权限来重载窗口。请手动重载或在设置中授予权限。"
                    .to_string(),
            )
        }
        Err(e) => Err(e),
    }
}

/// 移除 Codex App 的隔离属性 (修复闪退)
pub fn remove_quarantine() -> Result<(), String> {
    if !cfg!(target_os = "macos") {
        return Err("隔离属性修复仅适用于 macOS".into());
    }
    let script = r#"
    do shell script "xattr -dr com.apple.quarantine /Applications/Codex.app" with administrator privileges
    "#;

    run_applescript(script).map(|_| ())
}

/// 执行 AppleScript
fn run_applescript(script: &str) -> Result<String, String> {
    let output = Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output()
        .map_err(|e| format!("无法执行 osascript: {}", e))?;

    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(format!("AppleScript 执行失败: {}", err));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

#[cfg(all(test, not(target_os = "macos")))]
mod tests {
    #[test]
    fn unsupported_actions_return_before_spawning_platform_tools() {
        assert!(super::detect_running_ides().is_empty());
        assert!(super::reload_ide("Visual Studio Code", false).unwrap_err().contains("macOS"));
        assert!(super::remove_quarantine().unwrap_err().contains("macOS"));
    }
}
