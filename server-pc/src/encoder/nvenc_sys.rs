//! Raw FFI bindings to NVIDIA's NVENC encoder API.
//!
//! Most of this file is generated: `build.rs` runs `bindgen` against
//! `vendor/nv-codec-headers/include/ffnvcodec/nvEncodeAPI.h` and writes the
//! generated Rust source to `$OUT_DIR/nvenc_sys.rs`. We `include!()` it
//! and then patch in:
//!
//!  * `NV_ENC_*_GUID` constants — bindgen renders these as `extern static`
//!    but they're header-inline `static const`s with no DLL export. We
//!    hand-translate the bytes from the upstream header.
//!  * Struct-version macros — bindgen doesn't expand function-like macros,
//!    so the `*_VER` integers (each struct's version tag) are computed
//!    here from `NVENCAPI_STRUCT_VERSION`.
//!  * `NvEncodeAPICreateInstance` is intentionally blocklisted in
//!    bindgen so the linker doesn't demand `nvEncodeAPI64.lib` at build
//!    time — `nvenc_sdk::NvencApi` `LoadLibraryW`s the DLL at runtime.
//!
//! Anything in here is `unsafe` — wrap in `nvenc_sdk.rs` rather than
//! calling directly.

#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(dead_code)]
#![allow(clippy::all)]

#[cfg(windows)]
include!(concat!(env!("OUT_DIR"), "/nvenc_sys.rs"));

#[cfg(windows)]
mod supplements {
    use super::*;

    /// Compose the 32-bit version tag NVENC uses on every input struct.
    /// Mirrors the C macro `NVENCAPI_STRUCT_VERSION(ver)` in the header.
    /// Bit layout (low-to-high): 8 bits API major | 8 bits API minor |
    /// 16 bits struct revision | 4 bits "7" magic.
    #[inline]
    pub const fn struct_version(ver: u32) -> u32 {
        NVENCAPI_VERSION | (ver << 16) | (0x7 << 28)
    }

    // Struct version tags — see NVENC header for revision history.
    pub const NV_ENCODE_API_FUNCTION_LIST_VER: u32 = struct_version(2);
    pub const NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER: u32 = struct_version(1);
    pub const NV_ENC_PRESET_CONFIG_VER: u32 = struct_version(5) | (1u32 << 31);
    pub const NV_ENC_CONFIG_VER: u32 = struct_version(9) | (1u32 << 31);
    pub const NV_ENC_RC_PARAMS_VER: u32 = struct_version(1);
    pub const NV_ENC_INITIALIZE_PARAMS_VER: u32 = struct_version(7) | (1u32 << 31);
    pub const NV_ENC_RECONFIGURE_PARAMS_VER: u32 = struct_version(2) | (1u32 << 31);
    pub const NV_ENC_REGISTER_RESOURCE_VER: u32 = struct_version(5);
    pub const NV_ENC_MAP_INPUT_RESOURCE_VER: u32 = struct_version(4);
    pub const NV_ENC_PIC_PARAMS_VER: u32 = struct_version(7) | (1u32 << 31);
    pub const NV_ENC_LOCK_BITSTREAM_VER: u32 = struct_version(2) | (1u32 << 31);
    pub const NV_ENC_CREATE_BITSTREAM_BUFFER_VER: u32 = struct_version(1);
    pub const NV_ENC_EVENT_PARAMS_VER: u32 = struct_version(1);

    /// Build a `GUID` from the Microsoft `{D1-D2-D3-D4...}` notation used
    /// in the C header. We can't `const fn` the windows-rs `GUID::from_u128`
    /// because we use bindgen's `_GUID` here (different layout), so we
    /// just spell each one out.
    const fn guid(d1: u32, d2: u16, d3: u16, d4: [u8; 8]) -> GUID {
        GUID {
            Data1: d1 as ::std::os::raw::c_ulong,
            Data2: d2 as ::std::os::raw::c_ushort,
            Data3: d3 as ::std::os::raw::c_ushort,
            Data4: d4,
        }
    }

    // {6BC82762-4E63-4ca4-AA85-1E50F321F6BF}
    pub const NV_ENC_CODEC_H264_GUID: GUID = guid(
        0x6bc82762,
        0x4e63,
        0x4ca4,
        [0xaa, 0x85, 0x1e, 0x50, 0xf3, 0x21, 0xf6, 0xbf],
    );
    // {790CDC88-4522-4d7b-9425-BDA9975F7603}
    pub const NV_ENC_CODEC_HEVC_GUID: GUID = guid(
        0x790cdc88,
        0x4522,
        0x4d7b,
        [0x94, 0x25, 0xbd, 0xa9, 0x97, 0x5f, 0x76, 0x03],
    );

