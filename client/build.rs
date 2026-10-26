fn main() {
    #[cfg(feature = "desktop")]
    {
        println!("cargo:rerun-if-changed=icons/icon.ico");
        println!("cargo:rerun-if-changed=icons/icon.png");
        tauri_build::build();
    }
    // Tauri embeds the desktop resource; headless builds need their own.
    #[cfg(not(feature = "desktop"))]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        tauri_winres::WindowsResource::new()
            .set("ProductName", "StarBridge")
            .set("FileDescription", "StarBridge Headless Client")
            .set("OriginalFilename", "starbridge-client.exe")
            .compile()
            .expect("failed to embed Windows version information");
    }
}
