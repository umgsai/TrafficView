//! 系统深浅色：freedesktop appearance portal 读取与 `auto` 决策。
//!
//! portal 值：0=unspecified、1=dark、2=light。`auto` 在 unspecified 或 portal
//! 不可用时回退 dark，保证皮肤始终可读。

use futures_util::StreamExt;
use tokio::sync::mpsc;
use trafficview_core::{config::ThemeMode, ColorScheme};
use zbus::zvariant::OwnedValue;

pub const PORTAL_BUS: &str = "org.freedesktop.portal.Desktop";
pub const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
pub const PORTAL_IFACE: &str = "org.freedesktop.portal.Settings";
pub const APPEARANCE_NAMESPACE: &str = "org.freedesktop.appearance";
pub const COLOR_SCHEME_KEY: &str = "color-scheme";

/// 根据用户配置与 portal 原始值决定最终配色。
pub fn decide(mode: ThemeMode, portal: Option<u32>) -> ColorScheme {
    match mode {
        ThemeMode::Light => ColorScheme::Light,
        ThemeMode::Dark => ColorScheme::Dark,
        ThemeMode::Auto => match portal {
            Some(2) => ColorScheme::Light,
            _ => ColorScheme::Dark,
        },
    }
}

pub fn scheme_name(scheme: ColorScheme) -> &'static str {
    match scheme {
        ColorScheme::Light => "light",
        ColorScheme::Dark => "dark",
    }
}

/// 读取一次当前 portal 配色；portal 不存在时返回 `None` 而不是错误。
pub async fn read_portal_scheme(connection: &zbus::Connection) -> Option<u32> {
    // 先确认 portal 已在运行，避免在无 portal 的会话里触发服务激活。
    let owned = connection
        .call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "NameHasOwner",
            &(PORTAL_BUS,),
        )
        .await
        .ok()?
        .body()
        .deserialize::<bool>()
        .ok()?;
    if !owned {
        return None;
    }
    let reply = connection
        .call_method(
            Some(PORTAL_BUS),
            PORTAL_PATH,
            Some(PORTAL_IFACE),
            "ReadOne",
            &(APPEARANCE_NAMESPACE, COLOR_SCHEME_KEY),
        )
        .await
        .ok()?;
    let value: OwnedValue = reply.body().deserialize().ok()?;
    u32::try_from(value).ok()
}

/// 订阅 `SettingChanged`；返回的接收端只携带 appearance/color-scheme 的新值。
pub async fn subscribe(connection: &zbus::Connection) -> Option<mpsc::Receiver<u32>> {
    let rule = zbus::match_rule::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(PORTAL_IFACE)
        .ok()?
        .member("SettingChanged")
        .ok()?
        .build();
    let mut stream = zbus::MessageStream::for_match_rule(rule, connection, Some(16))
        .await
        .ok()?;
    let (tx, rx) = mpsc::channel(8);
    tokio::spawn(async move {
        while let Some(Ok(message)) = stream.next().await {
            let Ok((namespace, key, value)) =
                message.body().deserialize::<(String, String, OwnedValue)>()
            else {
                continue;
            };
            if namespace != APPEARANCE_NAMESPACE || key != COLOR_SCHEME_KEY {
                continue;
            }
            let Ok(scheme) = u32::try_from(value) else {
                continue;
            };
            if tx.send(scheme).await.is_err() {
                break;
            }
        }
    });
    Some(rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_follows_portal_and_defaults_to_dark() {
        assert_eq!(decide(ThemeMode::Auto, Some(2)), ColorScheme::Light);
        assert_eq!(decide(ThemeMode::Auto, Some(1)), ColorScheme::Dark);
        assert_eq!(decide(ThemeMode::Auto, Some(0)), ColorScheme::Dark);
        assert_eq!(decide(ThemeMode::Auto, None), ColorScheme::Dark);
    }

    #[test]
    fn explicit_modes_ignore_the_portal() {
        assert_eq!(decide(ThemeMode::Light, Some(1)), ColorScheme::Light);
        assert_eq!(decide(ThemeMode::Dark, Some(2)), ColorScheme::Dark);
        assert_eq!(scheme_name(ColorScheme::Light), "light");
        assert_eq!(scheme_name(ColorScheme::Dark), "dark");
    }
}
