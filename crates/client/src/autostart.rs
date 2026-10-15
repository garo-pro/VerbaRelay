//! Starting the client with the user session.
//!
//! * Windows: `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`
//! * Linux desktops: `~/.config/autostart/verbarelay-client.desktop`
//! * Headless Linux: use the systemd user unit from `packaging/systemd/` instead.

use anyhow::Context;

const NAME: &str = "verbarelay-client";

/// Registers the current executable for autostart (idempotent).
pub fn enable() -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("locating the executable")?;
    platform::enable(&exe)
}

/// Removes the autostart entry.
pub fn disable() -> anyhow::Result<()> {
    platform::disable()
}

#[cfg(windows)]
mod platform {
    use super::NAME;
    use std::path::Path;
    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, KEY_SET_VALUE};

    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

    pub fn enable(exe: &Path) -> anyhow::Result<()> {
        let key = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(RUN_KEY, KEY_SET_VALUE)?;
        key.set_value(NAME, &format!("\"{}\"", exe.display()))?;
        Ok(())
    }

    pub fn disable() -> anyhow::Result<()> {
        let key = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(RUN_KEY, KEY_SET_VALUE)?;
        let _ = key.delete_value(NAME);
        Ok(())
    }
}

#[cfg(not(windows))]
mod platform {
    use super::NAME;
    use std::path::{Path, PathBuf};

    fn desktop_file() -> Option<PathBuf> {
        directories::BaseDirs::new().map(|d| d.config_dir().join("autostart").join(format!("{NAME}.desktop")))
    }

    pub fn enable(exe: &Path) -> anyhow::Result<()> {
        // Only desktop sessions read autostart entries; headless machines use systemd.
        if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
            return Ok(());
        }
        let Some(file) = desktop_file() else { return Ok(()) };
        let contents = format!(
            "[Desktop Entry]\nType=Application\nName=VerbaRelay client\nComment=Transcribes audio for a VerbaRelay server\nExec=\"{}\"\nTerminal=false\nNoDisplay=true\nX-GNOME-Autostart-enabled=true\n",
            exe.display()
        );
        if std::fs::read_to_string(&file).ok().as_deref() != Some(contents.as_str()) {
            if let Some(parent) = file.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&file, contents)?;
        }
        Ok(())
    }

    pub fn disable() -> anyhow::Result<()> {
        if let Some(file) = desktop_file() {
            let _ = std::fs::remove_file(file);
        }
        Ok(())
    }
}
