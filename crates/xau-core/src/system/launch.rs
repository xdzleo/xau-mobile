//! Launch the Microsoft Store "Xbox" app via its AppUserModelID, reproducing
//! the `AutoLaunchXboxAppEnabled` behaviour in `HomeViewModel.InitializeViewModel`.

use std::process::Command;

use crate::error::{Error, Result};

/// Hard-coded AUMID for the modern Xbox PC app (from
/// `HKCR\Local Settings\Software\Microsoft\Windows\CurrentVersion\AppModel\PackageRepository\Packages`).
pub const XBOX_APP_AUMID: &str =
    r"shell:appsFolder\Microsoft.GamingApp_8wekyb3d8bbwe!Microsoft.Xbox.App";

/// Attempts to launch the Xbox PC app. No-ops on non-Windows.
///
/// Equivalent to:
/// ```text
/// Process.Start(new ProcessStartInfo { UseShellExecute = true, FileName = XBOX_APP_AUMID });
/// ```
#[allow(clippy::needless_pass_by_value)]
pub fn launch_xbox_app(hidden: bool) -> Result<()> {
    #[cfg(windows)]
    {
        // `cmd /c start "" shell:appsFolder\...` is the Rust-friendly shim
        // around `ShellExecute`. The empty "" is a placeholder for the
        // console title expected by `start`.
        let mut cmd = Command::new("cmd");
        cmd.args(["/c", "start", "", XBOX_APP_AUMID]);
        if hidden {
            // Hide the temporary cmd window.
            // We can't hide the Xbox app itself — Windows does not honour
            // "hidden" hints for Store apps — so this is best-effort.
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                const CREATE_NO_WINDOW: u32 = 0x0800_0000;
                cmd.creation_flags(CREATE_NO_WINDOW);
            }
        }
        let status = cmd
            .status()
            .map_err(|e| Error::other(format!("failed to spawn cmd /c start: {e}")))?;
        if !status.success() {
            return Err(Error::other(format!(
                "launching Xbox app failed with exit code {}",
                status.code().unwrap_or(-1)
            )));
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = hidden;
        Err(Error::Unsupported(
            "launching the Xbox PC app is Windows-only",
        ))
    }
}

/// Returns true when the Xbox PC app is already running.
pub fn is_xbox_app_running() -> Result<bool> {
    crate::memory::is_process_running(crate::xbox::constants::process_names::XBOX_PC_APP)
}