    // H.264 profiles
    // {0727BCAA-78C4-4c83-8C2F-EF3DFF267C6A}
    pub const NV_ENC_H264_PROFILE_BASELINE_GUID: GUID = guid(
        0x0727bcaa,
        0x78c4,
        0x4c83,
        [0x8c, 0x2f, 0xef, 0x3d, 0xff, 0x26, 0x7c, 0x6a],
    );
    // {60B5C1D4-67FE-4790-94D5-C4726D7B6E6D}
    pub const NV_ENC_H264_PROFILE_MAIN_GUID: GUID = guid(
        0x60b5c1d4,
        0x67fe,
        0x4790,
        [0x94, 0xd5, 0xc4, 0x72, 0x6d, 0x7b, 0x6e, 0x6d],
    );
    // {E7CBC309-4F7A-4b89-AF2A-D537C92BE310}
    pub const NV_ENC_H264_PROFILE_HIGH_GUID: GUID = guid(
        0xe7cbc309,
        0x4f7a,
        0x4b89,
        [0xaf, 0x2a, 0xd5, 0x37, 0xc9, 0x2b, 0xe3, 0x10],
    );
    // {BFD6F8E7-233C-4341-8B3E-4818523803F4}
    pub const NV_ENC_CODEC_PROFILE_AUTOSELECT_GUID: GUID = guid(
        0xbfd6f8e7,
        0x233c,
        0x4341,
        [0x8b, 0x3e, 0x48, 0x18, 0x52, 0x38, 0x03, 0xf4],
    );

    // P1 (fastest) → P7 (highest quality) presets, NVENC SDK ≥ 11.
    // {FC0A8D3E-45F8-4CF8-80C7-298871590EBF}
    pub const NV_ENC_PRESET_P1_GUID: GUID = guid(
        0xfc0a8d3e,
        0x45f8,
        0x4cf8,
        [0x80, 0xc7, 0x29, 0x88, 0x71, 0x59, 0x0e, 0xbf],
    );
    // {F581CFB8-88D6-4381-93F0-DF13F9C27DAB}
    pub const NV_ENC_PRESET_P2_GUID: GUID = guid(
        0xf581cfb8,
        0x88d6,
        0x4381,
        [0x93, 0xf0, 0xdf, 0x13, 0xf9, 0xc2, 0x7d, 0xab],
    );
    // {36850110-3A07-441F-94D5-3670631F91F6}
    pub const NV_ENC_PRESET_P3_GUID: GUID = guid(
        0x36850110,
        0x3a07,
        0x441f,
        [0x94, 0xd5, 0x36, 0x70, 0x63, 0x1f, 0x91, 0xf6],
    );
    // {90A7B826-DF06-4862-B9D2-CD6D73A08681}
    pub const NV_ENC_PRESET_P4_GUID: GUID = guid(
        0x90a7b826,
        0xdf06,
        0x4862,
        [0xb9, 0xd2, 0xcd, 0x6d, 0x73, 0xa0, 0x86, 0x81],
    );
    // {21C6E6B4-297A-4CBA-998F-B6CBDE72ADE3}
    pub const NV_ENC_PRESET_P5_GUID: GUID = guid(
        0x21c6e6b4,
        0x297a,
        0x4cba,
        [0x99, 0x8f, 0xb6, 0xcb, 0xde, 0x72, 0xad, 0xe3],
    );
    // {8E75C279-6299-4AB6-8302-0B215A335CF5}
    pub const NV_ENC_PRESET_P6_GUID: GUID = guid(
        0x8e75c279,
        0x6299,
        0x4ab6,
        [0x83, 0x02, 0x0b, 0x21, 0x5a, 0x33, 0x5c, 0xf5],
    );
    // {84848C12-6F71-4C13-931B-53E283F57974}
    pub const NV_ENC_PRESET_P7_GUID: GUID = guid(
        0x84848c12,
        0x6f71,
        0x4c13,
        [0x93, 0x1b, 0x53, 0xe2, 0x83, 0xf5, 0x79, 0x74],
    );
}

#[cfg(windows)]
pub use supplements::*;
