//! Create compute and raster stream pipelines when a command list first binds them.
//!
//! With `TUXBOX_PIPELINE_LAZY=1`, compute pipelines (`CreateComputePipelineState`)
//! and non-mesh stream pipelines (`CreatePipelineState`) spool their shader bytes
//! to disk by content hash and return a proxy wrapping the device's shared
//! placeholder. The native pipeline is compiled once on first bind, freeing
//! gigabytes of unused driver/vkd3d pipeline allocations during title startup.
use super::mesh_lazy::{LazyMesh, PrivateData};
use super::mesh_pipeline::spool;
use super::shader_cache;
use core::ffi::c_void;
use core::mem::MaybeUninit;
use std::ffi::{CStr, CString};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{E_FAIL, E_INVALIDARG};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;
use windows::core::{Interface, PCSTR};

pub(super) fn enabled() -> bool {
    static ENABLED: LazyLock<bool> = LazyLock::new(|| {
        std::env::var_os("TUXBOX_PIPELINE_LAZY").as_deref() == Some(std::ffi::OsStr::new("1"))
    });
    *ENABLED
}

/// Any pipeline whose native object is materialized on first bind.
pub(crate) enum LazyPipeline {
    Mesh(LazyMesh),
    Compute(LazyCompute),
    Stream(LazyStream),
}

impl LazyPipeline {
    pub(crate) fn native(&self) -> Option<ID3D12PipelineState> {
        match self {
            Self::Mesh(mesh) => mesh.native(),
            Self::Compute(compute) => compute.native(),
            Self::Stream(stream) => stream.native(),
        }
    }

    pub(crate) fn private(&self) -> &PrivateData {
        match self {
            Self::Mesh(mesh) => &mesh.private,
            Self::Compute(compute) => &compute.private,
            Self::Stream(stream) => &stream.private,
        }
    }
}

struct ComputeRecipe {
    shader: spool::Entry,
    retry: Option<spool::Entry>,
    root: Option<ID3D12RootSignature>,
    node_mask: u32,
    flags: D3D12_PIPELINE_STATE_FLAGS,
    label: String,
}

/// A compute pipeline created on first bind.
pub(crate) struct LazyCompute {
    recipe: Mutex<Option<ComputeRecipe>>,
    created: OnceLock<Option<ID3D12PipelineState>>,
    device: ID3D12Device,
    private: PrivateData,
}

impl LazyCompute {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn defer(
        device: &ID3D12Device,
        root: Option<ID3D12RootSignature>,
        shader: &[u8],
        retry: Option<&[u8]>,
        node_mask: u32,
        flags: D3D12_PIPELINE_STATE_FLAGS,
        label: String,
    ) -> Option<Self> {
        let shader = spool::store(shader)?;
        let retry = match retry {
            Some(bytes) => Some(spool::store(bytes)?),
            None => None,
        };
        let deferred = Stats::get().deferred.fetch_add(1, Ordering::Relaxed) + 1;
        if deferred.is_power_of_two() {
            Stats::report();
        }
        Some(Self {
            recipe: Mutex::new(Some(ComputeRecipe {
                shader,
                retry,
                root,
                node_mask,
                flags,
                label,
            })),
            created: OnceLock::new(),
            device: device.clone(),
            private: PrivateData::default(),
        })
    }

    fn native(&self) -> Option<ID3D12PipelineState> {
        self.created
            .get_or_init(|| {
                let recipe = self
                    .recipe
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take()?;
                let started = Instant::now();
                let created = match recipe.materialize(&self.device) {
                    Ok(native) => {
                        Stats::get().materialized.fetch_add(1, Ordering::Relaxed);
                        Some(native)
                    }
                    Err(error) => {
                        Stats::get().failed.fetch_add(1, Ordering::Relaxed);
                        crate::progress::stand_in();
                        let null = recipe.materialize_null(&self.device);
                        crate::diagnostics::log(&format!(
                            "{} failed on first use: {:#010x}; {}",
                            recipe.label,
                            error.code().0,
                            if null.is_ok() {
                                "using null compute stand-in"
                            } else {
                                "no stand-in either"
                            }
                        ));
                        null.ok()
                    }
                };
                record_wait(&recipe.label, started.elapsed());
                created
            })
            .clone()
    }
}

