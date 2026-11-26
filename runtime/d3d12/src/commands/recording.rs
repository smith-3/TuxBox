//! State changes, active segment management, and checks shared by recorded commands.
use super::*;
use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY;

/// Currently open native command list segment and its cached interface casts.
pub(super) struct ActiveList {
    pub(super) native: ID3D12GraphicsCommandList,
    extended: OnceLock<Option<ID3D12GraphicsCommandList1>>,
    enhanced_barriers: OnceLock<Option<ID3D12GraphicsCommandList7>>,
    mesh: OnceLock<Option<ID3D12GraphicsCommandList6>>,
    raytracing: Option<ID3D12GraphicsCommandList4>,
    pub(super) has_commands: bool,
}

impl ActiveList {
    pub(super) fn new(native: ID3D12GraphicsCommandList, compact: bool) -> Self {
        let raytracing = (!compact)
            .then(|| native.cast::<ID3D12GraphicsCommandList4>().ok())
            .flatten();
        Self {
            native,
            extended: OnceLock::new(),
            enhanced_barriers: OnceLock::new(),
            mesh: OnceLock::new(),
            raytracing,
            has_commands: false,
        }
    }

    pub(super) fn extended(&self) -> Option<&ID3D12GraphicsCommandList1> {
        self.extended
            .get_or_init(|| self.native.cast::<ID3D12GraphicsCommandList1>().ok())
            .as_ref()
    }

    pub(super) fn enhanced_barriers(&self) -> Option<&ID3D12GraphicsCommandList7> {
        self.enhanced_barriers
            .get_or_init(|| enhanced_barriers(&self.native))
            .as_ref()
    }

    pub(super) fn mesh(&self) -> Option<&ID3D12GraphicsCommandList6> {
        self.mesh
            .get_or_init(|| self.native.cast::<ID3D12GraphicsCommandList6>().ok())
            .as_ref()
    }

    pub(super) fn raytracing(&self) -> Option<&ID3D12GraphicsCommandList4> {
        self.raytracing.as_ref()
    }
}

#[derive(Clone)]
pub(super) struct Predication {
    pub(super) resource: ID3D12Resource,
    pub(super) offset: u64,
    pub(super) operation: D3D12_PREDICATION_OP,
}

/// CPU-side snapshot of bound RTV and DSV descriptors so mid-list splits on
/// direct lists can replay `OMSetRenderTargets` even if the guest later rewrites
/// its own descriptor heap slots.
struct RenderTargetSnapshot {
    device: ID3D12Device,
    _rtv_heap: ID3D12DescriptorHeap,
    _dsv_heap: ID3D12DescriptorHeap,
    rtv_handles: [D3D12_CPU_DESCRIPTOR_HANDLE; D3D12_SIMULTANEOUS_RENDER_TARGET_COUNT as usize],
    dsv_handle: D3D12_CPU_DESCRIPTOR_HANDLE,
    rtv_count: u32,
    has_dsv: bool,
}

