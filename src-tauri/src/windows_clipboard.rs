use std::process::Stdio;
use tokio::{
    io::AsyncWriteExt,
    process::Command,
    time::{timeout, Duration},
};

fn encode_text(text: &str) -> Vec<u8> {
    // ASCII stdin avoids codepage conversion; user text is never PowerShell syntax.
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .encode(text.as_bytes())
        .into_bytes()
}

pub async fn write_text(text: &str) -> Result<(), String> {
    let root = std::env::var_os("SystemRoot").ok_or("无法定位 Windows 系统目录")?;
    let mut command = Command::new(
        std::path::PathBuf::from(root).join("System32/WindowsPowerShell/v1.0/powershell.exe"),
    );
    command.args(["-NoLogo", "-NoProfile", "-NonInteractive", "-STA", "-Command",
        "$ErrorActionPreference='Stop'; $text=[Text.Encoding]::UTF8.GetString([Convert]::FromBase64String([Console]::In.ReadToEnd())); Add-Type -AssemblyName System.Windows.Forms; if ($text.Length -eq 0) { [Windows.Forms.Clipboard]::Clear() } else { [Windows.Forms.Clipboard]::SetText($text, [Windows.Forms.TextDataFormat]::UnicodeText) }"]);
    command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("无法启动剪贴板工具: {}", e))?;
    let result = timeout(Duration::from_secs(20), async {
        let mut stdin = child.stdin.take().ok_or("剪贴板输入不可写")?;
        stdin
            .write_all(&encode_text(text))
            .await
            .map_err(|e| format!("写入剪贴板失败: {}", e))?;
        drop(stdin);
        let status = child
            .wait()
            .await
            .map_err(|e| format!("等待剪贴板工具失败: {}", e))?;
        if !status.success() {
            return Err(format!("剪贴板工具退出失败: {}", status));
        }
        Ok(())
    })
    .await;
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => {
            let _ = child.kill().await;
            Err(error)
        }
        Err(_) => {
            let _ = child.kill().await;
            Err("剪贴板写入超时，请手动复制".into())
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn transport_preserves_unicode_and_metacharacters() {
        use base64::Engine;
        for text in ["한글 😀\r\n'$();<>|!&", "\u{feff}intentional BOM", ""] {
            let encoded = super::encode_text(text);
            assert!(encoded.is_ascii());
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .unwrap();
            assert_eq!(decoded, text.as_bytes());
        }
    }

    #[tokio::test]
    #[ignore = "Writes the OS clipboard; disposable GitHub Actions runner only"]
    async fn disposable_windows_clipboard_roundtrip() {
        assert_eq!(
            std::env::var("CODEX_SWITCHER_DISPOSABLE_TESTS").as_deref(),
            Ok("1")
        );
        assert_eq!(std::env::var("GITHUB_ACTIONS").as_deref(), Ok("true"));
        let long_url = format!(
            "https://example.invalid/authorize?state={}&extra=%25%26%22%3C",
            "a".repeat(1800)
        );
        for text in [
            "한글 😀 https://example.invalid/?code=a&state=b%20c\r\n'$();<>|!",
            "\u{feff}intentional BOM",
            long_url.as_str(),
            "",
        ] {
            super::write_text(text).await.unwrap();
            let root = std::env::var_os("SystemRoot").unwrap();
            let output = tokio::process::Command::new(std::path::PathBuf::from(root)
                .join("System32/WindowsPowerShell/v1.0/powershell.exe"))
                .args(["-NoProfile", "-NonInteractive", "-STA", "-Command",
                    "[Console]::OutputEncoding=[Text.UTF8Encoding]::new($false); [Console]::Write((Get-Clipboard -Raw))"])
                .output().await.unwrap();
            assert!(output.status.success());
            assert_eq!(String::from_utf8(output.stdout).unwrap(), text);
        }
    }
}
