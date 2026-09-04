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
    let environment_locale = ["NEXUS_LOCALE", "LC_ALL", "LANG", "LANGUAGE"]
        .into_iter()
        .filter_map(|key| std::env::var(key).ok())
        .find_map(|value| NativeLocale::from_code(&value));
    environment_locale
        .or_else(windows_default_locale)
        .unwrap_or(NativeLocale::English)
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

#[cfg(windows)]
fn windows_default_locale() -> Option<NativeLocale> {
    use windows_sys::Win32::Globalization::GetUserDefaultLocaleName;

    // GetUserDefaultLocaleName includes the terminating NUL in its length.
    let mut buffer = [0u16; 85];
    let length = unsafe { GetUserDefaultLocaleName(buffer.as_mut_ptr(), buffer.len() as i32) };
    if length <= 1 || length as usize > buffer.len() {
        return None;
    }
    String::from_utf16(&buffer[..length as usize - 1])
        .ok()
        .and_then(|value| NativeLocale::from_code(&value))
}

#[cfg(not(windows))]
fn windows_default_locale() -> Option<NativeLocale> {
    None
}

pub const fn text(locale: NativeLocale, key: NativeText) -> &'static str {
    match (locale, key) {
        (NativeLocale::English, NativeText::TrayShow) => "Show launcher",
        (NativeLocale::English, NativeText::TrayQuit) => "Quit",
        (NativeLocale::English, NativeText::TrayTooltip) => "Nexus Launcher",
        (NativeLocale::English, NativeText::MinimizedTitle) => "Nexus Launcher",
        (NativeLocale::English, NativeText::MinimizedBody) => {
            "Launcher is still running in the system tray"
        }
        (NativeLocale::SimplifiedChinese, NativeText::TrayShow) => "显示启动器",
        (NativeLocale::SimplifiedChinese, NativeText::TrayQuit) => "退出",
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
            "退出"
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
    }

    #[test]
    fn active_locale_can_be_updated_by_the_native_bridge() {
        set_active_locale(NativeLocale::SimplifiedChinese);
        assert_eq!(active_locale(), NativeLocale::SimplifiedChinese);
        set_active_locale(NativeLocale::English);
        assert_eq!(active_locale(), NativeLocale::English);
    }
}