impl RenderTargetSnapshot {
    fn new(device: ID3D12Device) -> windows::core::Result<Self> {
        let create = |kind: D3D12_DESCRIPTOR_HEAP_TYPE, count: u32| -> windows::core::Result<ID3D12DescriptorHeap> {
            let desc = D3D12_DESCRIPTOR_HEAP_DESC {
                Type: kind,
                NumDescriptors: count,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
                NodeMask: 0,
            };
            unsafe { device.CreateDescriptorHeap(&desc) }
        };
        let rtv_heap = create(
            D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
            D3D12_SIMULTANEOUS_RENDER_TARGET_COUNT,
        )?;
        let dsv_heap = create(D3D12_DESCRIPTOR_HEAP_TYPE_DSV, 1)?;
        let stride = unsafe { device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV) } as usize;
        let base = unsafe { rtv_heap.GetCPUDescriptorHandleForHeapStart() }.ptr;
        let mut rtv_handles = [D3D12_CPU_DESCRIPTOR_HANDLE::default();
            D3D12_SIMULTANEOUS_RENDER_TARGET_COUNT as usize];
        for (i, slot) in rtv_handles.iter_mut().enumerate() {
            slot.ptr = base + i * stride;
        }
        let dsv_handle = unsafe { dsv_heap.GetCPUDescriptorHandleForHeapStart() };
        Ok(Self {
            device,
            _rtv_heap: rtv_heap,
            _dsv_heap: dsv_heap,
            rtv_handles,
            dsv_handle,
            rtv_count: 0,
            has_dsv: false,
        })
    }

    fn clear(&mut self) {
        self.rtv_count = 0;
        self.has_dsv = false;
    }

    unsafe fn capture(
        &mut self,
        count: u32,
        rtvs: *const D3D12_CPU_DESCRIPTOR_HANDLE,
        contiguous: bool,
        dsv: *const D3D12_CPU_DESCRIPTOR_HANDLE,
    ) -> bool {
        if count > D3D12_SIMULTANEOUS_RENDER_TARGET_COUNT {
            return false;
        }
        if count > 0 {
            if contiguous {
                let first = unsafe { *rtvs };
                if first.ptr == 0 {
                    return false;
                }
                unsafe {
                    self.device.CopyDescriptorsSimple(
                        count,
                        self.rtv_handles[0],
                        first,
                        D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                    );
                }
            } else {
                for i in 0..count as usize {
                    let src = unsafe { *rtvs.add(i) };
                    if src.ptr == 0 {
                        return false;
                    }
                    unsafe {
                        self.device.CopyDescriptorsSimple(
                            1,
                            self.rtv_handles[i],
                            src,
                            D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                        );
                    }
                }
            }
        }
        let has_dsv = if let Some(src) = unsafe { dsv.as_ref() } {
            if src.ptr == 0 {
                return false;
            }
            unsafe {
                self.device.CopyDescriptorsSimple(
                    1,
                    self.dsv_handle,
                    *src,
                    D3D12_DESCRIPTOR_HEAP_TYPE_DSV,
                );
            }
            true
        } else {
            false
        };
        self.rtv_count = count;
        self.has_dsv = has_dsv;
        true
    }

    unsafe fn replay(&self, native: &ID3D12GraphicsCommandList) {
        if self.rtv_count > 0 || self.has_dsv {
            unsafe {
                native.OMSetRenderTargets(
                    self.rtv_count,
                    (self.rtv_count > 0).then_some(self.rtv_handles.as_ptr()),
                    false,
                    self.has_dsv.then_some(&self.dsv_handle),
                );
            }
        }
    }
}

/// Tracked direct command-list pipeline state replayed across mid-list splits.
#[derive(Default)]
pub(super) struct GraphicsState {
    pub(super) topology: Option<D3D_PRIMITIVE_TOPOLOGY>,
    pub(super) index_buffer: Option<D3D12_INDEX_BUFFER_VIEW>,
    pub(super) vertex_buffers:
        [Option<D3D12_VERTEX_BUFFER_VIEW>; D3D12_IA_VERTEX_INPUT_RESOURCE_SLOT_COUNT as usize],
    pub(super) stream_output:
        [Option<D3D12_STREAM_OUTPUT_BUFFER_VIEW>; D3D12_SO_BUFFER_SLOT_COUNT as usize],
    pub(super) viewports: Vec<D3D12_VIEWPORT>,
    pub(super) scissors: Vec<RECT>,
    pub(super) shading_rate: Option<(D3D12_SHADING_RATE, Option<[D3D12_SHADING_RATE_COMBINER; 2]>)>,
    pub(super) blend_factor: Option<[f32; 4]>,
    pub(super) stencil_ref: Option<u32>,
    pub(super) depth_bounds: Option<(f32, f32)>,
    render_targets: Option<RenderTargetSnapshot>,
    pub(super) rt_unknown: bool,
}

impl GraphicsState {
    pub(super) fn clear(&mut self) {
        self.topology = None;
        self.index_buffer = None;
        self.vertex_buffers = [None; D3D12_IA_VERTEX_INPUT_RESOURCE_SLOT_COUNT as usize];
        self.stream_output = [None; D3D12_SO_BUFFER_SLOT_COUNT as usize];
        self.viewports.clear();
        self.scissors.clear();
        self.shading_rate = None;
        self.blend_factor = None;
        self.stencil_ref = None;
        self.depth_bounds = None;
        if let Some(rt) = &mut self.render_targets {
            rt.clear();
        }
        self.rt_unknown = false;
    }

