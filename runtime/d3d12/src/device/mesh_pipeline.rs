//! Native creation of console mesh pipelines from a recipe.
//!
//! A recipe holds everything the host call reads: the patched stream, the
//! shader bytecode its fields point at, and the root signature the stream
//! names. The native pipeline can therefore be created during the title's
//! `CreatePipelineState` or later, from the same code path. A recipe made
//! during that call borrows the guest's and the translator's bytes. A recipe
//! kept for later stores them on disk by content (see [`MeshRecipe::spooled`]).
use super::shader_cache;
use core::{ffi::c_void, mem::MaybeUninit};
use std::borrow::Cow;
use windows::Win32::Foundation::{E_FAIL, E_INVALIDARG};
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;

/// Bytes a recipe needs: borrowed during the title's call, or stored on disk.
enum Code<'a> {
    Borrowed(&'a [u8]),
    Spooled(spool::Entry),
}

impl Code<'_> {
    fn bytes(&self) -> Option<Cow<'_, [u8]>> {
        match self {
            Self::Borrowed(bytes) => Some(Cow::Borrowed(bytes)),
            Self::Spooled(entry) => entry.load().map(Cow::Owned),
        }
    }

    fn spooled(&self) -> Option<Code<'static>> {
        Some(Code::Spooled(match self {
            Self::Borrowed(bytes) => spool::store(bytes)?,
            Self::Spooled(entry) => entry.clone(),
        }))
    }
}

/// One shader field of the stream and the bytecode it is given.
struct Field<'a> {
    offset: usize,
    code: Code<'a>,
}

impl Field<'_> {
    fn spooled(&self) -> Option<Field<'static>> {
        Some(Field { offset: self.offset, code: self.code.spooled()? })
    }
}

/// What retranslating the mesh program for its pixel consumer needs, after
/// the host rejected the ordinary pair.
pub(super) struct Relink<'a> {
    mesh_guest: Code<'a>,
    root_payload: Option<Code<'a>>,
    pixel_digest: String,
    pixel_source: Code<'a>,
}

impl<'a> Relink<'a> {
    pub(super) fn new(mesh_guest: &'a [u8], root_payload: Option<&'a [u8]>, pixel_digest: String, pixel_source: &'a [u8]) -> Self {
        Self {
            mesh_guest: Code::Borrowed(mesh_guest),
            root_payload: root_payload.map(Code::Borrowed),
            pixel_digest,
            pixel_source: Code::Borrowed(pixel_source),
        }
    }

    fn spooled(&self) -> Option<Relink<'static>> {
        Some(Relink {
            mesh_guest: self.mesh_guest.spooled()?,
            root_payload: match &self.root_payload {
                Some(root) => Some(root.spooled()?),
                None => None,
            },
            pixel_digest: self.pixel_digest.clone(),
            pixel_source: self.pixel_source.spooled()?,
        })
    }
}

