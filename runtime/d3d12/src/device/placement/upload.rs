//! Fence a private copy queue against the guest queues that read these textures.
use super::{Texture, committed, lock};
use core::mem::ManuallyDrop;
use std::{collections::VecDeque, sync::Arc};
use windows::Win32::Foundation::{CloseHandle, E_FAIL, E_INVALIDARG, E_OUTOFMEMORY, HANDLE};
use windows::Win32::Graphics::{Direct3D12::*, Dxgi::Common::DXGI_SAMPLE_DESC};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::{HRESULT, Interface, Result};

/// Target staging upload chunk size (32 MiB). Flushing before exceeding this
/// keeps normal chunks inside the persistent staging buffer without allocating
/// multi-hundred-MiB upload buffers that spill VRAM into host RAM.
const MAX_CHUNK_BYTES: u64 = 32 * 1024 * 1024;

/// Maximum persistent staging buffer capacity (64 MiB) so occasional >32 MiB
/// single mips can still reuse the same D3D12 UPLOAD heap block.
const MAX_STAGING_BYTES: u64 = 64 * 1024 * 1024;

/// Release oversized CPU scratch buffers after a large mip snapshot.
const MAX_SCRATCH_RETAIN: usize = 8 * 1024 * 1024;

/// Bounded wait when draining previous chunks during a multi-chunk burst.
const DRAIN_WAIT_MS: u32 = 2_000;

pub(super) struct Change {
    pub(super) texture: Arc<Texture>,
    pub(super) level: usize,
    pub(super) bytes: Vec<u8>,
    pub(super) digest: super::MipDigest,
}

// These COM objects must survive the asynchronous copy, even if the guest
// drops all associated resource and queue references immediately after Submit.
struct Upload {
    _buffer: ID3D12Resource,
    _allocator: ID3D12CommandAllocator,
    list: ID3D12CommandList,
    _textures: Vec<ID3D12Resource>,
}

struct OwnedEvent(HANDLE);

// SAFETY: Win32 event handles are process-wide kernel objects safe to use across threads.
unsafe impl Send for OwnedEvent {}

impl OwnedEvent {
    fn new() -> Result<Self> {
        Ok(Self(unsafe { CreateEventW(None, false, false, None)? }))
    }

    fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for OwnedEvent {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

pub(super) struct Transfer {
    queue: ID3D12CommandQueue,
    pub(super) fence: ID3D12Fence,
    event: OwnedEvent,
    next: u64,
    pub(super) failed: Option<HRESULT>,
    pending: VecDeque<(u64, Upload)>,
    staging: Option<(ID3D12Resource, u64)>,
    scratch: Vec<u8>,
}

impl Transfer {
    pub(super) fn new(device: &ID3D12Device) -> Result<Self> {
        let desc = D3D12_COMMAND_QUEUE_DESC {
            Type: D3D12_COMMAND_LIST_TYPE_COPY,
            ..Default::default()
        };
        let queue = unsafe { device.CreateCommandQueue(&desc)? };
        let fence = unsafe { device.CreateFence(0, D3D12_FENCE_FLAG_NONE)? };
        let event = OwnedEvent::new()?;
        Ok(Self {
            queue,
            fence,
            event,
            next: 0,
            failed: None,
            pending: VecDeque::new(),
            staging: None,
            scratch: Vec::new(),
        })
    }

    pub(super) fn tick(&mut self) -> Result<u64> {
        self.next = self.next.checked_add(1).ok_or(E_FAIL)?;
        Ok(self.next)
    }

    fn drain_completed(&mut self) {
        let completed = unsafe { self.fence.GetCompletedValue() };
        while self
            .pending
            .front()
            .is_some_and(|(tick, _)| *tick <= completed)
        {
            self.pending.pop_front();
        }
    }

    fn wait_and_drain(&mut self) {
        self.drain_completed();
        let Some(&(tick, _)) = self.pending.back() else {
            return;
        };
        if unsafe { self.fence.GetCompletedValue() } < tick
            && unsafe { self.fence.SetEventOnCompletion(tick, self.event.raw()) }.is_ok()
        {
            unsafe {
                WaitForSingleObject(self.event.raw(), DRAIN_WAIT_MS);
            }
        }
        self.drain_completed();
    }

    fn staging_buffer(&mut self, device: &ID3D12Device, required: u64) -> Result<ID3D12Resource> {
        // Reuse the persistent staging buffer whenever no prior upload is in flight.
        if self.pending.is_empty() && required <= MAX_STAGING_BYTES {
            if let Some((ref buffer, capacity)) = self.staging
                && capacity >= required
            {
                return Ok(buffer.clone());
            }
            let capacity = required.max(MAX_CHUNK_BYTES).min(MAX_STAGING_BYTES);
            let buffer = create_upload_buffer(device, capacity)?;
            self.staging = Some((buffer.clone(), capacity));
            return Ok(buffer);
        }
        create_upload_buffer(device, required)
    }

    fn submit_chunk(
        &mut self,
        device: &ID3D12Device,
        changes: &mut Vec<Change>,
    ) -> Result<()> {
        if changes.is_empty() {
            return Ok(());
        }
        if crate::diagnostics::display::trace_views() {
            for change in changes.iter() {
                let desc = change.texture.desc;
                if desc.Height == 1 && desc.Width <= 1024 {
                    use windows::core::Interface;
                    crate::diagnostics::recording_error(format_args!(
                        "uploading changed narrow texture {:p}, guest {:#x}, {}x{} format {} subresource {}",
                        change.texture.native.as_raw(), change.texture.address, desc.Width,
                        desc.Height, desc.Format.0, change.level,
                    ));
                }
            }
        }
        // Drain any completed upload so we can reuse the persistent staging buffer
        // without allocating a new D3D12 UPLOAD heap block on every batch.
        self.wait_and_drain();
        let (offsets, total) = Upload::layout(changes)?;
        let buffer = self.staging_buffer(device, total)?;
        let upload = Upload::record(device, buffer, changes, &offsets, total)?;
        for mips in changes.chunk_by(|a, b| Arc::ptr_eq(&a.texture, &b.texture)) {
            // A guest may use a different queue on each submission. Order
            // replacement writes after every outstanding reader of this texture.
            let contents = lock(&mips[0].texture.contents);
            for (fence, value) in &contents.readers {
                if unsafe { fence.GetCompletedValue() } < *value
                    && !super::held::blocked(fence, *value)
                {
                    unsafe {
                        self.queue.Wait(fence, *value)?;
                        crate::commands::queue_trace::waited(&self.queue, "upload waits for texture reads", fence, *value);
                    }
                }
            }
        }
        let tick = self.tick()?;
        self.pending.try_reserve(1).map_err(|_| E_OUTOFMEMORY)?;
        unsafe {
            self.queue.ExecuteCommandLists(&[Some(upload.list.clone())]);
        }
        self.pending.push_back((tick, upload));
        if let Err(error) = unsafe { self.queue.Signal(&self.fence, tick) } {
            self.failed = Some(error.code());
            return Err(error);
        }
        for change in changes.drain(..) {
            let mut state = lock(&change.texture.contents);
            state.previous[change.level] = Some(change.digest);
            if change.bytes.capacity() <= MAX_SCRATCH_RETAIN {
                self.scratch = change.bytes;
            }
            state.uploaded = tick;
            state.readers.clear();
        }
        Ok(())
    }

    pub(super) fn refresh(
        &mut self,
        device: &ID3D12Device,
        textures: &[Arc<Texture>],
    ) -> Result<()> {
        if let Some(error) = self.failed {
            return Err(error.into());
        }
        self.drain_completed();
        let alignment = u64::from(D3D12_TEXTURE_DATA_PLACEMENT_ALIGNMENT);
        let mut changes = Vec::new();
        let mut incoming = Vec::new();
        let mut chunk_bytes = 0u64;
        for texture in textures {
            incoming.clear();
            texture.changed(&mut incoming, &mut self.scratch)?;
            for change in incoming.drain(..) {
                let aligned = change.texture.mips[change.level]
                    .bytes
                    .next_multiple_of(alignment);
                if !changes.is_empty() && chunk_bytes.saturating_add(aligned) > MAX_CHUNK_BYTES {
                    self.submit_chunk(device, &mut changes)?;
                    chunk_bytes = 0;
                }
                chunk_bytes = chunk_bytes.saturating_add(aligned);
                changes.push(change);
                if chunk_bytes >= MAX_CHUNK_BYTES {
                    self.submit_chunk(device, &mut changes)?;
                    chunk_bytes = 0;
                }
            }
        }
        if !changes.is_empty() {
            self.submit_chunk(device, &mut changes)?;
        }
        if self.scratch.capacity() > MAX_SCRATCH_RETAIN {
            self.scratch = Vec::new();
        }
        Ok(())
    }
}

impl Drop for Transfer {
    fn drop(&mut self) {
        if let Some((tick, _)) = self.pending.back() {
            // Resignal in case the original signal failed after ExecuteCommandLists.
            unsafe {
                if self.fence.GetCompletedValue() < *tick
                    && self.queue.Signal(&self.fence, *tick).is_ok()
                {
                    let _ = self.fence.SetEventOnCompletion(*tick, HANDLE::default());
                }
            }
        }
    }
}

fn create_upload_buffer(device: &ID3D12Device, width: u64) -> Result<ID3D12Resource> {
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: width,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    };
    committed(
        device,
        D3D12_HEAP_TYPE_UPLOAD,
        &desc,
        D3D12_RESOURCE_STATE_GENERIC_READ,
        None,
    )
}

impl Upload {
    fn layout(changes: &[Change]) -> Result<(Vec<u64>, u64)> {
        let alignment = u64::from(D3D12_TEXTURE_DATA_PLACEMENT_ALIGNMENT);
        let mut total = 0u64;
        let mut offsets = Vec::with_capacity(changes.len());
        for change in changes {
            let offset = total
                .checked_next_multiple_of(alignment)
                .ok_or(E_OUTOFMEMORY)?;
            offsets.push(offset);
            total = offset
                .checked_add(change.texture.mips[change.level].bytes)
                .ok_or(E_OUTOFMEMORY)?;
        }
        let _ = usize::try_from(total)
            .ok()
            .filter(|n| *n <= isize::MAX as usize)
            .ok_or(E_OUTOFMEMORY)?;
        Ok((offsets, total))
    }

