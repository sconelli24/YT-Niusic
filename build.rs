fn main() {
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=assets/icon.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("ProductName", "YT-Niusic");
        res.set("FileDescription", "YT-Niusic — YouTube Music TUI Console");
        if let Err(e) = res.compile() {
            panic!("Failed to embed Windows application icon: {e}");
        }
    }
}
