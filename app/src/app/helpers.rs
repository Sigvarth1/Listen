//! Helper programs on Windows and macOS (mpv, yt-dlp, Deno): not bundled, installed from their
//! winget (Windows) or Homebrew (macOS) packages via a dialog or the onboarding, in a console /
//! Terminal window of their own.

use super::{Listen, Message};
use iced::Task;

/// The modal key of the helper-programs dialog (`ui::overlay`).
pub const MODAL: &str = "helper-programs";

/// The package manager the helper dialog uses on this platform.
pub const PACKAGE_MANAGER: &str = if cfg!(target_os = "macos") { "Homebrew" } else { "winget" };

/// Whether this platform gets the helper dialog (elsewhere the distribution's packages are used).
pub const SUPPORTED: bool = cfg!(any(windows, target_os = "macos"));

pub struct Helper {
    pub name: &'static str,
    pub winget_id: &'static str,
    /// The Homebrew formula (macOS).
    pub brew: &'static str,
    pub what: &'static str,
}

pub const HELPERS: [Helper; 3] = [
    Helper { name: "mpv", winget_id: "shinchiro.mpv", brew: "mpv", what: "Plays audio and video." },
    Helper {
        name: "yt-dlp",
        winget_id: "yt-dlp.yt-dlp",
        brew: "yt-dlp",
        what: "Finds the audio of YouTube Music songs.",
    },
    Helper {
        name: "Deno",
        winget_id: "DenoLand.Deno",
        brew: "deno",
        what: "The JavaScript runtime yt-dlp needs for YouTube.",
    },
];

/// The helpers that aren't installed (always empty off Windows and macOS).
pub fn missing() -> Vec<&'static Helper> {
    if !SUPPORTED {
        return Vec::new();
    }
    HELPERS
        .iter()
        .filter(|h| match h.name {
            "Deno" => listen_ytmusic::ytdlp::find_js_runtime().is_none(),
            name => !listen_core::platform::tool_installed(&name.to_lowercase()),
        })
        .collect()
}

/// The batch script that installs (or, with `upgrade`, updates) `ids` with winget.
pub fn script(ids: &[&str], upgrade: bool) -> String {
    let verb = if upgrade { "upgrade" } else { "install" };
    let mut s = String::from(
        "@echo off\r\ntitle Listen - helper programs\r\n\
         where winget >nul 2>nul || (\r\n\
         echo winget is missing. Install \"App Installer\" from the Microsoft Store, then try again.\r\n\
         start \"\" \"https://apps.microsoft.com/detail/9NBLGGH4NNS1\"\r\n\
         pause\r\nexit /b 1\r\n)\r\n",
    );
    for id in ids {
        s.push_str(&format!(
            "echo.\r\necho == {id}\r\nwinget {verb} --exact --id {id} --source winget \
             --accept-package-agreements --accept-source-agreements\r\n"
        ));
    }
    s.push_str("echo.\r\necho Done. Switch back to Listen and press \"Check again\".\r\npause\r\n");
    s
}

/// The shell script that installs (or, with `upgrade`, updates) the Homebrew `formulas`; it runs in
/// Terminal and explains how to get Homebrew when it is missing.
pub fn brew_script(formulas: &[&str], upgrade: bool) -> String {
    let verb = if upgrade { "upgrade" } else { "install" };
    format!(
        "#!/bin/bash\n\
         printf '\\033]0;Listen - helper programs\\007'\n\
         BREW=\"$(command -v brew || ls /opt/homebrew/bin/brew /usr/local/bin/brew 2>/dev/null | head -n 1)\"\n\
         if [ -z \"$BREW\" ]; then\n\
         \x20 echo 'Homebrew is missing. Install it from https://brew.sh (opening it now), then try again.'\n\
         \x20 open https://brew.sh\n\
         \x20 read -r -p 'Press Return to close.'\n\
         \x20 exit 1\n\
         fi\n\
         \"$BREW\" {verb} {}\n\
         echo\n\
         echo 'Done. Switch back to Listen and press \"Check again\".'\n\
         read -r -p 'Press Return to close.'\n",
        formulas.join(" ")
    )
}