impl ComputeRecipe {
    fn materialize(&self, device: &ID3D12Device) -> windows::core::Result<ID3D12PipelineState> {
        let shader = self.shader.load().ok_or_else(|| {
            crate::diagnostics::log(&format!("{} lost its spooled shader bytes", self.label));
            windows::core::Error::from(E_FAIL)
        })?;
        let label = self.label.clone();
        let compile = |bytes: Vec<u8>| {
            let name = label.clone();
            let device = device.clone();
            let root = self.root.clone();
            let node_mask = self.node_mask;
            let flags = self.flags;
            crate::big_stack::run_logged(&label, move || {
                let _slot = crate::compile_gate::large_compile(bytes.len(), |slots| {
                    crate::diagnostics::log(&format!(
                        "{name} waits for one of {slots} large compile slots"
                    ));
                });
                super::compute::owned_compile(device, root, bytes, node_mask, flags)
            })
        };
        let mut result = compile(shader);
        if result.is_err()
            && let Some(retry) = &self.retry
            && let Some(original) = retry.load()
        {
            result = compile(original);
        }
        result
    }

    fn materialize_null(&self, device: &ID3D12Device) -> windows::core::Result<ID3D12PipelineState> {
        use super::fallback::{Stage, enabled, shader};
        if !enabled() {
            return Err(E_FAIL.into());
        }
        let code = shader(Stage::Compute).ok_or(E_FAIL)?;
        crate::big_stack::run_logged("null CS", || {
            super::compute::owned_compile(
                device.clone(),
                self.root.clone(),
                code.to_vec(),
                self.node_mask,
                self.flags,
            )
        })
    }
}

struct InputElement {
    semantic_name: CString,
    semantic_index: u32,
    format: DXGI_FORMAT,
    input_slot: u32,
    aligned_byte_offset: u32,
    input_slot_class: D3D12_INPUT_CLASSIFICATION,
    instance_data_step_rate: u32,
}

impl InputElement {
    unsafe fn capture(desc: &D3D12_INPUT_LAYOUT_DESC) -> Option<Vec<Self>> {
        if desc.NumElements == 0 {
            return Some(Vec::new());
        }
        if desc.pInputElementDescs.is_null() {
            return None;
        }
        let slice =
            unsafe { core::slice::from_raw_parts(desc.pInputElementDescs, desc.NumElements as usize) };
        let mut out = Vec::with_capacity(slice.len());
        for elem in slice {
            if elem.SemanticName.is_null() {
                return None;
            }
            let name = unsafe { CStr::from_ptr(elem.SemanticName.as_ptr().cast()) }.to_owned();
            out.push(Self {
                semantic_name: name,
                semantic_index: elem.SemanticIndex,
                format: elem.Format,
                input_slot: elem.InputSlot,
                aligned_byte_offset: elem.AlignedByteOffset,
                input_slot_class: elem.InputSlotClass,
                instance_data_step_rate: elem.InstanceDataStepRate,
            });
        }
        Some(out)
    }

    fn as_d3d(&self) -> D3D12_INPUT_ELEMENT_DESC {
        D3D12_INPUT_ELEMENT_DESC {
            SemanticName: PCSTR(self.semantic_name.as_ptr().cast()),
            SemanticIndex: self.semantic_index,
            Format: self.format,
            InputSlot: self.input_slot,
            AlignedByteOffset: self.aligned_byte_offset,
            InputSlotClass: self.input_slot_class,
            InstanceDataStepRate: self.instance_data_step_rate,
        }
    }
}

struct RasterRelink {
    producer: usize,
    producer_offset: usize,
    pixel_offset: usize,
    source_digest: String,
    pixel_digest: String,
    producer_guest: spool::Entry,
    pixel_guest: spool::Entry,
}

struct StreamRecipe {
    storage: Box<[MaybeUninit<usize>]>,
    size: usize,
    /// `(stage_index 0..=5, byte offset of D3D12_SHADER_BYTECODE in storage, spooled bytes)`.
    stages: Vec<(usize, usize, spool::Entry)>,
    _root: Option<ID3D12RootSignature>,
    input_layout: Option<(usize, Vec<InputElement>)>,
    view_instancing: Option<(usize, Vec<D3D12_VIEW_INSTANCE_LOCATION>)>,
    rectangles: bool,
    geometry_offset: Option<usize>,
    relink: Option<RasterRelink>,
    hashes: [Option<(String, bool)>; 8],
    label: String,
}

/// A non-mesh stream pipeline (compute or graphics) created on first bind.
pub(crate) struct LazyStream {
    recipe: Mutex<Option<StreamRecipe>>,
    created: OnceLock<Option<ID3D12PipelineState>>,
    host: ID3D12Device2,
    private: PrivateData,
}

