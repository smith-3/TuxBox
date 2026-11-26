//! Create console mesh pipelines when a command list first binds them.
//!
//! ProjectScarlett creates thousands of mesh pipelines while it loads and binds
//! few of them: one MatrixVI boot spent 1,303 s of driver time in 15,861 native
//! mesh creations, with none bound before the loading screen. With
//! `TUXBOX_MESH_LAZY=1` an admitted creation returns at once. The proxy keeps a
//! spooled recipe and wraps a shared placeholder, and the native pipeline is
//! created on first bind. A pipeline the host then rejects is replaced by
//! resource-free shaders on the same stream, so its draws emit nothing.
//!
//! The placeholder is shared, so a lazy pipeline keeps its own private data
//! instead of forwarding it (see [`PrivateData`]).
use super::Device;
use super::mesh_pipeline::MeshRecipe;
use core::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{E_INVALIDARG, S_OK};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::{DXGI_ERROR_MORE_DATA, DXGI_ERROR_NOT_FOUND};
use windows::core::{GUID, HRESULT, IUnknown, Interface};

pub(super) fn enabled() -> bool {
    static ENABLED: LazyLock<bool> = LazyLock::new(|| {
        std::env::var_os("TUXBOX_MESH_LAZY").as_deref() == Some(std::ffi::OsStr::new("1"))
    });
    *ENABLED
}

/// A mesh pipeline whose native object is created on first bind.
pub(crate) struct LazyMesh {
    recipe: Mutex<Option<MeshRecipe<'static>>>,
    created: OnceLock<Option<ID3D12PipelineState>>,
    host: ID3D12Device2,
    pub(crate) private: PrivateData,
}

impl LazyMesh {
    /// Defer `recipe`, or `None` when it cannot outlive the title's call.
    pub(super) fn defer(recipe: &MeshRecipe<'_>, host: &ID3D12Device2) -> Option<Self> {
        let recipe = recipe.spooled()?;
        Stats::get().deferred.fetch_add(1, Ordering::Relaxed);
        Stats::report();
        Some(Self {
            recipe: Mutex::new(Some(recipe)),
            created: OnceLock::new(),
            host: host.clone(),
            private: PrivateData::default(),
        })
    }

    /// The native pipeline, created by the first caller. Concurrent first
    /// binds wait for that one creation.
    pub(crate) fn native(&self) -> Option<ID3D12PipelineState> {
        self.created.get_or_init(|| {
            let recipe = self.recipe.lock().unwrap_or_else(|error| error.into_inner()).take()?;
            let started = Instant::now();
            let created = match recipe.materialize(&self.host) {
                Ok(native) => {
                    Stats::get().materialized.fetch_add(1, Ordering::Relaxed);
                    Some(native)
                }
                Err(error) => {
                    Stats::get().failed.fetch_add(1, Ordering::Relaxed);
                    crate::progress::stand_in();
                    let null = recipe.materialize_null(&self.host);
                    crate::diagnostics::log(&format!(
                        "{} failed on first use: {:#010x}; {}", recipe.label(), error.code().0,
                        if null.is_ok() { "its draws emit nothing" } else { "no stand-in either" }
                    ));
                    null.ok()
                }
            };
            let waited = started.elapsed();
            Stats::get().waited_us.fetch_add(waited.as_micros() as u64, Ordering::Relaxed);
            if waited >= Duration::from_millis(100) {
                crate::diagnostics::log(&format!("first use of {} waited {}ms for its mesh pipeline", recipe.label(), waited.as_millis()));
            }
            Stats::report();
            created
        }).clone()
    }
}

/// The object a lazy pipeline's proxy wraps until first bind: one resource-free
/// compute pipeline per device. It is never bound for a draw.
pub(super) fn placeholder(device: &Device) -> Option<ID3D12PipelineState> {
    device.state.mesh_placeholder.get_or_init(|| {
        let code = super::fallback::shader(super::fallback::Stage::Compute)?;
        let desc = D3D12_ROOT_SIGNATURE_DESC::default();
        let mut blob = None;
        // SAFETY: the empty description and the blob slot are live for the call.
        unsafe { D3D12SerializeRootSignature(&desc, D3D_ROOT_SIGNATURE_VERSION_1, &mut blob, None) }.ok()?;
        let blob = blob?;
        // SAFETY: the blob owns its buffer for this call.
        let bytes = unsafe { core::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize()) };
        let root: ID3D12RootSignature = unsafe { device.native.CreateRootSignature(0, bytes) }.ok()?;
        let pipeline = D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: core::mem::ManuallyDrop::new(Some(root)),
            CS: super::fallback::bytecode(code),
            ..Default::default()
        };
        let mut pipeline = pipeline;
        // SAFETY: the description, its root and its shader are live for the call.
        let native = unsafe { device.native.CreateComputePipelineState(&pipeline) };
        unsafe { core::mem::ManuallyDrop::drop(&mut pipeline.pRootSignature) };
        native.ok()
    }).clone()
}

