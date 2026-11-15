fn main() {
    println!("cargo:rerun-if-changed=assets/app.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/app.ico");
        res.set("ProductName", "File Recovery");
        res.set(
            "FileDescription",
            "File Recovery — read-only recovery workspace",
        );
        res.set_manifest_file("assets/app.manifest");
        res.compile().expect("embed Windows identity resources");
    }
}