    pub(super) unsafe fn set_render_targets(
        &mut self,
        native: &ID3D12GraphicsCommandList,
        count: u32,
        rtvs: *const D3D12_CPU_DESCRIPTOR_HANDLE,
        contiguous: bool,
        dsv: *const D3D12_CPU_DESCRIPTOR_HANDLE,
    ) {
        if count == 0 && dsv.is_null() {
            if let Some(rt) = &mut self.render_targets {
                rt.clear();
            }
            self.rt_unknown = false;
            return;
        }
        if self.render_targets.is_none() {
            let mut device: Option<ID3D12Device> = None;
            let created = unsafe { native.GetDevice(&mut device) }
                .ok()
                .and_then(|()| device)
                .and_then(|dev| RenderTargetSnapshot::new(dev).ok());
            self.render_targets = created;
        }
        if let Some(rt) = &mut self.render_targets {
            if unsafe { rt.capture(count, rtvs, contiguous, dsv) } {
                self.rt_unknown = false;
            } else {
                rt.clear();
                self.rt_unknown = true;
            }
        } else {
            self.rt_unknown = true;
        }
    }

    pub(super) unsafe fn replay(&self, active: &ActiveList) {
        let native = &active.native;
        unsafe {
            if let Some(topology) = self.topology {
                native.IASetPrimitiveTopology(topology);
            }
            if let Some(index_buffer) = &self.index_buffer {
                native.IASetIndexBuffer(Some(index_buffer));
            }
            for (slot, view) in self.vertex_buffers.iter().enumerate() {
                if let Some(view) = view {
                    native.IASetVertexBuffers(slot as u32, Some(slice::from_ref(view)));
                }
            }
            for (slot, view) in self.stream_output.iter().enumerate() {
                if let Some(view) = view {
                    (native.vtable().SOSetTargets)(
                        native.as_raw(),
                        slot as u32,
                        1,
                        ptr::from_ref(view),
                    );
                }
            }
            if !self.viewports.is_empty() {
                native.RSSetViewports(&self.viewports);
            }
            if !self.scissors.is_empty() {
                native.RSSetScissorRects(&self.scissors);
            }
            if let Some((rate, combiners)) = &self.shading_rate
                && let Ok(list5) = native.cast::<ID3D12GraphicsCommandList5>()
            {
                list5.RSSetShadingRate(*rate, combiners.as_ref().map(|c| c.as_ptr()));
            }
            if let Some(factor) = &self.blend_factor {
                native.OMSetBlendFactor(Some(factor));
            }
            if let Some(reference) = self.stencil_ref {
                native.OMSetStencilRef(reference);
            }
            if let Some((min, max)) = self.depth_bounds
                && let Some(ext) = active.extended()
            {
                ext.OMSetDepthBounds(min, max);
            }
            if let Some(rt) = &self.render_targets {
                rt.replay(native);
            }
        }
    }
}

impl Recording {
    pub(super) fn native(&self) -> &ID3D12GraphicsCommandList {
        &self.active.native
    }

    pub(super) fn extended(&self) -> Option<&ID3D12GraphicsCommandList1> {
        self.active.extended()
    }

    pub(super) fn enhanced_barriers(&self) -> Option<&ID3D12GraphicsCommandList7> {
        self.active.enhanced_barriers()
    }

    pub(super) fn mesh(&self) -> Option<&ID3D12GraphicsCommandList6> {
        self.active.mesh()
    }

    pub(super) fn raytracing(&self) -> Option<&ID3D12GraphicsCommandList4> {
        self.active.raytracing()
    }

    pub(super) fn record_write(&mut self, address: u64, bytes: u64) {
        self.written_ranges.record(address, bytes);
    }

    pub(super) fn record_unknown_write(&mut self) {
        self.written_ranges.record_unknown();
    }

    pub(super) fn can_split_mid_list(&self) -> bool {
        match self.list_type {
            D3D12_COMMAND_LIST_TYPE_COPY | D3D12_COMMAND_LIST_TYPE_COMPUTE => {
                self.active_queries == 0
            }
            D3D12_COMMAND_LIST_TYPE_DIRECT => {
                self.active_queries == 0
                    && !self.bundle_executed
                    && !self.graphics_state.rt_unknown
            }
            _ => false,
        }
    }

