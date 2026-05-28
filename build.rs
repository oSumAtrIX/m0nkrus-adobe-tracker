fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    #[cfg(target_os = "windows")]
    {
        if std::path::Path::new("assets/icon.ico").exists() {
            winresource::WindowsResource::new()
                .set_icon("assets/icon.ico")
                .compile()
                .expect("failed to compile Windows resources");
        }
    }
}
