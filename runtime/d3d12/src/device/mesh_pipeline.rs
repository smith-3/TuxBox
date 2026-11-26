//! Native creation of console mesh pipelines from an owned recipe.
//!
//! A recipe holds everything the host call reads: the patched stream, the
//! shader bytecode its fields point at, and the root signature the stream
//! names. The native pipeline can therefore be created during the title's
//! `CreatePipelineState` or later, from the same code path. A recipe made
//! during that call borrows the guest's and the translator's bytes.
use super::shader_cache;
use core::{ffi::c_void, mem::MaybeUninit};
use std::borrow::Cow;
use windows::Win32::Foundation::E_INVALIDARG;
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;

/// One shader field of the stream and the bytecode it is given.
struct Field<'a> {
    offset: usize,
    code: Cow<'a, [u8]>,
}

/// What retranslating the mesh program for its pixel consumer needs, after
/// the host rejected the ordinary pair.
pub(super) struct Relink<'a> {
    pub(super) mesh_guest: Cow<'a, [u8]>,
    pub(super) root_payload: Option<Cow<'a, [u8]>>,
    pub(super) pixel_digest: String,
    pub(super) pixel_source: Cow<'a, [u8]>,
}

pub(super) struct MeshRecipe<'a> {
    storage: Box<[MaybeUninit<usize>]>,
    size: usize,
    mesh: Field<'a>,
    pixel: Option<Field<'a>>,
    amplification: Option<Field<'a>>,
    /// Keeps the root signature the stream points at alive.
    _root: Option<ID3D12RootSignature>,
    relink: Option<Relink<'a>>,
    hashes: [Option<(String, bool)>; 8],
    label: String,
}

