fn main() {
    println!("cargo:rerun-if-changed=src/videotoolbox.c");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        cc::Build::new()
            .file("src/videotoolbox.c")
            .compile("windroid_vt");
        for framework in ["CoreFoundation", "CoreMedia", "CoreVideo", "VideoToolbox"] {
            println!("cargo:rustc-link-lib=framework={framework}");
        }
    }
}