impl LazyStream {
    /// Capture and spool a prepared non-mesh pipeline stream so it can be
    /// materialized on first bind.
    ///
    /// # Safety
    /// `stream` holds `size` initialized bytes; all field, root, input-layout,
    /// and view-instancing pointers point into `stream` and their referenced
    /// buffers remain live for this call.
    #[allow(clippy::too_many_arguments)]
    pub(super) unsafe fn defer(
        host: &ID3D12Device2,
        stream: *const u8,
        size: usize,
        fields: &[Option<*mut D3D12_SHADER_BYTECODE>; 8],
        root_record: Option<*mut *mut c_void>,
        input_layout_record: Option<*mut D3D12_INPUT_LAYOUT_DESC>,
        view_instancing_record: Option<*mut D3D12_VIEW_INSTANCING_DESC>,
        rectangles: bool,
        geometry_record: *mut D3D12_SHADER_BYTECODE,
        original: &[Option<(&[u8], Option<usize>)>; 8],
        hashes: &[Option<(String, bool)>; 8],
        label: String,
    ) -> Option<Self> {
        let base_addr = stream.addr();
        let mut spooled_stages = Vec::new();
        for (index, field) in fields[..6].iter().enumerate() {
            let Some(pointer) = *field else { continue };
            let shader = unsafe { pointer.read() };
            if shader.pShaderBytecode.is_null() || shader.BytecodeLength == 0 {
                continue;
            }
            let bytes = unsafe {
                core::slice::from_raw_parts(shader.pShaderBytecode.cast::<u8>(), shader.BytecodeLength)
            };
            spooled_stages.push((index, pointer.addr() - base_addr, spool::store(bytes)?));
        }
        let geometry_offset = if !geometry_record.is_null() {
            let offset = geometry_record.addr() - base_addr;
            if rectangles && !spooled_stages.iter().any(|&(_, off, _)| off == offset) {
                let shader = unsafe { geometry_record.read() };
                if !shader.pShaderBytecode.is_null() && shader.BytecodeLength != 0 {
                    let bytes = unsafe {
                        core::slice::from_raw_parts(
                            shader.pShaderBytecode.cast::<u8>(),
                            shader.BytecodeLength,
                        )
                    };
                    spooled_stages.push((4, offset, spool::store(bytes)?));
                }
            }
            Some(offset)
        } else {
            None
        };
        let input_layout = match input_layout_record {
            Some(record) => {
                let desc = unsafe { &*record };
                if desc.NumElements != 0 {
                    Some((record.addr() - base_addr, unsafe { InputElement::capture(desc) }?))
                } else {
                    None
                }
            }
            None => None,
        };
        let view_instancing = match view_instancing_record {
            Some(record) => {
                let desc = unsafe { &*record };
                if desc.ViewInstanceCount != 0 {
                    if desc.pViewInstanceLocations.is_null() {
                        return None;
                    }
                    let locs = unsafe {
                        core::slice::from_raw_parts(
                            desc.pViewInstanceLocations,
                            desc.ViewInstanceCount as usize,
                        )
                    }
                    .to_vec();
                    Some((record.addr() - base_addr, locs))
                } else {
                    None
                }
            }
            None => None,
        };
        let root = root_record.and_then(|record| unsafe {
            let raw = record.read();
            ID3D12RootSignature::from_raw_borrowed(&raw).cloned()
        });
        let producer = if fields[4].is_some() { 4 } else { 0 };
        let direct = fields[5..].iter().all(Option::is_none)
            && (producer == 4 || fields[2..4].iter().all(Option::is_none));
        let relink = if direct
            && let (Some(p_field), Some(ps_field)) = (fields[producer], fields[1])
            && let (Some((source_digest, _)), Some((pixel_digest, _))) =
                (&hashes[producer], &hashes[1])
            && let (Some((producer_code, Some(_))), Some((pixel_code, _))) =
                (original[producer], original[1])
        {
            Some(RasterRelink {
                producer,
                producer_offset: p_field.addr() - base_addr,
                pixel_offset: ps_field.addr() - base_addr,
                source_digest: source_digest.clone(),
                pixel_digest: pixel_digest.clone(),
                producer_guest: spool::store(producer_code)?,
                pixel_guest: spool::store(pixel_code)?,
            })
        } else {
            None
        };

        let words = size.div_ceil(size_of::<usize>());
        let mut storage = vec![MaybeUninit::<usize>::uninit(); words].into_boxed_slice();
        unsafe { core::ptr::copy_nonoverlapping(stream, storage.as_mut_ptr().cast::<u8>(), size) };

        let deferred = Stats::get().deferred.fetch_add(1, Ordering::Relaxed) + 1;
        if deferred.is_power_of_two() {
            Stats::report();
        }
        Some(Self {
            recipe: Mutex::new(Some(StreamRecipe {
                storage,
                size,
                stages: spooled_stages,
                _root: root,
                input_layout,
                view_instancing,
                rectangles,
                geometry_offset,
                relink,
                hashes: hashes.clone(),
                label,
            })),
            created: OnceLock::new(),
            host: host.clone(),
            private: PrivateData::default(),
        })
    }