    pub(super) fn split_fallback_reason(&self) -> &'static str {
        if self.list_type == D3D12_COMMAND_LIST_TYPE_BUNDLE {
            "bundle command list"
        } else if self.active_queries != 0 {
            "open non-timestamp query"
        } else if self.bundle_executed {
            "prior ExecuteBundle"
        } else if self.graphics_state.rt_unknown {
            "unsnapshotted render targets"
        } else {
            "unsupported command list type"
        }
    }

    /// Close the currently active native segment, open a fresh segment on the
    /// guest's command allocator, and replay all tracked bound state onto it.
    pub(super) fn split_active(
        &mut self,
    ) -> windows::core::Result<(ID3D12CommandList, ID3D12CommandAllocator)> {
        let mut device: Option<ID3D12Device> = None;
        unsafe { self.active.native.GetDevice(&mut device)? };
        let device = device.ok_or(E_FAIL)?;
        let close = unsafe { self.active.native.Close() };
        self.phase = ListState::Open { native_open: false };
        close?;
        let next: ID3D12GraphicsCommandList =
            unsafe { device.CreateCommandList(0, self.list_type, &self.allocator, None)? };
        let closed: ID3D12CommandList = self.active.native.clone().into();
        self.active = ActiveList::new(next, self.compact);
        self.phase = ListState::Open { native_open: true };
        match self.list_type {
            D3D12_COMMAND_LIST_TYPE_COMPUTE => unsafe {
                self.bindings.replay(&self.active, false)?;
            },
            D3D12_COMMAND_LIST_TYPE_DIRECT => unsafe {
                self.bindings.replay(&self.active, true)?;
                self.graphics_state.replay(&self.active);
            },
            _ => {}
        }
        if let Some(pred) = &self.predication {
            unsafe {
                self.active
                    .native
                    .SetPredication(&pred.resource, pred.offset, pred.operation);
            }
        }
        Ok((closed, self.allocator.clone()))
    }

    pub(super) fn track_texture(
        accesses: &mut Vec<Arc<Texture>>,
        texture: &Arc<Texture>,
    ) -> Result<(), HRESULT> {
        if !accesses.iter().any(|known| Arc::ptr_eq(known, texture)) {
            accesses.try_reserve(1).map_err(|_| E_OUTOFMEMORY)?;
            accesses.push(Arc::clone(texture));
        }
        Ok(())
    }

    pub(super) fn prepare_copy(
        &mut self,
        destination: Option<&Arc<Texture>>,
        source: Option<&Arc<Texture>>,
    ) -> Result<(), HRESULT> {
        for (accesses, texture) in [
            (&mut self.placed_reads, source),
            (&mut self.placed_writes, destination),
        ] {
            if let Some(texture) = texture {
                Self::track_texture(accesses, texture)?;
            }
        }
        Ok(())
    }

    /// Leave out one command TuxBox cannot express. Failing the list instead
    /// would also drop its other work, including the fence writes other queues
    /// wait for, and a stalled queue trips the title's GPU watchdog.
    pub(super) fn skip(what: &str, error: HRESULT) {
        static COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let count = COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        if count <= 16 || count.is_power_of_two() {
            crate::diagnostics::log(&format!(
                "skipped {what} ({:#010x}, #{count}, guest caller {:#x})",
                error.0,
                crate::survey::title_rva().unwrap_or(0),
            ));
        }
    }

    pub(super) fn compute_dispatch(
        &mut self,
        list: &List,
        dispatch: impl FnOnce(&ID3D12GraphicsCommandList),
    ) {
        self.record_unknown_write();
        self.note_shader_fallback();
        let null_usage = self.bindings.null_usage.clone();
        let native = self.active.native.clone();
        let dispatch = move || {
            dispatch(&native);
            if let Some(tag) = &null_usage {
                crate::diagnostics::display::record_null(tag, false);
            }
        };
        if let Some(table) = self.bindings.reflected_table::<true>() {
            if let Err(error) = self.bindings.with_reflected_table::<true>(
                table,
                list,
                &self.active.native,
                self.descriptor_arena.as_ref(),
                &mut self.descriptor_leases,
                self.resource_heap.as_ref(),
                &mut self.sampled_heaps,
                &self.resources,
                dispatch,
            ) {
                Self::skip("compute dispatch with a reflected table", error.code());
            }
            return;
        }
        if self.descriptor_arena.is_some() && !self.bindings.supports_paged_draw::<true>() {
            Self::skip(
                "compute dispatch: pipeline has no paged descriptor layout",
                E_NOTIMPL,
            );
            return;
        }
        if let Err(error) = self
            .bindings
            .refresh_inline_tables::<true>(self.descriptor_arena.as_ref(), &mut self.descriptor_leases)
        {
            Self::skip("compute dispatch: descriptor table refresh", error.code());
            return;
        }
        self.sample_tables::<true>();
        if let Some(root) = self.bindings.sampler_root() {
            unsafe { self.bindings.dispatch_with_root(&list.native, root, dispatch) };
            return;
        }
        let Some(root) = self.bindings.scene_root() else {
            dispatch();
            return;
        };
        let result = self
            .bindings
            .scene_resource(list)
            .and_then(|scene| {
                if let Some([instances, normals]) = &scene.geometry {
                    raytracing::retain(self, &[&scene.scene.0, &instances.0, &normals.0])?;
                } else {
                    raytracing::retain(self, &[&scene.scene.0])?;
                }
                Ok(scene)
            })
            .and_then(|scene| {
                let va = scene.scene.1;
                let geometry = scene
                    .geometry
                    .as_ref()
                    .map(|[instances, normals]| [instances.1, normals.1]);
                if scene.recheck_on_submit()
                    && !matches!(
                        self.scene_commands.last(),
                        Some(raytracing::SceneCommand::Check(last)) if *last == scene
                    )
                {
                    self.scene_commands
                        .try_reserve(1)
                        .map_err(|_| E_OUTOFMEMORY)?;
                    self.scene_commands
                        .push(raytracing::SceneCommand::Check(scene));
                }
                unsafe {
                    self.bindings
                        .dispatch_scene(&self.active.native, root, va, geometry, dispatch)
                };
                crate::diagnostics::log_once(&root.program.first_dispatch, || {
                    format!(
                        "recorded inline compute shader {} dispatch with scene GPU VA {va:#x}",
                        root.program.source_hash
                    )
                });
                Ok(())
            });
        if let Err(error) = result {
            Self::skip("compute dispatch: inline ray scene binding", error.code());
        }
    }

    pub(super) fn draw(
        &mut self,
        list: &List,
        rasterizes: bool,
        draw: impl FnOnce(&ID3D12GraphicsCommandList),
    ) {
        self.record_unknown_write();
        if rasterizes {
            self.note_shader_fallback();
        }
        let null_usage = self.bindings.null_usage.clone();
        let display_bound = self.display_bound;
        let native = self.active.native.clone();
        let draw = move || {
            draw(&native);
            if rasterizes {
                crate::diagnostics::display::draw(display_bound, null_usage.is_some());
                if let Some(tag) = &null_usage {
                    crate::diagnostics::display::record_null(tag, display_bound);
                }
            }
        };
        if rasterizes {
            for target in &self.bound_placed_targets {
                if let Err(error) = Self::track_texture(&mut self.placed_writes, target) {
                    self.error = error;
                    return;
                }
            }
        }
        let trace = |state: &mut Self| {
            if let Some(trace) = &state.bindings.draw_trace {
                trace.recorded(|| {
                    state.bindings.raster_inputs(
                        state.resource_heap.as_ref(),
                        state.descriptor_arena.as_deref(),
                        state.display_bound,
                    )
                });
                if let Some(target) = &state.trace_target
                    && let Some(label) = trace.sample_label()
                    && let Some(sample) = crate::present::sampler::record(
                        &state.active.native,
                        target,
                        D3D12_RESOURCE_STATE_RENDER_TARGET,
                    )
                {
                    state.trace_samples.push((label, sample));
                }
            }
        };
        if let Some(table) = self.bindings.reflected_table::<false>() {
            if self.stencil_slot.is_some() {
                Self::skip("draw: stencil export with a reflected table", E_NOTIMPL);
                return;
            }
            if let Some(binding) = table.index_map {
                // These PSOs have one descriptor table followed by one root
                // constants parameter. The native ISA selects its scalar
                // buffer with a word (and optional mask) from that parameter.
                let descriptor = self
                    .bindings
                    .graphics_constant(1, binding.scalar_word)
                    .and_then(|word| {
                        self.resource_heap.as_ref()?.scalar_descriptor(
                            word & binding.scalar_mask,
                            self.descriptor_arena.as_deref(),
                        )
                    });
                let Some(descriptor) = descriptor else {
                    Self::skip("draw: no scalar descriptor for the indexed load", E_INVALIDARG);
                    return;
                };
                if self.scalar_descriptors.try_reserve(1).is_err() {
                    self.error = E_OUTOFMEMORY;
                    return;
                }
                self.scalar_descriptors.insert(descriptor);
                if let Some(arena) = &self.descriptor_arena
                    && !self
                        .indexed_arenas
                        .iter()
                        .any(|used| Arc::ptr_eq(used, arena))
                {
                    if self.indexed_arenas.try_reserve(1).is_err() {
                        self.error = E_OUTOFMEMORY;
                        return;
                    }
                    self.indexed_arenas.push(arena.clone());
                }
            }
            if let Err(error) = self.bindings.with_reflected_table::<false>(
                table,
                list,
                &self.active.native,
                self.descriptor_arena.as_ref(),
                &mut self.descriptor_leases,
                self.resource_heap.as_ref(),
                &mut self.sampled_heaps,
                &self.resources,
                draw,
            ) {
                Self::skip("draw with a reflected table", error.code());
            } else {
                trace(self);
            }
            return;
        }
        if self.descriptor_arena.is_some() && !self.bindings.supports_paged_draw::<false>() {
            Self::skip("draw: pipeline has no paged descriptor layout", E_NOTIMPL);
            return;
        }
        if let Err(error) = self
            .bindings
            .refresh_inline_tables::<false>(self.descriptor_arena.as_ref(), &mut self.descriptor_leases)
        {
            Self::skip("draw: descriptor table refresh", error.code());
            return;
        }
        self.sample_tables::<false>();
        if let Some(slot) = self.stencil_slot
            && self.dsv_bound
        {
            let status = self
                .stencil_targets
                .as_ref()
                .ok_or_else(|| windows::core::Error::from(E_NOTIMPL))
                .and_then(|t| t.draw(&self.active.native, slot, draw));
            if let Err(e) = status {
                Self::skip("draw: stencil export", e.code());
            } else {
                trace(self);
            }
        } else {
            draw();
            trace(self);
        }
    }

    pub(super) fn sample_tables<const COMPUTE: bool>(&mut self) {
        if let Some(heap) = &self.resource_heap
            && self
                .bindings
                .sample_tables::<COMPUTE>(heap, &mut self.sampled_heaps)
                .is_none()
        {
            self.sample_heap();
        }
    }

    pub(super) fn sample_heap(&mut self) {
        if let Some(heap) = &self.resource_heap {
            descriptors::include_range(&mut self.sampled_heaps, heap.cpu.clone());
        }
    }

    /// Ensure the active native segment is open for state-binding commands that
    /// execute no GPU work on their own.
    pub(super) fn prepare_state(&mut self) -> bool {
        self.ensure_open()
    }

    /// Ensure the active native segment is open and mark that GPU work has been
    /// recorded into it, so a later memory wait splits before subsequent work.
    pub(super) fn can_record(&mut self) -> bool {
        if !self.ensure_open() {
            return false;
        }
        self.active.has_commands = true;
        true
    }

    pub(super) fn ensure_open(&mut self) -> bool {
        if self.error.is_err() {
            return false;
        }
        match &self.phase {
            ListState::Fresh(allocator, initial) => {
                // The factory closed the empty native list to release its allocator.
                // Reopen with its original bindings when the first operation records.
                if let Err(error) = unsafe { self.active.native.Reset(allocator, initial.as_ref()) }
                {
                    self.error = error.code();
                    return false;
                }
                self.phase = ListState::Open { native_open: true };
            }
            ListState::Open { native_open: true } => {}
            ListState::Open { native_open: false } | ListState::Closed => self.error = E_FAIL,
        }
        self.error.is_ok()
    }
}
