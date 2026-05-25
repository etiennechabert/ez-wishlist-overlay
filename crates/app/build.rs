// Embeds assets/icon.ico into the Windows .exe so the binary shows our app
// icon in Explorer, the Taskbar, Alt-Tab, and the Start Menu shortcut.
// No-op on other targets.

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");

    #[cfg(target_os = "windows")]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        // Icon-embed needs rc.exe on PATH (Windows SDK), which is only
        // populated inside a VS Dev Shell. CI and `./scripts/run-app.ps1`
        // both enter the Dev Shell, so shipping/release builds embed the
        // icon as expected. Plain `cargo app-dev` from a vanilla PowerShell
        // would otherwise panic here — degrade to a warning so local
        // iteration still builds. The dev .exe just won't have an embedded
        // icon in that case (cosmetic only; release artifacts are unaffected).
        if let Err(e) = res.compile() {
            println!(
                "cargo:warning=skipping icon embed ({e}). Build from a VS Dev Shell \
                 (e.g. `./scripts/run-app.ps1`) to embed the icon."
            );
        }
    }
}
