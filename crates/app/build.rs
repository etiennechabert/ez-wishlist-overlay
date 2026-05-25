// Embeds assets/icon.ico into the Windows .exe so the binary shows our app
// icon in Explorer, the Taskbar, Alt-Tab, and the Start Menu shortcut.
// No-op on other targets.

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");

    #[cfg(target_os = "windows")]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.compile().expect("compile Windows resource (icon)");
    }
}
