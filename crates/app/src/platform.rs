//! Cross-platform "open URL or path" via the OS shell handler.

use std::ffi::OsStr;
use std::io;
use std::process::Command;

pub fn open<S: AsRef<OsStr>>(target: S) -> io::Result<()> {
    let target = target.as_ref();
    #[cfg(target_os = "windows")]
    {
        Command::new("cmd")
            .arg("/C")
            .arg("start")
            .arg("")
            .arg(target)
            .spawn()
            .map(|_| ())
    }
    #[cfg(target_os = "macos")]
    {
        Command::new("open").arg(target).spawn().map(|_| ())
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Command::new("xdg-open").arg(target).spawn().map(|_| ())
    }
}

/// How this build was installed, which decides whether the app can apply an
/// update itself. An MSI-managed install lives under Program Files and can be
/// upgraded in place by handing the downloaded `.msi` to `msiexec`; a portable
/// `.exe` run from anywhere else has no installer to invoke, so the UI falls
/// back to the browser download link.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InstallKind {
    /// Installed via the MSI (running from Program Files).
    Msi,
    /// Standalone portable binary, or any non-Windows dev build.
    Portable,
}

/// Classify the running build. The interesting case is Windows-only; every
/// other OS is a dev build with no installer, so it reports
/// [`InstallKind::Portable`].
pub fn install_kind() -> InstallKind {
    #[cfg(target_os = "windows")]
    {
        if running_under_program_files() {
            InstallKind::Msi
        } else {
            InstallKind::Portable
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        InstallKind::Portable
    }
}

/// True when `current_exe()` sits under one of the Program Files roots.
/// Writing there needs elevation, so a binary running from Program Files is
/// effectively an installed (MSI-managed) copy — a portable exe runs from a
/// user-writable location (Downloads, Desktop, a USB stick) and returns false.
/// Comparison is case-insensitive because Windows paths are. A mis-classify is
/// safe either way: it only chooses between "Update now" and the browser link.
#[cfg(target_os = "windows")]
fn running_under_program_files() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let exe = exe.to_string_lossy().to_lowercase();
    ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"]
        .iter()
        .filter_map(|var| std::env::var(var).ok())
        .filter(|root| !root.is_empty())
        .any(|root| exe.starts_with(&root.to_lowercase()))
}

/// Hand a downloaded MSI to the Windows Installer. `msiexec /i` shows the
/// normal install UI; because our package is `perMachine`, Windows raises the
/// UAC consent prompt automatically. The spawned process is detached — it
/// keeps running after we exit, which is exactly what we want: the app closes
/// so the installer can swap the binary, and the installer's Finish dialog
/// offers to relaunch the updated build.
#[cfg(target_os = "windows")]
pub fn run_msi_installer(msi_path: &std::path::Path) -> io::Result<()> {
    Command::new("msiexec")
        .arg("/i")
        .arg(msi_path)
        .spawn()
        .map(|_| ())
}

/// Non-Windows builds have no MSI to run; surface a clear error so the caller
/// degrades to the browser link.
#[cfg(not(target_os = "windows"))]
pub fn run_msi_installer(_msi_path: &std::path::Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "MSI install is only available on Windows",
    ))
}