pub(super) struct MeshRecipe<'a> {
    storage: Box<[MaybeUninit<usize>]>,
    size: usize,
    mesh: Field<'a>,
    pixel: Option<Field<'a>>,
    amplification: Option<Field<'a>>,
    /// Keeps the root signature the stream points at alive.
    root: Option<ID3D12RootSignature>,
    /// No subobject reads memory outside the stream, its shaders and its root.
    self_contained: bool,
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
        foreign_pointers: bool,
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
            Some(Field { offset: pointer.addr() - stream.addr(), code: Code::Borrowed(code) })
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
        let only_mesh_stages = fields.iter().enumerate().all(|(index, field)| field.is_none() || matches!(index, 1 | 6 | 7));
        Some(Self {
            storage,
            size,
            mesh,
            pixel: field(1),
            amplification: field(6),
            root,
            self_contained: only_mesh_stages && !foreign_pointers,
            relink,
            hashes,
            label,
        })
    }

    /// The recipe with every byte it borrows stored on disk, so it outlives
    /// the title's call without holding shader bytes in memory. `None` when
    /// the stream points at guest memory, has an amplification stage, or the
    /// spool cannot be written.
    pub(super) fn spooled(&self) -> Option<MeshRecipe<'static>> {
        if !self.self_contained || self.amplification.is_some() {
            return None;
        }
        Some(MeshRecipe {
            storage: self.storage.clone(),
            size: self.size,
            mesh: self.mesh.spooled()?,
            pixel: match &self.pixel {
                Some(pixel) => Some(pixel.spooled()?),
                None => None,
            },
            amplification: None,
            root: self.root.clone(),
            self_contained: true,
            relink: match &self.relink {
                Some(relink) => Some(relink.spooled()?),
                None => None,
            },
            hashes: self.hashes.clone(),
            label: self.label.clone(),
        })
    }

    pub(super) fn label(&self) -> &str {
        &self.label
    }

    /// Create the native pipeline. A pair the host rejects as incompatible is
    /// retried once with the mesh program retranslated for this pixel stage.
    pub(super) fn materialize(&self, host: &ID3D12Device2) -> windows::core::Result<ID3D12PipelineState> {
        fn load<'f>(field: &'f Field<'_>, label: &str) -> windows::core::Result<Cow<'f, [u8]>> {
            field.code.bytes().ok_or_else(|| {
                crate::diagnostics::log(&format!("{label} lost its spooled shader bytes"));
                windows::core::Error::from(E_FAIL)
            })
        }
        let load = |field| load(field, &self.label);
        let mesh = load(&self.mesh)?;
        let pixel = self.pixel.as_ref().map(load).transpose()?;
        let amplification = self.amplification.as_ref().map(load).transpose()?;
        let mut storage = self.storage.clone();
        let base = storage.as_mut_ptr().cast::<u8>();
        let bind = |offset: usize, code: &[u8]| {
            // SAFETY: `offset` addresses a shader payload inside `storage`, and
            // `code` outlives every native call made with this stream.
            unsafe { base.add(offset).cast::<D3D12_SHADER_BYTECODE>().write(super::rectangle::bytecode(code)) };
        };
        bind(self.mesh.offset, &mesh);
        if let (Some(field), Some(code)) = (&self.pixel, &pixel) {
            bind(field.offset, code);
        }
        if let (Some(field), Some(code)) = (&self.amplification, &amplification) {
            bind(field.offset, code);
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
            && let (Some(relink), Some(pixel)) = (&self.relink, &pixel)
            && let (Some(mesh_guest), Some(pixel_source)) = (relink.mesh_guest.bytes(), relink.pixel_source.bytes())
        {
            let root = relink.root_payload.as_ref().and_then(Code::bytes);
            let source = shader_cache::mesh_link_source(&mesh_guest, root.as_deref());
            let pixel_stage = (relink.pixel_digest.as_str(), &*pixel_source);
            if let Some(linked) = shader_cache::link_mesh(&source, pixel_stage, &mesh, pixel) {
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

    /// The same stream with resource-free mesh and pixel shaders (see
    /// `fallback::null_mesh`): draws through it emit no primitives.
    pub(super) fn materialize_null(&self, host: &ID3D12Device2) -> windows::core::Result<ID3D12PipelineState> {
        let (mesh, pixel) = super::fallback::null_mesh().ok_or(E_FAIL)?;
        if self.amplification.is_some() {
            return Err(E_FAIL.into());
        }
        let mut storage = self.storage.clone();
        let base = storage.as_mut_ptr().cast::<u8>();
        for (offset, code) in [(Some(self.mesh.offset), mesh), (self.pixel.as_ref().map(|p| p.offset), pixel)] {
            if let Some(offset) = offset {
                // SAFETY: `offset` addresses a shader payload inside `storage`,
                // and the null shaders are static.
                unsafe { base.add(offset).cast::<D3D12_SHADER_BYTECODE>().write(super::rectangle::bytecode(code)) };
            }
        }
        let desc = D3D12_PIPELINE_STATE_STREAM_DESC {
            SizeInBytes: self.size,
            pPipelineStateSubobjectStream: base.cast(),
        };
        // SAFETY: the stream, its null shaders and its root stay live for this call.
        unsafe { host.CreatePipelineState(&desc) }
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

/// Recipe bytes stored once by content, so thousands of deferred pipelines
/// that share shaders keep one copy on disk and none in memory.
pub(super) mod spool {
    use std::path::PathBuf;
    use std::sync::OnceLock;

    #[derive(Clone)]
    pub(in crate::device) struct Entry {
        digest: String,
        len: usize,
    }

    fn directory() -> Option<&'static PathBuf> {
        static DIRECTORY: OnceLock<Option<PathBuf>> = OnceLock::new();
        DIRECTORY.get_or_init(|| {
            let directory = std::env::temp_dir().join("tuxbox_shader_cache").join("mesh_spool");
            std::fs::create_dir_all(&directory).ok().map(|()| directory)
        }).as_ref()
    }

    pub(in crate::device) fn store(bytes: &[u8]) -> Option<Entry> {
        let directory = directory()?;
        let digest = super::shader_cache::source_digest(bytes)?;
        let path = directory.join(format!("{digest}.bin"));
        if !path.is_file() {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let unique = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let pending = directory.join(format!("{digest}.{}.{unique}.tmp", std::process::id()));
            std::fs::write(&pending, bytes).ok()?;
            // Another thread may publish the same content first. Either copy is right.
            if std::fs::rename(&pending, &path).is_err() {
                let _ = std::fs::remove_file(&pending);
                if !path.is_file() {
                    return None;
                }
            }
        }
        Some(Entry { digest, len: bytes.len() })
    }

    impl Entry {
        /// The stored bytes, if they are still there and unchanged.
        pub(in crate::device) fn load(&self) -> Option<Vec<u8>> {
            let bytes = std::fs::read(directory()?.join(format!("{}.bin", self.digest))).ok()?;
            (bytes.len() == self.len && super::shader_cache::source_digest(&bytes)? == self.digest).then_some(bytes)
        }
    }
}
