//! Compile provider-side helper shaders with the desktop `dxcompiler.dll`.
//!
//! DXC stays optional: applications that never need a Shader Model 6 helper
//! never load it. The compiler and every blob are released before the module
//! is unloaded.
use core::{ffi::c_void, ptr, slice};
use std::sync::Arc;
use windows::Win32::Foundation::{E_NOTIMPL, FreeLibrary};
use windows::Win32::Graphics::Direct3D::Dxc::*;
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_APPLICATION_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows::core::{GUID, HRESULT, Interface, PCWSTR, Result, s, w};

/// Compile `hlsl` for `profile` (such as `w!("gs_6_0")`) at `entry`. `purpose`
/// names the helper in the one log line a missing compiler produces.
pub(super) fn compile(hlsl: &str, profile: PCWSTR, entry: PCWSTR, purpose: &str) -> Result<Arc<[u8]>> {
    type Create = unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut c_void) -> HRESULT;
    let module = unsafe {
        LoadLibraryExW(
            w!("dxcompiler.dll"),
            None,
            LOAD_LIBRARY_SEARCH_APPLICATION_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
        ).inspect_err(|error| {
            static FAILED: std::sync::Once = std::sync::Once::new();
            crate::diagnostics::log_once(&FAILED, || format!(
                "{purpose} shader: dxcompiler.dll load failed {:#010x}; stage matching x64 dxcompiler.dll and dxil.dll",
                error.code().0
            ));
        })?
    };
    let result = (|| unsafe {
        let entry_point = GetProcAddress(module, s!("DxcCreateInstance")).ok_or(E_NOTIMPL)?;
        let create: Create = core::mem::transmute(entry_point);
        let mut raw = ptr::null_mut();
        create(&CLSID_DxcCompiler, &IDxcCompiler3::IID, &mut raw).ok()?;
        let compiler = IDxcCompiler3::from_raw(raw);
        let source = DxcBuffer {
            Ptr: hlsl.as_ptr().cast(),
            Size: hlsl.len(),
            Encoding: DXC_CP_UTF8.0,
        };
        let result: IDxcResult = compiler.Compile(&source, Some(&[w!("-T"), profile, w!("-E"), entry]), None)?;
        result.GetStatus()?.ok()?;
        let blob = result.GetResult()?;
        Ok(Arc::from(slice::from_raw_parts(
            blob.GetBufferPointer().cast::<u8>(),
            blob.GetBufferSize(),
        )))
    })();
    let _ = unsafe { FreeLibrary(module) };
    result
}