/// macOS: writes the script to the temp folder and opens it in Terminal.
#[cfg(target_os = "macos")]
fn run_brew(formulas: &[&str], upgrade: bool) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::temp_dir().join("listen-helper-programs.command");
    std::fs::write(&path, brew_script(formulas, upgrade)).map_err(|e| format!("{}: {e}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    std::process::Command::new("/usr/bin/open")
        .args(["-a", "Terminal"])
        .arg(&path)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Writes the script to the temp folder and starts it in a console window.
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn run_script(text: &str) -> Result<(), String> {
    let path = std::env::temp_dir().join("listen-helper-programs.cmd");
    std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
    std::process::Command::new(&path).spawn().map(|_| ()).map_err(|e| e.to_string())
}

#[derive(Debug, Clone)]
pub enum HelpersMsg {
    Open,
    Install,
    /// `winget upgrade` for all three (YouTube changes often break old yt-dlp versions).
    Update,
    Check,
    Close,
}

#[derive(Debug, Default)]
pub struct HelpersState {
    /// Names of the missing helpers, as of the last check.
    pub missing: Vec<&'static str>,
    /// winget was started from this dialog.
    pub started: bool,
}

impl Listen {
    fn helpers_check(&mut self) {
        self.helpers.missing = missing().iter().map(|h| h.name).collect();
    }

    /// Start-up (Windows, macOS): opens the dialog when something is missing and onboarding is done.
    pub(super) fn helpers_boot(&mut self) {
        if !SUPPORTED || self.demo || self.ephemeral {
            return;
        }
        self.helpers_check();
        if !self.helpers.missing.is_empty() && !self.onboarding_pending() {
            self.open_modal(MODAL);
        }
    }

    pub(super) fn on_helpers(&mut self, msg: HelpersMsg) -> Task<Message> {
        match msg {
            HelpersMsg::Open => {
                self.helpers_check();
                self.open_modal(MODAL);
            }
            HelpersMsg::Install | HelpersMsg::Update => {
                let upgrade = matches!(msg, HelpersMsg::Update);
                let id = |h: &Helper| if cfg!(target_os = "macos") { h.brew } else { h.winget_id };
                let ids: Vec<&str> = if upgrade {
                    HELPERS.iter().map(id).collect()
                } else {
                    missing().into_iter().map(id).collect()
                };
                if ids.is_empty() {
                    self.toast_success("All helper programs are installed.".into());
                    return Task::none();
                }
                #[cfg(target_os = "macos")]
                let started = run_brew(&ids, upgrade);
                #[cfg(not(target_os = "macos"))]
                let started = run_script(&script(&ids, upgrade));
                match started {
                    Ok(()) => self.helpers.started = true,
                    Err(e) => self.toast_error(format!("Could not start {PACKAGE_MANAGER}: {e}")),
                }
            }
            HelpersMsg::Check => {
                self.helpers_check();
                if self.helpers.missing.is_empty() {
                    self.toast_success(
                        "All helper programs are installed. Restart Listen to use them.".into(),
                    );
                    if matches!(self.overlay.modal, Some(super::overlay::Modal::Custom(MODAL))) {
                        self.close_modal();
                    }
                }
            }
            HelpersMsg::Close => self.close_modal(),
        }
        Task::none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_installs_each_package_from_winget() {
        let s = script(&["shinchiro.mpv", "yt-dlp.yt-dlp"], false);
        assert!(s.contains("winget install --exact --id shinchiro.mpv --source winget"));
        assert!(s.contains("winget install --exact --id yt-dlp.yt-dlp --source winget"));
        assert!(s.contains("where winget"));
        assert!(script(&["DenoLand.Deno"], true).contains("winget upgrade --exact --id DenoLand.Deno"));
        assert!(s.lines().all(|l| l.len() < 200));
    }

    #[test]
    fn nothing_is_missing_off_windows_and_macos() {
        if !SUPPORTED {
            assert!(missing().is_empty());
        }
    }

    #[test]
    fn brew_script_installs_the_formulas() {
        let s = brew_script(&["mpv", "yt-dlp"], false);
        assert!(s.starts_with("#!/bin/bash\n"));
        assert!(s.contains("\"$BREW\" install mpv yt-dlp\n"), "{s}");
        assert!(s.contains("https://brew.sh"));
        assert!(brew_script(&["deno"], true).contains("\"$BREW\" upgrade deno"));
    }
}
