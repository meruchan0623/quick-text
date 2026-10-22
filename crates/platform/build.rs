fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    for kind in ["detect", "inject"] {
        let base = format!("../../vendor/espanso-win32/{kind}");
        println!("cargo:rerun-if-changed={base}/native.cpp");
        println!("cargo:rerun-if-changed={base}/native.h");
        cc::Build::new()
            .cpp(true)
            .file(format!("{base}/native.cpp"))
            .flag_if_supported("/std:c++14")
            .compile(&format!("espanso_{kind}"));
    }
    println!("cargo:rustc-link-lib=user32");
}
