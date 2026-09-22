mod ru;
#[allow(dead_code)]
mod zh_cn;

use ru::RussianLocale;
use std::sync::atomic::{AtomicU8, Ordering};
use zh_cn::ChineseLocale;

pub const APP_NAME: &str = "Codex Switcher";

trait BackendLocale {
    fn oauth_success_html(&self) -> &'static str;
    fn oauth_failure_response(&self) -> &'static str;
    fn tray_tooltip_logged_out(&self) -> String;
    fn tray_tooltip_account(&self, name: &str, five_hour_left: f64, weekly_left: f64) -> String;
    fn tray_show_main(&self) -> &'static str;
    fn tray_next_account(&self) -> &'static str;
    fn tray_quit(&self) -> &'static str;
    fn notification_account_banned_subtitle(&self) -> &'static str;
    fn notification_auto_switch_subtitle(&self) -> &'static str;
    fn injected_switch_message(&self, account_name: &str) -> String;
    fn referral_unsupported_program(&self) -> &'static str;
    fn referral_uncertain_send_suffix(&self) -> &'static str;
    fn referral_network_error(&self, error: &reqwest::Error, uncertain: &str) -> String;
    fn referral_non_json_response(&self, status: u16, uncertain: &str) -> String;
    fn referral_upstream_rejected(&self) -> &'static str;
    fn referral_failed_emails(&self, emails: &serde_json::Value) -> String;
    fn referral_unrecognized_response(&self, uncertain: &str) -> String;
    fn referral_missing_items(&self) -> &'static str;
    fn referral_no_available_campaign(&self) -> &'static str;
}

const RUSSIAN_LOCALE: RussianLocale = RussianLocale;
const CHINESE_LOCALE: ChineseLocale = ChineseLocale;
const LOCALE_AUTO: u8 = 0;
const LOCALE_ZH_CN: u8 = 1;
const LOCALE_RU: u8 = 2;
static ACTIVE_LOCALE: AtomicU8 = AtomicU8::new(LOCALE_AUTO);

fn active_locale() -> &'static dyn BackendLocale {
    match ACTIVE_LOCALE.load(Ordering::Relaxed) {
        LOCALE_RU => &RUSSIAN_LOCALE,
        LOCALE_ZH_CN => &CHINESE_LOCALE,
        _ => {
            let language = std::env::var("LANG").unwrap_or_default().to_lowercase();
            if language.starts_with("ru") {
                &RUSSIAN_LOCALE
            } else {
                &CHINESE_LOCALE
            }
        }
    }
}

#[tauri::command]
pub fn set_app_locale(app: tauri::AppHandle, locale: String) -> Result<(), String> {
    let value = match locale.as_str() {
        "ru" => LOCALE_RU,
        "zh-CN" => LOCALE_ZH_CN,
        _ => return Err(format!("Unsupported application locale: {locale}")),
    };
    ACTIVE_LOCALE.store(value, Ordering::Relaxed);
    crate::tray::update_tray_native_menu(&app).map_err(|error| error.to_string())?;
    crate::tray::update_tray_menu(&app);
    Ok(())
}

fn is_generic_locale(locale: &str) -> bool {
    matches!(
        locale.trim().to_ascii_lowercase().as_str(),
        "c" | "c.utf-8" | "posix"
    )
}

fn locale_from_environment() -> Option<String> {
    ["LC_ALL", "LC_MESSAGES", "LANGUAGE", "LANG"]
        .into_iter()
        .filter_map(|name| std::env::var(name).ok())
        .map(|value| value.trim().to_string())
        .find(|value| !value.is_empty() && !is_generic_locale(value))
}

#[cfg(target_os = "linux")]
fn locale_from_system_file() -> Option<String> {
    let contents = std::fs::read_to_string("/etc/default/locale").ok()?;
    contents.lines().find_map(|line| {
        let (name, value) = line.split_once('=')?;
        if name.trim() != "LANG" {
            return None;
        }
        let value = value.trim().trim_matches(['"', '\'']);
        (!value.is_empty() && !is_generic_locale(value)).then(|| value.to_string())
    })
}

#[cfg(target_os = "macos")]
fn locale_from_system_file() -> Option<String> {
    let output = std::process::Command::new("defaults")
        .args(["read", "-g", "AppleLocale"])
        .output()
        .ok()?;
    let value = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!value.is_empty() && !is_generic_locale(&value)).then_some(value)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn locale_from_system_file() -> Option<String> {
    None
}

#[tauri::command]
pub fn get_system_locale() -> Option<String> {
    locale_from_environment().or_else(locale_from_system_file)
}

pub fn oauth_success_html() -> &'static str {
    active_locale().oauth_success_html()
}

pub fn oauth_failure_response() -> &'static str {
    active_locale().oauth_failure_response()
}

pub fn tray_tooltip_logged_out() -> String {
    active_locale().tray_tooltip_logged_out()
}

pub fn tray_tooltip_account(name: &str, five_hour_left: f64, weekly_left: f64) -> String {
    active_locale().tray_tooltip_account(name, five_hour_left, weekly_left)
}

pub fn tray_tooltip_default() -> String {
    APP_NAME.to_string()
}

pub fn tray_show_main() -> &'static str {
    active_locale().tray_show_main()
}

pub fn tray_next_account() -> &'static str {
    active_locale().tray_next_account()
}

pub fn tray_quit() -> &'static str {
    active_locale().tray_quit()
}

pub fn notification_account_banned_subtitle() -> &'static str {
    active_locale().notification_account_banned_subtitle()
}

pub fn notification_auto_switch_subtitle() -> &'static str {
    active_locale().notification_auto_switch_subtitle()
}

pub fn injected_switch_message(account_name: &str) -> String {
    active_locale().injected_switch_message(account_name)
}

pub fn referral_unsupported_program() -> &'static str {
    active_locale().referral_unsupported_program()
}

pub fn referral_uncertain_send_suffix() -> &'static str {
    active_locale().referral_uncertain_send_suffix()
}

pub fn referral_network_error(error: &reqwest::Error, uncertain: &str) -> String {
    active_locale().referral_network_error(error, uncertain)
}

pub fn referral_non_json_response(status: u16, uncertain: &str) -> String {
    active_locale().referral_non_json_response(status, uncertain)
}

pub fn referral_upstream_rejected() -> &'static str {
    active_locale().referral_upstream_rejected()
}

pub fn referral_failed_emails(emails: &serde_json::Value) -> String {
    active_locale().referral_failed_emails(emails)
}

pub fn referral_unrecognized_response(uncertain: &str) -> String {
    active_locale().referral_unrecognized_response(uncertain)
}

pub fn referral_missing_items() -> &'static str {
    active_locale().referral_missing_items()
}

pub fn referral_no_available_campaign() -> &'static str {
    active_locale().referral_no_available_campaign()
}
