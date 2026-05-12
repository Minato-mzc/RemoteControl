//! Build script. Two unrelated jobs:
//!   1. Generate Rust FFI bindings to NVIDIA's NVENC encoder API from
//!      `vendor/nv-codec-headers/include/ffnvcodec/nvEncodeAPI.h`. The
//!      bindings land in `$OUT_DIR/nvenc_sys.rs` and are `include!()`'d
//!      by `src/encoder/nvenc_sys.rs`. We deliberately *don't* link
//!      against `nvEncodeAPI64.dll` — the encoder loads it at runtime
//!      via `LoadLibraryW` so the binary still starts on a machine
//!      without an NVIDIA GPU.
//!   2. Render `assets/icon.svg` into a multi-resolution `.ico` and
//!      embed it (plus a `VS_VERSIONINFO` block) into the exe via
//!      `embed-resource`. Keeps the SVG as the single source of truth
//!      — no binary `.ico` committed to the repo.

use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!(
        "cargo:rerun-if-changed=../vendor/nv-codec-headers/include/ffnvcodec/nvEncodeAPI.h"
    );
    println!("cargo:rerun-if-changed=assets/icon.svg");

    let target = env::var("TARGET").unwrap_or_default();
    let is_windows = target.contains("windows");

    if is_windows {
        emit_windows_resources();
    }

    if !is_windows {
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

/// Render `assets/icon.svg` → multi-resolution `.ico`, write a `.rc` file
/// with `ICON` + `VS_VERSIONINFO` blocks, compile + link both into the
/// exe via `embed-resource`. Windows-only path.
fn emit_windows_resources() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let svg_path = manifest_dir.join("assets").join("icon.svg");
    let ico_path = out_dir.join("icon.ico");
    let rc_path = out_dir.join("app.rc");

    let svg_bytes = std::fs::read(&svg_path)
        .unwrap_or_else(|e| panic!("read {}: {}", svg_path.display(), e));
    let tree = usvg::Tree::from_data(&svg_bytes, &usvg::Options::default())
        .expect("parse icon.svg with usvg");

    // Standard Windows .ico contents — sizes that the shell, alt-tab,
    // taskbar, and high-DPI start menu pick from. 256 is required by
    // modern Explorer thumbnails; the small sizes are taskbar/tray-ish.
    let sizes: [u32; 7] = [16, 24, 32, 48, 64, 128, 256];

    let mut icon_dir = ico::IconDir::new(ico::ResourceType::Icon);
    let svg_size = tree.size();
    for &size in &sizes {
        let mut pixmap = tiny_skia::Pixmap::new(size, size)
            .expect("pixmap alloc");
        let scale = size as f32 / svg_size.width().max(svg_size.height());
        let transform = tiny_skia::Transform::from_scale(scale, scale);
        resvg::render(&tree, transform, &mut pixmap.as_mut());
        let image = ico::IconImage::from_rgba_data(size, size, pixmap.data().to_vec());
        let entry = ico::IconDirEntry::encode(&image)
            .expect("encode ico entry");
        icon_dir.add_entry(entry);
    }
    let f = std::fs::File::create(&ico_path)
        .expect("create icon.ico");
    icon_dir.write(f).expect("write icon.ico");

    // .rc parser treats `\` as escape, so we hand it forward slashes.
    // The Windows resource compiler is happy either way on real paths.
    let ico_for_rc = ico_path.to_string_lossy().replace('\\', "/");

    // Version digits parsed from CARGO_PKG_VERSION (set by cargo). We
    // pad to 4 components — `0.2.0` → `0,2,0,0`.
    let (v_a, v_b, v_c) = parse_semver(env!("CARGO_PKG_VERSION"));
    let version_quad = format!("{v_a},{v_b},{v_c},0");
    let version_str = format!("{v_a}.{v_b}.{v_c}.0");

    let rc = format!(
        r#"#include <winver.h>
1 ICON "{ico}"

1 VERSIONINFO
 FILEVERSION    {ver_quad}
 PRODUCTVERSION {ver_quad}
 FILEOS         VOS_NT_WINDOWS32
 FILETYPE       VFT_APP
 FILESUBTYPE    VFT2_UNKNOWN
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904b0"
        BEGIN
            VALUE "CompanyName",      "RemoteControl"
            VALUE "FileDescription",  "RemoteControl PC server"
            VALUE "FileVersion",      "{ver_str}"
            VALUE "InternalName",     "remotecontrol-server"
            VALUE "LegalCopyright",   "RemoteControl contributors"
            VALUE "OriginalFilename", "remotecontrol-server.exe"
            VALUE "ProductName",      "RemoteControl"
            VALUE "ProductVersion",   "{ver_str}"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x0409, 0x04B0
    END
END
"#,
        ico = ico_for_rc,
        ver_quad = version_quad,
        ver_str = version_str,
    );
    std::fs::write(&rc_path, rc).expect("write app.rc");

    embed_resource::compile(&rc_path, embed_resource::NONE);
}

fn parse_semver(v: &str) -> (u16, u16, u16) {
    let mut parts = v.split('.').map(|p| p.parse::<u16>().unwrap_or(0));
    let a = parts.next().unwrap_or(0);
    let b = parts.next().unwrap_or(0);
    let c = parts.next().unwrap_or(0);
    (a, b, c)
}
