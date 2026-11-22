//! 管理 XDG autostart 桌面条目（`$XDG_CONFIG_HOME/autostart/trafficview.desktop`）。
//!
//! 这是**本地用户文件**，不是 daemon 的 `config.toml`，因此设置界面直接读写它，
//! 与 `trafficview autostart enable/disable` 的行为一致；不需要提权、不经过 D-Bus。

use std::{
    fs, io,
    path::{Path, PathBuf},
};

/// 解析当前用户的配置根目录：`$XDG_CONFIG_HOME` 回退到 `~/.config`。
pub fn config_home() -> PathBuf {
    if let Ok(value) = std::env::var("XDG_CONFIG_HOME") {
        if !value.is_empty() {
            return PathBuf::from(value);
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".config");
    }
    PathBuf::from(".config")
}

/// autostart 桌面文件路径（基于默认配置根）。
pub fn autostart_path() -> PathBuf {
    autostart_path_in(&config_home())
}

/// 给定配置根目录下的 autostart 桌面文件路径（便于测试注入临时目录）。
pub fn autostart_path_in(config_home: &Path) -> PathBuf {
    config_home.join("autostart").join("trafficview.desktop")
}

/// 尽量解析出 `trafficview` 可执行文件的绝对路径，用于桌面条目的 `Exec`。
fn tray_executable() -> String {
    if let Ok(exe) = std::env::current_exe() {
        let sibling = exe.with_file_name("trafficview");
        if sibling.exists() {
            return sibling.display().to_string();
        }
    }
    // 找不到同目录二进制时，交给登录时的 PATH 解析。
    "trafficview".to_string()
}

fn desktop_entry_contents() -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName=TrafficView\nComment=Network, CPU and memory monitor\nExec={} tray --autostart\nIcon=trafficview\nTerminal=false\nX-GNOME-Autostart-enabled=true\n",
        tray_executable()
    )
}

/// 当前是否已启用自启动。
pub fn is_enabled() -> bool {
    autostart_path().exists()
}

/// 在指定配置根下是否已启用（测试用）。
#[cfg(test)]
pub fn is_enabled_in(config_home: &Path) -> bool {
    autostart_path_in(config_home).exists()
}

/// 写入或移除 autostart 条目。
pub fn set_enabled(enabled: bool) -> Result<(), String> {
    set_enabled_in(&config_home(), enabled)
}

pub fn set_enabled_in(config_home: &Path, enabled: bool) -> Result<(), String> {
    let path = autostart_path_in(config_home);
    if enabled {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create autostart dir: {error}"))?;
        }
        fs::write(&path, desktop_entry_contents())
            .map_err(|error| format!("cannot write autostart entry: {error}"))?;
    } else {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("cannot remove autostart entry: {error}")),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enable_writes_desktop_file_and_is_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path();
        assert!(!is_enabled_in(base));

        set_enabled_in(base, true).expect("enable");
        let path = autostart_path_in(base);
        assert!(path.exists());
        let contents = fs::read_to_string(&path).expect("read");
        assert!(contents.contains("tray --autostart"), "{contents}");
        assert!(
            contents.contains("X-GNOME-Autostart-enabled=true"),
            "{contents}"
        );

        // 重复启用不报错。
        set_enabled_in(base, true).expect("enable again");
        assert!(is_enabled_in(base));
    }

    #[test]
    fn disable_removes_entry_and_keeps_other_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path();
        set_enabled_in(base, true).expect("enable");
        let other = base.join("autostart").join("other.desktop");
        fs::write(&other, "x").expect("write other");

        set_enabled_in(base, false).expect("disable");
        assert!(!is_enabled_in(base));
        assert!(other.exists(), "other autostart entry must be kept");

        // 重复禁用不报错。
        set_enabled_in(base, false).expect("disable again");
    }

    #[test]
    fn disable_when_never_enabled_is_ok() {
        let dir = tempfile::tempdir().expect("tempdir");
        set_enabled_in(dir.path(), false).expect("disable no-op");
    }
}
