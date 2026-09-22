use super::{BackendLocale, APP_NAME};

pub struct RussianLocale;

impl BackendLocale for RussianLocale {
    fn oauth_success_html(&self) -> &'static str {
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\r\n\
         <html lang=\"ru\"><body><h1>Авторизация выполнена</h1>\
         <p>OpenAI успешно подключён. Можно закрыть это окно и вернуться в приложение.</p>\
         <script>setTimeout(() => window.close(), 3000)</script></body></html>"
    }

    fn oauth_failure_response(&self) -> &'static str {
        "HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n\
         Авторизация не выполнена: проверка state не пройдена или отсутствуют параметры"
    }

    fn tray_tooltip_logged_out(&self) -> String {
        format!("{APP_NAME} - вход не выполнен")
    }

    fn tray_tooltip_account(&self, name: &str, five_hour_left: f64, weekly_left: f64) -> String {
        format!("{APP_NAME} - {name} | 5ч: {five_hour_left:.0}%  неделя: {weekly_left:.0}%")
    }

    fn tray_show_main(&self) -> &'static str {
        "Открыть главное окно"
    }

    fn tray_next_account(&self) -> &'static str {
        "Переключиться на следующий аккаунт"
    }

    fn tray_quit(&self) -> &'static str {
        "Выйти"
    }

    fn notification_account_banned_subtitle(&self) -> &'static str {
        "Аккаунт заблокирован"
    }

    fn notification_auto_switch_subtitle(&self) -> &'static str {
        "Автопереключение"
    }

    fn injected_switch_message(&self, account_name: &str) -> String {
        format!("⚡ [Codex Switcher] Переключено на {account_name}")
    }

    fn referral_unsupported_program(&self) -> &'static str {
        "Неподдерживаемый тип пригласительной акции"
    }

    fn referral_uncertain_send_suffix(&self) -> &'static str {
        "; результат отправки не подтверждён. Сначала проверьте историю приглашений и не отправляйте повторно вслепую"
    }

    fn referral_network_error(&self, error: &reqwest::Error, uncertain: &str) -> String {
        format!("Сетевая ошибка API приглашений: {error}{uncertain}")
    }

    fn referral_non_json_response(&self, status: u16, uncertain: &str) -> String {
        format!(
            "API приглашений вернул ответ HTTP {status} не в формате JSON. Возможно, требуется сеанс входа официального приложения; подтвердить право на участие не удалось{uncertain}"
        )
    }

    fn referral_upstream_rejected(&self) -> &'static str {
        "Запрос отклонён upstream"
    }

    fn referral_failed_emails(&self, emails: &serde_json::Value) -> String {
        format!("; email: {emails}")
    }

    fn referral_unrecognized_response(&self, uncertain: &str) -> String {
        format!("Не удалось распознать ответ API приглашений{uncertain}")
    }

    fn referral_missing_items(&self) -> &'static str {
        "В истории приглашений отсутствует поле items; подтвердить записи не удалось"
    }

    fn referral_no_available_campaign(&self) -> &'static str {
        "Для текущего аккаунта нет доступной пригласительной акции; обновите сведения о праве на участие"
    }
}
