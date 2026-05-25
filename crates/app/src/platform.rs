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
