//! Small native-surface dictionary for text that is rendered outside the
//! React webview (tray menu, tooltip, and minimize notification).
//!
//! The webview owns the full user-selectable locale. Native surfaces are
//! initialized before React is available, so they use the explicit
//! `NEXUS_LOCALE` override when present and otherwise follow the process
//! locale. Keeping these strings in one dictionary prevents native UI text
//! from bypassing the launcher's localization boundary.

use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeLocale {
    English,
    SimplifiedChinese,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeText {
    TrayShow,
    TrayQuit,
    TrayStopQuit,
    TrayTooltip,
    MinimizedTitle,
    MinimizedBody,
}

static ACTIVE_LOCALE: OnceLock<Mutex<NativeLocale>> = OnceLock::new();

impl NativeLocale {
    pub fn from_code(value: &str) -> Option<Self> {
        let normalized = value.trim().to_ascii_lowercase();
        if normalized.starts_with("zh") || normalized.contains("chinese") {
            Some(Self::SimplifiedChinese)
        } else if normalized.starts_with("en") || normalized.contains("english") {
            Some(Self::English)
        } else {
            None
        }
    }
}

pub fn detect_locale() -> NativeLocale {
    if let Some(locale) = std::env::var("NEXUS_LOCALE")
        .ok()
        .and_then(|value| NativeLocale::from_code(&value))
    {
        return locale;
    }

    #[cfg(windows)]
    if let Some(locale) = windows_ui_locale() {
        return locale;
    }

    #[cfg(not(windows))]
    if let Some(locale) = ["LC_ALL", "LANG", "LANGUAGE"]
        .into_iter()
        .filter_map(|key| std::env::var(key).ok())
        .find_map(|value| NativeLocale::from_code(&value))
    {
        return locale;
    }

    NativeLocale::English
}

/// Returns the locale currently shared by the webview and native surfaces.
/// The initial value follows the explicit environment override or the host
/// platform's UI culture; the Tauri command updates it when the user changes
/// the language in Settings.
pub fn active_locale() -> NativeLocale {
    ACTIVE_LOCALE
        .get_or_init(|| Mutex::new(detect_locale()))
        .lock()
        .map(|locale| *locale)
        .unwrap_or_else(|_| detect_locale())
}

pub fn set_active_locale(locale: NativeLocale) {
    if let Ok(mut current) = ACTIVE_LOCALE
        .get_or_init(|| Mutex::new(detect_locale()))
        .lock()
    {
        *current = locale;
    }
}

/// Maps a Windows LANGID to the small locale set supported by the Launcher.
/// The low ten bits contain the primary language, so region-specific values
/// such as zh-CN (0x0804) and en-US (0x0409) remain stable inputs for tests.
pub fn locale_from_windows_langid(lang_id: u16) -> Option<NativeLocale> {
    match lang_id & 0x03ff {
        0x0004 => Some(NativeLocale::SimplifiedChinese),
        0x0009 => Some(NativeLocale::English),
        _ => None,
    }
}

#[cfg(windows)]
fn windows_ui_locale() -> Option<NativeLocale> {
    use windows_sys::Win32::Globalization::GetUserDefaultUILanguage;

    locale_from_windows_langid(unsafe { GetUserDefaultUILanguage() })
}

pub const fn text(locale: NativeLocale, key: NativeText) -> &'static str {
    match (locale, key) {
        (NativeLocale::English, NativeText::TrayShow) => "Show launcher",
        (NativeLocale::English, NativeText::TrayQuit) => "Exit launcher (keep services running)",
        (NativeLocale::English, NativeText::TrayStopQuit) => "Stop services and exit",
        (NativeLocale::English, NativeText::TrayTooltip) => "Nexus Launcher",
        (NativeLocale::English, NativeText::MinimizedTitle) => "Nexus Launcher",
        (NativeLocale::English, NativeText::MinimizedBody) => {
            "Launcher is still running in the system tray"
        }
        (NativeLocale::SimplifiedChinese, NativeText::TrayShow) => "显示启动器",
        (NativeLocale::SimplifiedChinese, NativeText::TrayQuit) => "退出启动器（服务继续运行）",
        (NativeLocale::SimplifiedChinese, NativeText::TrayStopQuit) => "停止服务并退出",
        (NativeLocale::SimplifiedChinese, NativeText::TrayTooltip) => "Nexus Launcher",
        (NativeLocale::SimplifiedChinese, NativeText::MinimizedTitle) => "Nexus Launcher",
        (NativeLocale::SimplifiedChinese, NativeText::MinimizedBody) => "启动器仍在系统托盘中运行",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_dictionary_has_both_locales() {
        assert_eq!(
            text(NativeLocale::English, NativeText::TrayShow),
            "Show launcher"
        );
        assert_eq!(
            text(NativeLocale::SimplifiedChinese, NativeText::TrayShow),
            "显示启动器"
        );
        assert_eq!(
            text(NativeLocale::SimplifiedChinese, NativeText::TrayQuit),
            "退出启动器（服务继续运行）"
        );
    }

    #[test]
    fn locale_codes_are_normalized_for_webview_sync_and_platform_detection() {
        assert_eq!(
            NativeLocale::from_code("zh-CN"),
            Some(NativeLocale::SimplifiedChinese)
        );
        assert_eq!(
            NativeLocale::from_code("en-US"),
            Some(NativeLocale::English)
        );
        assert_eq!(NativeLocale::from_code("C.UTF-8"), None);
        assert_eq!(
            locale_from_windows_langid(0x0804),
            Some(NativeLocale::SimplifiedChinese)
        );
        assert_eq!(
            locale_from_windows_langid(0x0409),
            Some(NativeLocale::English)
        );
        assert_eq!(locale_from_windows_langid(0x0407), None);
    }

    #[test]
    fn active_locale_can_be_updated_by_the_native_bridge() {
        set_active_locale(NativeLocale::SimplifiedChinese);
        assert_eq!(active_locale(), NativeLocale::SimplifiedChinese);
        set_active_locale(NativeLocale::English);
        assert_eq!(active_locale(), NativeLocale::English);
    }
}
