fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // WindowsResource reads the inherited Cargo package version.
        tauri_winres::WindowsResource::new()
            .set("ProductName", "StarBridge Server")
            .set("FileDescription", "StarBridge Central Server")
            .set("OriginalFilename", "starbridge-server.exe")
            .compile()
            .expect("failed to embed Windows version information");
    }
}
