//! Small native-surface dictionary for text that is rendered outside the
//! React webview (tray menu, tooltip, and minimize notification).
//!
//! The webview owns the full user-selectable locale. Native surfaces are
//! initialized before React is available, so they use the explicit
//! `NEXUS_LOCALE` override when present and otherwise follow the process
//! locale. Keeping these strings in one dictionary prevents native UI text
//! from bypassing the launcher's localization boundary.

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

pub fn detect_locale() -> NativeLocale {
    ["NEXUS_LOCALE", "LC_ALL", "LANG", "LANGUAGE"]
        .into_iter()
        .filter_map(|key| std::env::var(key).ok())
        .find_map(|value| {
            let normalized = value.to_ascii_lowercase();
            if normalized.starts_with("zh") || normalized.contains("chinese") {
                Some(NativeLocale::SimplifiedChinese)
            } else if normalized.starts_with("en") || normalized.contains("english") {
                Some(NativeLocale::English)
            } else {
                None
            }
        })
        .unwrap_or(NativeLocale::English)
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
        (NativeLocale::SimplifiedChinese, NativeText::MinimizedBody) => {
            "启动器仍在系统托盘中运行"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_dictionary_has_both_locales() {
        assert_eq!(text(NativeLocale::English, NativeText::TrayShow), "Show launcher");
        assert_eq!(text(NativeLocale::SimplifiedChinese, NativeText::TrayShow), "显示启动器");
        assert_eq!(text(NativeLocale::SimplifiedChinese, NativeText::TrayQuit), "退出");
    }
}
