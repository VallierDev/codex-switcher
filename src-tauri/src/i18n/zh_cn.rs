use super::{BackendLocale, APP_NAME};

pub struct ChineseLocale;

impl BackendLocale for ChineseLocale {
    fn oauth_success_html(&self) -> &'static str {
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\r\n\
         <html lang=\"zh-CN\"><body><h1>授权成功</h1><p>已成功连接 OpenAI，你可以关闭此窗口并回到应用。</p>\
         <script>setTimeout(() => window.close(), 3000)</script></body></html>"
    }

    fn oauth_failure_response(&self) -> &'static str {
        "HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n授权失败: State 校验不通过或参数缺失"
    }

    fn tray_tooltip_logged_out(&self) -> String {
        format!("{APP_NAME} - 未登录")
    }

    fn tray_tooltip_account(&self, name: &str, five_hour_left: f64, weekly_left: f64) -> String {
        format!("{APP_NAME} - {name} | 5H: {five_hour_left:.0}%  周: {weekly_left:.0}%")
    }

    fn tray_show_main(&self) -> &'static str {
        "打开主窗口"
    }

    fn tray_next_account(&self) -> &'static str {
        "切换到下一个账号"
    }

    fn tray_quit(&self) -> &'static str {
        "退出"
    }

    fn notification_account_banned_subtitle(&self) -> &'static str {
        "检测到封号"
    }

    fn notification_auto_switch_subtitle(&self) -> &'static str {
        "自动切号"
    }

    fn injected_switch_message(&self, account_name: &str) -> String {
        format!("⚡ [Codex Switcher] 已切换到 {account_name}")
    }

    fn referral_unsupported_program(&self) -> &'static str {
        "不支持的邀请活动类型"
    }

    fn referral_uncertain_send_suffix(&self) -> &'static str {
        "；发送结果未确认，请先查询邀请记录，勿直接重发"
    }

    fn referral_network_error(&self, error: &reqwest::Error, uncertain: &str) -> String {
        format!("邀请接口网络错误：{error}{uncertain}")
    }

    fn referral_non_json_response(&self, status: u16, uncertain: &str) -> String {
        format!(
            "邀请接口返回 HTTP {status} 非 JSON 响应，可能需要官方桌面版登录会话；无法确认活动资格{uncertain}"
        )
    }

    fn referral_upstream_rejected(&self) -> &'static str {
        "请求被上游拒绝"
    }

    fn referral_failed_emails(&self, emails: &serde_json::Value) -> String {
        format!("；邮箱：{emails}")
    }

    fn referral_unrecognized_response(&self, uncertain: &str) -> String {
        format!("邀请接口响应格式无法识别{uncertain}")
    }

    fn referral_missing_items(&self) -> &'static str {
        "邀请记录响应缺少 items，无法确认记录"
    }

    fn referral_no_available_campaign(&self) -> &'static str {
        "当前账号没有可发送的邀请活动，请刷新资格"
    }
}
