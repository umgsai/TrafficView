//! 中英文静态翻译表与语言解析。
//!
//! 所有 GUI 用户可见文本集中在此；新增文本必须同时补充 ZH/EN 两条。

/// 界面语言。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Zh,
    En,
}

/// 读取 LC_ALL/LANG 判定语言；未设置或非中文环境均为英文。
pub fn detect() -> Language {
    let locale = std::env::var("LC_ALL")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| std::env::var("LANG").ok());
    detect_from(locale.as_deref())
}

/// 纯函数版检测，便于测试：`zh` 开头为中文，其余（含 C/POSIX）为英文。
pub fn detect_from(locale: Option<&str>) -> Language {
    match locale {
        Some(value) if value.to_ascii_lowercase().starts_with("zh") => Language::Zh,
        _ => Language::En,
    }
}

/// 配置为 zh/en 时优先；auto、空串与未知值回退到系统检测。
pub fn resolve(configured: &str) -> Language {
    match configured.trim().to_ascii_lowercase().as_str() {
        "zh" => Language::Zh,
        "en" => Language::En,
        _ => detect(),
    }
}

/// 取静态文案；未知 key 回退英文表，仍无则返回 key 本身，不 panic。
pub fn tr(lang: Language, key: &'static str) -> &'static str {
    let table = match lang {
        Language::Zh => ZH,
        Language::En => EN,
    };
    table
        .iter()
        .find(|(name, _)| *name == key)
        .map(|(_, text)| *text)
        .or_else(|| {
            EN.iter()
                .find(|(name, _)| *name == key)
                .map(|(_, text)| *text)
        })
        .unwrap_or(key)
}

/// 取带占位的文案并替换 `{name}` 参数。
pub fn trf(lang: Language, key: &'static str, args: &[(&str, &str)]) -> String {
    let mut text = tr(lang, key).to_owned();
    for (name, value) in args {
        text = text.replace(&format!("{{{name}}}"), value);
    }
    text
}

pub const ZH: &[(&str, &str)] = &[("notify.title", "TrafficView 告警")];
pub const EN: &[(&str, &str)] = &[("notify.title", "TrafficView Alert")];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_from_locale_strings() {
        assert_eq!(detect_from(Some("zh_CN.UTF-8")), Language::Zh);
        assert_eq!(detect_from(Some("zh_TW")), Language::Zh);
        assert_eq!(detect_from(Some("en_US.UTF-8")), Language::En);
        assert_eq!(detect_from(Some("C")), Language::En);
        assert_eq!(detect_from(None), Language::En);
    }

    #[test]
    fn explicit_language_overrides_environment() {
        assert_eq!(resolve("zh"), Language::Zh);
        assert_eq!(resolve("EN"), Language::En);
        let _ = resolve("auto");
        let _ = resolve("fr");
    }

    #[test]
    fn unknown_key_falls_back_without_panic() {
        assert_eq!(tr(Language::Zh, "notify.title"), "TrafficView 告警");
        assert_eq!(tr(Language::En, "notify.title"), "TrafficView Alert");
        assert_eq!(tr(Language::Zh, "no.such.key"), "no.such.key");
    }

    #[test]
    fn trf_replaces_named_placeholders() {
        let text = trf(Language::En, "notify.title", &[]);
        assert_eq!(text, "TrafficView Alert");
    }
}
