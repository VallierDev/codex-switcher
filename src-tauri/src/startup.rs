use serde::Serialize;
#[cfg(target_os = "linux")]
use tauri::Manager;
mod launch;
pub use launch::{is_autostart_launch, should_show_existing_window, AUTOSTART_ARG};
#[cfg(target_os = "linux")]
mod xdg;

pub fn configure_initial_window(context: &mut tauri::Context<tauri::Wry>) {
    #[cfg(target_os = "linux")]
    if is_autostart_launch(std::env::args_os()) {
        // Set visibility before Tauri creates the window, avoiding a startup flash.
        for window in &mut context.config_mut().app.windows {
            if window.label == "main" {
                window.visible = false;
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = context;
}

#[derive(Serialize)]
pub struct AutostartStatus {
    supported: bool,
    enabled: bool,
}

#[cfg(target_os = "linux")]
fn entry_path() -> Result<std::path::PathBuf, String> {
    xdg::entry_path(std::env::var_os("XDG_CONFIG_HOME"), dirs::home_dir())
        .map_err(|error| format!("读取自动启动设置失败：{}", error))
}

#[tauri::command]
pub fn get_autostart_status(app: tauri::AppHandle) -> Result<AutostartStatus, String> {
    #[cfg(target_os = "linux")]
    {
        // Development binaries depend on the dev server and must not be registered.
        if cfg!(debug_assertions) {
            return Ok(AutostartStatus {
                supported: false,
                enabled: false,
            });
        }
        let _ = app;
        let enabled = xdg::is_enabled(&entry_path()?)
            .map_err(|error| format!("读取自动启动设置失败：{}", error))?;
        Ok(AutostartStatus {
            supported: true,
            enabled,
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = app;
        Ok(AutostartStatus {
            supported: false,
            enabled: false,
        })
    }
}

#[tauri::command]
pub fn set_autostart_enabled(
    app: tauri::AppHandle,
    enabled: bool,
) -> Result<AutostartStatus, String> {
    #[cfg(target_os = "linux")]
    {
        if cfg!(debug_assertions) {
            return Err("开发构建不支持自动启动，请使用已安装的正式构建".to_string());
        }
        let executable = app.env().appimage.map(std::path::PathBuf::from).unwrap_or(
            std::env::current_exe().map_err(|error| format!("更改自动启动设置失败：{}", error))?,
        );
        xdg::set_enabled(&entry_path()?, &executable, AUTOSTART_ARG, enabled)
            .map_err(|error| format!("更改自动启动设置失败：{}", error))?;
        get_autostart_status(app.clone())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (app, enabled);
        Err("当前平台不支持此自动启动设置".to_string())
    }
}