    fn native(&self) -> Option<ID3D12PipelineState> {
        self.created
            .get_or_init(|| {
                let recipe = self
                    .recipe
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take()?;
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
                            "{} failed on first use: {:#010x}; {}",
                            recipe.label,
                            error.code().0,
                            if null.is_ok() {
                                "using null stream stand-in"
                            } else {
                                "no stand-in either"
                            }
                        ));
                        null.ok()
                    }
                };
                record_wait(&recipe.label, started.elapsed());
                created
            })
            .clone()
    }
}

impl StreamRecipe {
    fn prepare_storage<'a>(
        &'a self,
        input_descs: &'a [D3D12_INPUT_ELEMENT_DESC],
    ) -> Box<[MaybeUninit<usize>]> {
        let mut storage = self.storage.clone();
        let base = storage.as_mut_ptr().cast::<u8>();
        if let Some((offset, _)) = &self.input_layout {
            let desc = unsafe { &mut *base.add(*offset).cast::<D3D12_INPUT_LAYOUT_DESC>() };
            desc.pInputElementDescs = input_descs.as_ptr();
            desc.NumElements = input_descs.len() as u32;
        }
        if let Some((offset, locs)) = &self.view_instancing {
            let desc = unsafe { &mut *base.add(*offset).cast::<D3D12_VIEW_INSTANCING_DESC>() };
            desc.pViewInstanceLocations = locs.as_ptr();
            desc.ViewInstanceCount = locs.len() as u32;
        }
        storage
    }

    fn materialize(&self, host: &ID3D12Device2) -> windows::core::Result<ID3D12PipelineState> {
        let mut loaded = Vec::with_capacity(self.stages.len());
        let mut cs_len = 0usize;
        for &(stage, offset, ref entry) in &self.stages {
            let bytes = entry.load().ok_or_else(|| {
                crate::diagnostics::log(&format!("{} lost its spooled shader bytes", self.label));
                windows::core::Error::from(E_FAIL)
            })?;
            if stage == 5 {
                cs_len = bytes.len();
            }
            loaded.push((stage, offset, bytes));
        }
        let input_descs: Vec<D3D12_INPUT_ELEMENT_DESC> = self
            .input_layout
            .as_ref()
            .map(|(_, elems)| elems.iter().map(InputElement::as_d3d).collect())
            .unwrap_or_default();
        let mut storage = self.prepare_storage(&input_descs);
        let base = storage.as_mut_ptr().cast::<u8>();
        for &(_, offset, ref bytes) in &loaded {
            unsafe {
                base.add(offset)
                    .cast::<D3D12_SHADER_BYTECODE>()
                    .write(super::rectangle::bytecode(bytes));
            }
        }
        let desc = D3D12_PIPELINE_STATE_STREAM_DESC {
            SizeInBytes: self.size,
            pPipelineStateSubobjectStream: base.cast(),
        };
        let label = self.label.clone();
        let initial = crate::big_stack::run_logged(&self.label, || {
            let _slot = if cs_len != 0 {
                crate::compile_gate::large_compile(cs_len, |slots| {
                    crate::diagnostics::log(&format!(
                        "{label} waits for one of {slots} large compile slots"
                    ));
                })
            } else {
                None
            };
            unsafe { host.CreatePipelineState(&desc) }
        });
        let Err(error) = &initial else {
            return initial;
        };
        let status = error.code();
        if let Some(relink) = &self.relink {
            let producer_bytes = loaded
                .iter()
                .find(|&&(s, _, _)| s == relink.producer)
                .map(|(_, _, b)| b.as_slice());
            let pixel_bytes = loaded
                .iter()
                .find(|&&(s, _, _)| s == 1)
                .map(|(_, _, b)| b.as_slice());
            let linked = (if relink.producer == 4 {
                shader_cache::linked_geometry(&relink.source_digest, &relink.pixel_digest)
            } else {
                shader_cache::linked_vertex(&relink.source_digest, &relink.pixel_digest)
            })
            .or_else(|| {
                if status != E_INVALIDARG {
                    return None;
                }
                let producer_guest = relink.producer_guest.load()?;
                let pixel_guest = relink.pixel_guest.load()?;
                shader_cache::link_on_demand(
                    (&relink.source_digest, &producer_guest),
                    (&relink.pixel_digest, &pixel_guest),
                    producer_bytes?,
                    pixel_bytes?,
                )
            });
            if let Some(linked) = linked {
                unsafe {
                    base.add(relink.producer_offset)
                        .cast::<D3D12_SHADER_BYTECODE>()
                        .write(super::rectangle::bytecode(&linked));
                }
                let linked_rect;
                if self.rectangles
                    && let Some(geo_off) = self.geometry_offset
                {
                    let vs = unsafe {
                        &*base
                            .add(relink.producer_offset)
                            .cast::<D3D12_SHADER_BYTECODE>()
                    };
                    let ps = unsafe {
                        *base
                            .add(relink.pixel_offset)
                            .cast::<D3D12_SHADER_BYTECODE>()
                    };
                    linked_rect = unsafe { super::rectangle::geometry(vs, &ps) }?;
                    unsafe {
                        base.add(geo_off)
                            .cast::<D3D12_SHADER_BYTECODE>()
                            .write(super::rectangle::bytecode(&linked_rect));
                    }
                }
                let retry = crate::big_stack::run_logged(&format!("linked {}", self.label), || {
                    unsafe { host.CreatePipelineState(&desc) }
                });
                if retry.is_ok() {
                    return retry;
                }
                if let Err(err) = &retry {
                    crate::diagnostics::pipeline_stages(&self.hashes, err.code().0, true);
                }
                return retry;
            }
        }
        crate::diagnostics::pipeline_stages(&self.hashes, status.0, false);
        initial
    }

    fn materialize_null(&self, host: &ID3D12Device2) -> windows::core::Result<ID3D12PipelineState> {
        use super::fallback::{Stage, bytecode, enabled, shader};
        if !enabled() {
            return Err(E_FAIL.into());
        }
        let input_descs: Vec<D3D12_INPUT_ELEMENT_DESC> = self
            .input_layout
            .as_ref()
            .map(|(_, elems)| elems.iter().map(InputElement::as_d3d).collect())
            .unwrap_or_default();
        let mut storage = self.prepare_storage(&input_descs);
        let base = storage.as_mut_ptr().cast::<u8>();
        for &(stage, offset, _) in &self.stages {
            let replacement = match stage {
                0 => bytecode(shader(Stage::Vertex).ok_or(E_FAIL)?),
                1 => bytecode(shader(Stage::Pixel).ok_or(E_FAIL)?),
                5 => bytecode(shader(Stage::Compute).ok_or(E_FAIL)?),
                _ => D3D12_SHADER_BYTECODE::default(),
            };
            unsafe {
                base.add(offset)
                    .cast::<D3D12_SHADER_BYTECODE>()
                    .write(replacement);
            }
        }
        if let Some(geo_off) = self.geometry_offset {
            unsafe {
                base.add(geo_off)
                    .cast::<D3D12_SHADER_BYTECODE>()
                    .write(D3D12_SHADER_BYTECODE::default());
            }
        }
        let desc = D3D12_PIPELINE_STATE_STREAM_DESC {
            SizeInBytes: self.size,
            pPipelineStateSubobjectStream: base.cast(),
        };
        unsafe { host.CreatePipelineState(&desc) }
    }
}

fn record_wait(label: &str, waited: Duration) {
    Stats::get()
        .waited_us
        .fetch_add(waited.as_micros() as u64, Ordering::Relaxed);
    if waited >= Duration::from_millis(100) {
        crate::diagnostics::log(&format!(
            "first use of {label} waited {}ms for its lazy pipeline",
            waited.as_millis()
        ));
    }
    let stats = Stats::get();
    let used = stats.materialized.load(Ordering::Relaxed) + stats.failed.load(Ordering::Relaxed);
    if used != 0 && used.is_power_of_two() {
        Stats::report();
    }
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

    fn report() {
        let stats = Self::get();
        let deferred = stats.deferred.load(Ordering::Relaxed);
        crate::diagnostics::log(&format!(
            "pipeline lazy deferred={deferred} materialized={} failed={} first_use_ms={}",
            stats.materialized.load(Ordering::Relaxed),
            stats.failed.load(Ordering::Relaxed),
            stats.waited_us.load(Ordering::Relaxed) / 1000,
        ));
    }
}
