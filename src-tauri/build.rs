fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rerun-if-changed=vendor/pacparser/pacparser.c");
        println!("cargo:rerun-if-changed=vendor/pacparser/pacparser.h");
        println!("cargo:rerun-if-changed=vendor/pacparser/pac_utils.h");
        println!("cargo:rerun-if-changed=vendor/pacparser/quickjs/quickjs.c");
        println!("cargo:rerun-if-changed=vendor/pacparser/quickjs/quickjs.h");
        cc::Build::new()
            .file("vendor/pacparser/pacparser.c")
            .file("vendor/pacparser/quickjs/quickjs.c")
            .include("vendor/pacparser")
            .include("vendor/pacparser/quickjs")
            .define("VERSION", "1.5.1")
            .warnings(false)
            .compile("pacparser");
    }
    tauri_build::build()
}
