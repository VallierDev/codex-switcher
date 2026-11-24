use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const FILE_NAME: &str = "com.codex.switcher.desktop";
const OWNER_KEY: &str = "X-Codex-Switcher-Autostart";

pub fn entry_path(config_home: Option<OsString>, home: Option<PathBuf>) -> io::Result<PathBuf> {
    let config = config_home
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| home.map(|path| path.join(".config")))
        .ok_or_else(|| io::Error::other("Cannot resolve the user configuration directory"))?;
    Ok(config.join("autostart").join(FILE_NAME))
}

// Read only our own small desktop entry; never replace a foreign file or symlink.
fn read_entry(path: &Path) -> io::Result<Option<BTreeMap<String, String>>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_file() || metadata.len() > 64 * 1024 {
        return Err(io::Error::other(
            "Autostart entry is not a regular bounded file",
        ));
    }
    let mut fields = BTreeMap::new();
    let mut desktop_entry = false;
    for line in fs::read_to_string(path)?.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            desktop_entry = line == "[Desktop Entry]";
        } else if desktop_entry && !line.starts_with('#') {
            if let Some((key, value)) = line.split_once('=') {
                fields.insert(key.trim().to_owned(), value.trim().to_owned());
            }
        }
    }
    if fields.get(OWNER_KEY).map(String::as_str) != Some("true") {
        return Err(io::Error::other(
            "Autostart entry belongs to another launcher",
        ));
    }
    Ok(Some(fields))
}

pub fn is_enabled(path: &Path) -> io::Result<bool> {
    Ok(read_entry(path)?.is_some_and(|fields| {
        fields.get("Hidden").map(String::as_str) != Some("true")
            && fields.get("X-GNOME-Autostart-enabled").map(String::as_str) != Some("false")
    }))
}

pub fn desktop_entry(executable: &Path, arg: &str, enabled: bool) -> io::Result<String> {
    let executable = executable
        .to_str()
        .filter(|_| executable.is_absolute())
        .ok_or_else(|| io::Error::other("The autostart executable needs an absolute UTF-8 path"))?;
    if executable.chars().any(|ch| matches!(ch, '\n' | '\r' | '=')) || arg != "--autostart" {
        return Err(io::Error::other(
            "The executable path cannot be represented in a desktop entry",
        ));
    }
    // Desktop Entry string escaping is applied before Exec quoting and field codes.
    // Use env without a shell: desktop launchers may validate argv[0] before
    // expanding %% in an AppImage filename. The absolute target is one argument.
    let mut quoted = String::from("env -- \"");
    for ch in executable.chars() {
        if matches!(ch, '"' | '`' | '$' | '\\') {
            quoted.push('\\');
        }
        if ch == '%' {
            quoted.push('%');
        }
        quoted.push(ch);
    }
    quoted.push('"');
    let exec = quoted.replace('\\', "\\\\").replace('\t', "\\t");
    Ok(format!(
        "[Desktop Entry]\nType=Application\nVersion=1.0\nName=Codex Switcher\nExec={exec} {arg}\nTerminal=false\nStartupNotify=false\nHidden={}\n{OWNER_KEY}=true\n",
        !enabled,
    ))
}

pub fn set_enabled(path: &Path, executable: &Path, arg: &str, enabled: bool) -> io::Result<()> {
    let metadata = fs::metadata(executable)?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err(io::Error::other(
            "The autostart target is not an executable file",
        ));
    }
    read_entry(path)?;
    let content = desktop_entry(executable, arg, enabled)?;
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("Invalid autostart directory"))?;
    fs::create_dir_all(parent)?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = parent.join(format!(
        ".codex-switcher-{}-{nonce}.tmp",
        std::process::id()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    let result = (|| {
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        // Recheck ownership immediately before atomically replacing the entry.
        read_entry(path)?;
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir()
                .join(format!("switcher-autostart-{}-{nonce}", std::process::id()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn entry(&self) -> PathBuf {
            self.0.join("config/autostart").join(FILE_NAME)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn respects_xdg_config_home_and_ignores_relative_values() {
        let home = Some(PathBuf::from("/users/example"));
        assert_eq!(
            entry_path(Some("/custom/settings".into()), home.clone()).unwrap(),
            PathBuf::from("/custom/settings/autostart").join(FILE_NAME)
        );
        for value in [None, Some("".into()), Some("relative".into())] {
            assert_eq!(
                entry_path(value, home.clone()).unwrap(),
                PathBuf::from("/users/example/.config/autostart").join(FILE_NAME)
            );
        }
        assert!(entry_path(None, None).is_err());
    }

    #[test]
    fn quotes_executable_paths_without_shell_expansion_or_field_substitution() {
        let content = desktop_entry(
            Path::new(r#"/apps/Codex $x `x` \"quote\" 50%.AppImage"#),
            "--autostart",
            true,
        )
        .unwrap();
        assert!(content.contains("\\\\$x"));
        assert!(content.contains("\\\\`x\\\\`"));
        assert!(content.contains("50%%.AppImage\" --autostart"));
        assert!(desktop_entry(Path::new("relative"), "--autostart", true).is_err());
        assert!(desktop_entry(Path::new("/apps/invalid\nname"), "--autostart", true).is_err());
    }

    #[test]
    fn enabling_disabling_and_reenabling_only_changes_the_registration() {
        let fixture = Fixture::new();
        let entry = fixture.entry();
        let executable = std::env::current_exe().unwrap();
        assert!(!is_enabled(&entry).unwrap());
        for enabled in [true, false, true, false] {
            set_enabled(&entry, &executable, "--autostart", enabled).unwrap();
            assert_eq!(is_enabled(&entry).unwrap(), enabled);
            assert!(fs::read_to_string(&entry)
                .unwrap()
                .contains(&format!("Hidden={}", !enabled)));
            assert_eq!(
                fs::metadata(&entry).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        // Hidden=true also masks a same-name system-wide entry, without touching it.
        assert!(entry.exists());
        assert_eq!(fs::read_dir(entry.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn foreign_files_and_symlinks_are_never_replaced() {
        let fixture = Fixture::new();
        let entry = fixture.entry();
        fs::create_dir_all(entry.parent().unwrap()).unwrap();
        fs::write(&entry, "[Desktop Entry]\nName=Other\n").unwrap();
        let executable = std::env::current_exe().unwrap();
        assert!(set_enabled(&entry, &executable, "--autostart", true).is_err());
        assert_eq!(
            fs::read_to_string(&entry).unwrap(),
            "[Desktop Entry]\nName=Other\n"
        );
        fs::remove_file(&entry).unwrap();
        let other = fixture.0.join("other");
        fs::write(&other, "untouched").unwrap();
        std::os::unix::fs::symlink(&other, &entry).unwrap();
        assert!(set_enabled(&entry, &executable, "--autostart", true).is_err());
        assert_eq!(fs::read_to_string(other).unwrap(), "untouched");
    }

    #[test]
    fn external_desktop_disable_is_reflected_and_invalid_target_does_not_overwrite() {
        let fixture = Fixture::new();
        let entry = fixture.entry();
        let executable = std::env::current_exe().unwrap();
        set_enabled(&entry, &executable, "--autostart", true).unwrap();
        let content = fs::read_to_string(&entry).unwrap() + "X-GNOME-Autostart-enabled=false\n";
        fs::write(&entry, &content).unwrap();
        assert!(!is_enabled(&entry).unwrap());
        assert!(set_enabled(
            &entry,
            Path::new("/nonexistent/switcher"),
            "--autostart",
            true
        )
        .is_err());
        assert_eq!(fs::read_to_string(entry).unwrap(), content);
    }
}
