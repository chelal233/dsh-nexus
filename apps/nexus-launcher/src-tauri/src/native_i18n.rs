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
    HarnessRunning,
    HarnessStopped,
    HarnessStarting,
    HarnessStopping,
    HarnessFailed,
    HarnessUnknown,
    HarnessStart,
    HarnessStop,
    HarnessWeb,
    DshTerminal,

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
        if ["zh-tw", "zh-hk", "zh-mo", "zh-hant", "zh_tw", "zh_hk", "zh_mo"].iter().any(|prefix| normalized.starts_with(prefix)) || normalized.contains("traditional") {
            Some(Self::English)
        } else if normalized.starts_with("zh") || normalized.contains("chinese") {
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
/// Chinese sublanguages are distinct: unsupported Traditional Chinese falls
/// back to English instead of silently selecting Simplified Chinese.
pub fn locale_from_windows_langid(lang_id: u16) -> Option<NativeLocale> {
    match lang_id {
        0x0804 | 0x1004 => Some(NativeLocale::SimplifiedChinese),
        0x0404 | 0x0c04 | 0x1404 => Some(NativeLocale::English),
        value if value & 0x03ff == 0x0009 => Some(NativeLocale::English),
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
        (NativeLocale::English, NativeText::HarnessRunning) => "Harness: running",
        (NativeLocale::SimplifiedChinese, NativeText::HarnessRunning) => "Harness：运行中",
        (NativeLocale::English, NativeText::HarnessStopped) => "Harness: stopped",
        (NativeLocale::SimplifiedChinese, NativeText::HarnessStopped) => "Harness：已停止",
        (NativeLocale::English, NativeText::HarnessStarting) => "Harness: starting",
        (NativeLocale::SimplifiedChinese, NativeText::HarnessStarting) => "Harness：启动中",
        (NativeLocale::English, NativeText::HarnessStopping) => "Harness: stopping",
        (NativeLocale::SimplifiedChinese, NativeText::HarnessStopping) => "Harness：停止中",
        (NativeLocale::English, NativeText::HarnessFailed) => "Harness: failed",
        (NativeLocale::SimplifiedChinese, NativeText::HarnessFailed) => "Harness：运行失败",
        (NativeLocale::English, NativeText::HarnessUnknown) => "Harness: status unavailable",
        (NativeLocale::SimplifiedChinese, NativeText::HarnessUnknown) => "Harness：状态不可用",
        (NativeLocale::English, NativeText::HarnessStart) => "Start Harness",
        (NativeLocale::SimplifiedChinese, NativeText::HarnessStart) => "启动 Harness",
        (NativeLocale::English, NativeText::HarnessStop) => "Stop Harness",
        (NativeLocale::SimplifiedChinese, NativeText::HarnessStop) => "停止 Harness",
        (NativeLocale::English, NativeText::HarnessWeb) => "Open Harness Web",
        (NativeLocale::SimplifiedChinese, NativeText::HarnessWeb) => "打开 Harness Web",
        (NativeLocale::English, NativeText::DshTerminal) => "Open DSH terminal",
        (NativeLocale::SimplifiedChinese, NativeText::DshTerminal) => "打开 DSH 终端",

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
        for code in ["zh-TW","zh-HK","zh-MO","zh-Hant","zh_HK"] { assert_eq!(NativeLocale::from_code(code),Some(NativeLocale::English)); }
        for id in [0x0404,0x0c04,0x1404] { assert_eq!(locale_from_windows_langid(id),Some(NativeLocale::English)); }
        assert_eq!(locale_from_windows_langid(0x1004),Some(NativeLocale::SimplifiedChinese));
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
