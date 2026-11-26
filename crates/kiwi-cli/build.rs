//! Links the clang runtime on macOS when local models (ONNX Runtime) are built in.
//!
//! ONNX Runtime's CoreML code uses `@available`, which needs `__isPlatformVersionAtLeast`
//! from `libclang_rt.osx.a`; rustc links with `-nodefaultlibs`, so optimised builds that
//! keep that code fail to link without it.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let macos = std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "macos");
    let onnx = std::env::var_os("CARGO_FEATURE_FASTEMBED").is_some();
    if !(macos && onnx) {
        return;
    }
    let Ok(output) = std::process::Command::new("clang")
        .arg("--print-runtime-dir")
        .output()
    else {
        println!("cargo:warning=clang not found; linking ONNX Runtime may fail");
        return;
    };
    let dir = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let lib = std::path::Path::new(&dir).join("libclang_rt.osx.a");
    if lib.exists() {
        println!("cargo:rustc-link-arg={}", lib.display());
    }
}