#[derive(Default)]
struct Stats {
    deferred: AtomicU64,
    materialized: AtomicU64,
    failed: AtomicU64,
    waited_us: AtomicU64,
}

impl Stats {
    fn get() -> &'static Self {
        static STATS: LazyLock<Stats> = LazyLock::new(Stats::default);
        &STATS
    }

    /// Log the counts whenever the number of deferred or created pipelines
    /// reaches a power of two.
    fn report() {
        let stats = Self::get();
        let deferred = stats.deferred.load(Ordering::Relaxed);
        let used = stats.materialized.load(Ordering::Relaxed) + stats.failed.load(Ordering::Relaxed);
        let due = |n: u64| n != 0 && n.is_power_of_two();
        if due(deferred) || due(used) {
            crate::diagnostics::log(&format!(
                "mesh lazy deferred={deferred} materialized={} failed={} first_use_ms={}",
                stats.materialized.load(Ordering::Relaxed), stats.failed.load(Ordering::Relaxed),
                stats.waited_us.load(Ordering::Relaxed) / 1000,
            ));
        }
    }
}

/// `ID3D12Object` private data kept on the proxy, with the semantics of the
/// native methods: a copy of bytes, or a reference to an interface.
#[derive(Default)]
pub(crate) struct PrivateData(Mutex<Vec<(GUID, Value)>>);

enum Value {
    Bytes(Vec<u8>),
    Interface(IUnknown),
}

impl PrivateData {
    /// # Safety
    /// `size` is null or writable; when it holds at least the stored size,
    /// `data` is null or has room for that many bytes.
    pub(crate) unsafe fn get(&self, key: &GUID, size: *mut u32, data: *mut c_void) -> HRESULT {
        let Some(size) = (unsafe { size.as_mut() }) else { return E_INVALIDARG };
        let entries = self.0.lock().unwrap_or_else(|error| error.into_inner());
        let Some((_, value)) = entries.iter().find(|(stored, _)| stored == key) else {
            *size = 0;
            return DXGI_ERROR_NOT_FOUND;
        };
        let stored = match value {
            Value::Bytes(bytes) => bytes.len() as u32,
            Value::Interface(_) => size_of::<*mut c_void>() as u32,
        };
        if data.is_null() {
            *size = stored;
            return S_OK;
        }
        if *size < stored {
            *size = stored;
            return DXGI_ERROR_MORE_DATA;
        }
        *size = stored;
        match value {
            // SAFETY: the caller's buffer holds at least `stored` bytes.
            Value::Bytes(bytes) => unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), data.cast(), bytes.len()) },
            // SAFETY: as above; the caller receives a reference of its own.
            Value::Interface(object) => unsafe { data.cast::<*mut c_void>().write(object.clone().into_raw()) },
        }
        S_OK
    }

    /// # Safety
    /// `data` is null or readable for `size` bytes.
    pub(crate) unsafe fn set(&self, key: &GUID, size: u32, data: *const c_void) -> HRESULT {
        let value = (!data.is_null() && size != 0).then(|| {
            // SAFETY: the caller's buffer holds `size` bytes.
            Value::Bytes(unsafe { core::slice::from_raw_parts(data.cast::<u8>(), size as usize) }.to_vec())
        });
        self.replace(key, value);
        S_OK
    }

    /// # Safety
    /// `object` is null or a live COM interface.
    pub(crate) unsafe fn set_interface(&self, key: &GUID, object: *mut c_void) -> HRESULT {
        // SAFETY: a non-null pointer is a live interface; cloning takes a reference.
        let value = unsafe { IUnknown::from_raw_borrowed(&object) }.cloned().map(Value::Interface);
        self.replace(key, value);
        S_OK
    }

    fn replace(&self, key: &GUID, value: Option<Value>) {
        let mut entries = self.0.lock().unwrap_or_else(|error| error.into_inner());
        entries.retain(|(stored, _)| stored != key);
        if let Some(value) = value {
            entries.push((*key, value));
        }
    }
}

#[cfg(test)]
#[path = "../tests/device/mesh_lazy.rs"]
mod tests;
