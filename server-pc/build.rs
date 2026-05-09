//! Build script that generates Rust FFI bindings to NVIDIA's NVENC encoder
//! API from the vendored FFmpeg `nv-codec-headers` submodule.
//!
//! The bindings land in `$OUT_DIR/nvenc_sys.rs` and are `include!()`'d by
//! `src/encoder/nvenc_sys.rs`. We deliberately *don't* link against
//! `nvEncodeAPI64.dll` — the encoder loads it at runtime via `LoadLibraryW`
//! so the binary still starts on a machine without an NVIDIA GPU (where
//! we fall back to the MFT software / hardware H.264 encoder).

use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!(
        "cargo:rerun-if-changed=../vendor/nv-codec-headers/include/ffnvcodec/nvEncodeAPI.h"
    );

    let target = env::var("TARGET").unwrap_or_default();
    if !target.contains("windows") {
        // NVENC direct path is Windows-only in this project — we tie it
        // to D3D11 desktop duplication. On other targets we just skip.
        println!("cargo:warning=non-Windows target, skipping NVENC bindgen");
        let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
        std::fs::write(out_dir.join("nvenc_sys.rs"), "// stub\n").ok();
        return;
    }

    // bindgen needs libclang to parse C headers. winget-installed LLVM
    // puts it at C:\Program Files\LLVM\bin\libclang.dll. We point the
    // env var explicitly so anyone with a stock LLVM install picks up
    // bindings without further setup.
    if env::var("LIBCLANG_PATH").is_err() {
        let default = "C:\\Program Files\\LLVM\\bin";
        if PathBuf::from(default).join("libclang.dll").exists() {
            env::set_var("LIBCLANG_PATH", default);
        }
    }

    let header = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("server-pc has parent")
        .join("vendor")
        .join("nv-codec-headers")
        .join("include")
        .join("ffnvcodec")
        .join("nvEncodeAPI.h");

    if !header.exists() {
        panic!(
            "missing NVENC header: {}\n\nRun `git submodule update --init --recursive`.",
            header.display()
        );
    }

    let bindings = bindgen::Builder::default()
        .header(header.to_string_lossy())
        // Limit codegen to the NVENC encoder API surface — we don't want
        // CUDA / cuvid / nvcuvid types pulled in via sibling headers.
        .allowlist_type("NV_ENC.*|NVENC.*|GUID|_NV_ENC.*|_NVENC.*")
        .allowlist_var("NV_ENC.*|NVENC.*")
        .allowlist_function("NvEnc.*")
        // GUIDs are header-inline `static const`s — no DLL exports them, so
        // we hand-translate them in nvenc_sys.rs to keep the link clean.
        .blocklist_item("NV_ENC_.*_GUID")
        // We dlopen() nvEncodeAPI64.dll at runtime; don't let bindgen
        // declare the entry point as an `extern` symbol or the linker
        // will demand the import lib at build time.
        .blocklist_function("NvEncodeAPICreateInstance")
        .derive_default(true)
        .derive_debug(false)
        .layout_tests(false)
        .generate_comments(false)
        .prepend_enum_name(false)
        // Map enum-like #defines to constified module groups so the rust
        // side can reference e.g. NV_ENC_BUFFER_FORMAT::NV12 directly.
        .default_enum_style(bindgen::EnumVariation::ModuleConsts)
        .generate()
        .expect("bindgen failed on nvEncodeAPI.h");

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR set by cargo"));
    let out_path = out_dir.join("nvenc_sys.rs");
    bindings
        .write_to_file(&out_path)
        .expect("write bindings to OUT_DIR");
}