impl<'a> MeshRecipe<'a> {
    /// Copy a prepared mesh stream and the shaders its fields point at.
    ///
    /// # Safety
    /// `stream` holds `size` initialized bytes, and every field pointer and
    /// the root record point into it at shader and root payloads. The
    /// bytecode those fields reference stays live and unchanged for `'a`.
    pub(super) unsafe fn capture(
        stream: *const u8,
        size: usize,
        fields: &[Option<*mut D3D12_SHADER_BYTECODE>; 8],
        root_record: Option<*mut *mut c_void>,
        relink: Option<Relink<'a>>,
        hashes: [Option<(String, bool)>; 8],
        label: String,
    ) -> Option<Self> {
        let field = |index: usize| -> Option<Field<'a>> {
            let pointer = fields[index]?;
            // SAFETY: the caller's field pointers address shader payloads.
            let shader = unsafe { pointer.read() };
            if shader.pShaderBytecode.is_null() || shader.BytecodeLength == 0 {
                return None;
            }
            // SAFETY: a non-empty field references bytecode of that length,
            // live for `'a` by the caller's contract.
            let code: &'a [u8] = unsafe { core::slice::from_raw_parts(shader.pShaderBytecode.cast::<u8>(), shader.BytecodeLength) };
            Some(Field { offset: pointer.addr() - stream.addr(), code: Cow::Borrowed(code) })
        };
        let mesh = field(7)?;
        let words = size.div_ceil(size_of::<usize>());
        let mut storage = vec![MaybeUninit::<usize>::uninit(); words].into_boxed_slice();
        // SAFETY: `stream` holds `size` bytes, and the copy fits `storage`.
        unsafe { core::ptr::copy_nonoverlapping(stream, storage.as_mut_ptr().cast::<u8>(), size) };
        // SAFETY: the root record holds a native root the guest keeps alive for
        // this call. Cloning takes a reference of our own.
        let root = root_record.and_then(|record| unsafe {
            let raw = record.read();
            ID3D12RootSignature::from_raw_borrowed(&raw).cloned()
        });
        Some(Self {
            storage,
            size,
            mesh,
            pixel: field(1),
            amplification: field(6),
            _root: root,
            relink,
            hashes,
            label,
        })
    }

    /// Create the native pipeline. A pair the host rejects as incompatible is
    /// retried once with the mesh program retranslated for this pixel stage.
    pub(super) fn materialize(&self, host: &ID3D12Device2) -> windows::core::Result<ID3D12PipelineState> {
        let mut storage = self.storage.clone();
        let base = storage.as_mut_ptr().cast::<u8>();
        let bind = |offset: usize, code: &[u8]| {
            // SAFETY: `offset` addresses a shader payload inside `storage`, and
            // `code` outlives every native call made with this stream.
            unsafe { base.add(offset).cast::<D3D12_SHADER_BYTECODE>().write(super::rectangle::bytecode(code)) };
        };
        for field in [Some(&self.mesh), self.pixel.as_ref(), self.amplification.as_ref()].into_iter().flatten() {
            bind(field.offset, &field.code);
        }
        let desc = D3D12_PIPELINE_STATE_STREAM_DESC {
            SizeInBytes: self.size,
            pPipelineStateSubobjectStream: base.cast(),
        };
        let initial = create_native_mesh(host, &desc, &self.label);
        let Err(error) = &initial else { return initial };
        let status = error.code();
        if status == E_INVALIDARG
            && self.amplification.is_none()
            && let (Some(relink), Some(pixel)) = (&self.relink, &self.pixel)
        {
            let source = shader_cache::mesh_link_source(&relink.mesh_guest, relink.root_payload.as_deref());
            let pixel_stage = (relink.pixel_digest.as_str(), &*relink.pixel_source);
            if let Some(linked) = shader_cache::link_mesh(&source, pixel_stage, &self.mesh.code, &pixel.code) {
                bind(self.mesh.offset, &linked);
                let retry = create_native_mesh(host, &desc, &format!("linked {}", self.label));
                match &retry {
                    Ok(_) => {
                        static LINKED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                        let n = LINKED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if n < 8 || n.is_power_of_two() {
                            crate::diagnostics::log(&format!("linked slot 86 MS for PS={} (#{})", relink.pixel_digest, n + 1));
                        }
                    }
                    Err(error) => crate::diagnostics::pipeline_stages(&self.hashes, error.code().0, true),
                }
                return retry;
            }
        }
        crate::diagnostics::pipeline_stages(&self.hashes, status.0, false);
        initial
    }
}

/// Create one native mesh pipeline under the mesh compile limit.
fn create_native_mesh(
    host: &ID3D12Device2,
    desc: &D3D12_PIPELINE_STATE_STREAM_DESC,
    label: &str,
) -> windows::core::Result<ID3D12PipelineState> {
    use std::{sync::LazyLock, time::Instant};
    static CLOCK: LazyLock<Option<Instant>> = LazyLock::new(|| {
        (std::env::var_os("TUXBOX_TRACE_MESH_PSO").as_deref() == Some(std::ffi::OsStr::new("1")))
            .then(Instant::now)
    });
    let clock = CLOCK.as_ref();
    let arrival = clock.map(Instant::elapsed);
    let slot = crate::compile_gate::mesh_compile();
    let start = clock.map(Instant::elapsed);
    // SAFETY: The caller keeps the stream and every borrowed subobject live
    // through the synchronous native call, including the helper thread's join.
    let result = crate::big_stack::run_logged(label, || unsafe { host.CreatePipelineState(desc) });
    let end = clock.map(Instant::elapsed);
    let gated = slot.is_some();
    drop(slot);
    if let (Some(arrival), Some(start), Some(end)) = (arrival, start, end) {
        let hr = result.as_ref().err().map_or(0, |error| error.code().0 as u32);
        crate::diagnostics::log(&format!(
            "mesh native PSO {label} gated={} arrival_ns={} start_ns={} end_ns={} hr={hr:08x}",
            u8::from(gated), arrival.as_nanos(), start.as_nanos(), end.as_nanos()
        ));
    }
    result
}