    fn record(
        device: &ID3D12Device,
        buffer: ID3D12Resource,
        changes: &[Change],
        offsets: &[u64],
        total: u64,
    ) -> Result<Self> {
        let size = usize::try_from(total)
            .ok()
            .filter(|n| *n <= isize::MAX as usize)
            .ok_or(E_OUTOFMEMORY)?;
        let allocator = unsafe { device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_COPY)? };
        let list: ID3D12GraphicsCommandList =
            unsafe { device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_COPY, &allocator, None)? };
        let mut mapped = core::ptr::null_mut();
        let empty = D3D12_RANGE::default();
        unsafe {
            buffer.Map(0, Some(&empty), Some(&mut mapped))?;
        }
        // SAFETY: this CPU-writeable buffer covers 'size' bytes; this command
        // list has not been executed and the pointer is scoped to the mapping.
        let output = unsafe { core::slice::from_raw_parts_mut(mapped.cast::<u8>(), size) };
        let mut destinations = Vec::new();
        let result: Result<()> = changes
            .iter()
            .zip(offsets.iter().copied())
            .try_for_each(|(change, offset)| {
                change.detile(&mut output[offset as usize..])?;
                let texture = &change.texture;
                let mut footprint = texture.mips[change.level].footprint;
                footprint.Offset = offset;
                let mut src = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: ManuallyDrop::new(Some(buffer.clone())),
                    Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                        PlacedFootprint: footprint,
                    },
                };
                let mut dst = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: ManuallyDrop::new(Some(texture.native.clone())),
                    Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                        SubresourceIndex: change.level as u32,
                    },
                };
                // SAFETY: both active union fields match their location tags. The
                // batch retains source and destinations until its fence completes.
                unsafe {
                    list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
                    ManuallyDrop::drop(&mut src.pResource);
                    ManuallyDrop::drop(&mut dst.pResource);
                }
                if destinations.last() != Some(&texture.native) {
                    destinations.push(texture.native.clone());
                }
                Ok(())
            });
        unsafe {
            buffer.Unmap(0, None);
        }
        result?;
        unsafe {
            list.Close()?;
        }
        Ok(Self {
            _buffer: buffer,
            _allocator: allocator,
            list: list.cast()?,
            _textures: destinations,
        })
    }
}

impl Change {
    fn detile(&self, output: &mut [u8]) -> Result<()> {
        self.texture.layout_runs(self.level, |tiled, linear| {
            output
                .get_mut(linear)
                .ok_or(E_INVALIDARG)?
                .copy_from_slice(self.bytes.get(tiled).ok_or(E_INVALIDARG)?);
            Ok(())
        })
    }
}

impl Texture {
    /// Corresponding contiguous byte runs in the tiled Xbox mip and in its
    /// D3D12 footprint. Upload and writeback must use the same addresses.
    pub(super) fn layout_runs(
        &self,
        level: usize,
        mut visit: impl FnMut(std::ops::Range<usize>, std::ops::Range<usize>) -> Result<()>,
    ) -> Result<()> {
        let mip = &self.mips[level];
        let element = mip.element_bytes as usize;
        let pitch = mip.footprint.Footprint.RowPitch as usize;
        let Some(plane) = &self.plane else {
            let source_pitch = ((mip.source.end - mip.source.start) / u64::from(mip.rows)) as usize;
            let row = mip.elements as usize * element;
            for y in 0..mip.rows as usize {
                visit(y * source_pitch..y * source_pitch + row, y * pitch..y * pitch + row)?;
            }
            return Ok(());
        };
        if self.desc.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
            for z in 0..mip.footprint.Footprint.Depth {
                for y in 0..mip.rows {
                    for x in 0..mip.elements {
                        let tiled = plane.element_offset_in_volume(z, mip.level, u64::from(x), y).ok_or(E_INVALIDARG)? as usize;
                        let linear = (z as usize * mip.rows as usize + y as usize) * pitch + x as usize * element;
                        visit(tiled..tiled + element, linear..linear + element)?;
                    }
                }
            }
            return Ok(());
        }
        // In layout 0x105, X occupies the low address bits through byte 15.
        let run = if self.desc.Layout.0 == 0x100 {
            mip.elements as usize
        } else {
            16 / element
        };
        for y in 0..mip.rows {
            for x in (0..mip.elements).step_by(run) {
                let count = (mip.elements - x).min(run as u32) as usize * element;
                let tiled = plane
                    .element_offset_in_slice(mip.slice, mip.level, x.into(), y)
                    .and_then(|a| a.checked_sub(mip.source.start))
                    .ok_or(E_INVALIDARG)? as usize;
                let linear = y as usize * pitch + x as usize * element;
                visit(tiled..tiled + count, linear..linear + count)?;
            }
        }
        Ok(())
    }
}
