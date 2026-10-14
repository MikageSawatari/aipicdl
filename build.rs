fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/icon.ico");

    // エクスプローラーのプロパティや署名ダイアログに表示されるバージョン情報。
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let version = format!("{}.0", env!("CARGO_PKG_VERSION"));
        let mut res = winresource::WindowsResource::new();
        res.set("FileDescription", "aipicdl - Aipictors archiver");
        res.set("ProductName", "aipicdl");
        res.set("FileVersion", &version);
        res.set("ProductVersion", &version);
        res.set("OriginalFilename", "aipicdl.exe");
        res.set("LegalCopyright", "Copyright (C) 2026 Mikage Sawatari");
        if std::path::Path::new("assets/icon.ico").exists() {
            res.set_icon("assets/icon.ico");
        }
        if let Err(e) = res.compile() {
            println!("cargo:warning=winresource compile error: {e}");
        }
    }
}
