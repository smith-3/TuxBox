use super::*;
use std::collections::VecDeque;
use std::ffi::{CStr, CString};
use std::os::windows::io::{FromRawHandle, IntoRawHandle, OwnedHandle};
use std::sync::Condvar;
use std::thread::{self, JoinHandle};
use windows::Win32::Graphics::Direct3D12::ID3D12Fence;
use windows::Win32::System::Threading::{CreateEventW, ResetEvent, SetEvent};

const PHYSICAL_PAGE_BYTES: usize = 64 * 1024;
const SCARLETT_TILE_RUN_LIMIT: usize = 32 * 1024 * 1024;
const ZLIB: u64 = 1;
const TO_PAGES: u64 = 1 << 21;

#[implement(IQueue2)]
struct Queue {
    shared: Arc<Shared>,
    worker: Mutex<Option<JoinHandle<()>>>,
    name: Option<CString>,
    source_type: u64,
    priority: i8,
    manual: bool,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    capacity: usize,
    priority: usize,
    error_event: OwnedHandle,
}

struct State {
    entries: VecDeque<Entry>,
    ready: usize,
    processing: bool,
    closed: bool,
    error_count: u32,
    first_error: Option<Box<ErrorRecord>>,
}

enum Entry {
    Read(Read),
    Status(Arc<[AtomicI32]>, usize),
    Signal(ID3D12Fence, u64),
    Event(OwnedHandle),
    Invalid(HRESULT, u32),
}

enum Source {
    File(FileRead, u64),
    Memory(usize),
}

struct Read {
    source: Source,
    /// Physical page count and byte offset into the first page; tokens are read
    /// from `dest` at execution time under the DirectStorage lifetime contract.
    pages: Option<(usize, usize)>,
    dest: usize,
    capacity: usize,
    size: usize,
    options: u64,
    intermediate_size: u32,
    tag: u64,
    cancelled: bool,
}

// Borrowed process buffer addresses are valid until queue completion by the
// DirectStorage contract. The file is retained independently of its COM object.
unsafe fn capture(queue_type: u64, req: *const Request) -> Result<Read, HRESULT> {
    let req = unsafe { req.as_ref() }.ok_or(E_POINTER)?;
    const FROM_MEMORY: u64 = 1 << 22;
    if req.options & !(FROM_MEMORY | TO_PAGES | ZLIB) != 0 {
        trace::unsupported(req.options);
        return Err(E_NOTIMPL);
    }
    // Physical page tokens may be populated between EnqueueRequest and Submit.
    let pages = if req.options & TO_PAGES != 0 {
        if req.destination.is_null() || !req.destination.cast::<u64>().is_aligned() {
            return Err(E_INVALIDARG);
        }
        let offset = usize::from(req.page_offset);
        let count = (offset + req.destination_size as usize).div_ceil(PHYSICAL_PAGE_BYTES);
        Some((count, offset))
    } else {
        None
    };
    let memory = req.options & FROM_MEMORY != 0;
    if memory != (queue_type == 1)
        || req.destination.is_null()
        || req.source.is_null()
        || req.size == 0
        || req.destination_size == 0
        || (req.options & 1 == 0 && req.size > req.destination_size)
    {
        return Err(E_INVALIDARG);
    }
    let source = if memory {
        Source::Memory(req.source as usize)
    } else {
        let file = unsafe { IUnknown::from_raw_borrowed(&req.source) }.ok_or(E_POINTER)?;
        let object = file
            .cast_object_ref::<StorageFile>()
            .map_err(|e| e.code())?;
        Source::File(object.backing.capture()?, req.offset)
    };
    Ok(Read {
        source,
        pages,
        dest: req.destination as usize,
        capacity: req.destination_size as usize,
        size: req.size as usize,
        options: req.options,
        intermediate_size: req.intermediate_size,
        tag: req.tag,
        cancelled: false,
    })
}

unsafe extern "system" {
    fn ReadFile(
        file: *mut c_void,
        output: *mut u8,
        size: u32,
        got: *mut u32,
        overlapped: *mut Overlapped,
    ) -> i32;
}

#[repr(C)]
#[derive(Default)]
struct Overlapped {
    internal: usize,
    bytes: usize,
    offset: u64,
    event: usize,
}

fn inflate_page_prefix(mut compressed: &[u8], capacity: usize) -> Result<(Vec<u8>, usize), HRESULT> {
    use miniz_oxide::inflate::stream::{InflateState, inflate};
    use miniz_oxide::{DataFormat, MZFlush, MZStatus};

    let invalid = HRESULT::from_win32(ERROR_INVALID_DATA.0);
    let mut state = InflateState::new_boxed(DataFormat::Zlib);
    let mut prefix = vec![0; capacity];
    let mut discarded = [0; 16 * 1024];
    let mut decoded = 0;
    loop {
        let output = if decoded < capacity {
            &mut prefix[decoded..]
        } else {
            &mut discarded
        };
        let step = inflate(&mut state, compressed, output, MZFlush::None);
        let status = step.status.map_err(|_| invalid)?;
        compressed = &compressed[step.bytes_consumed..];
        decoded += step.bytes_written;
        if decoded > SCARLETT_TILE_RUN_LIMIT {
            return Err(invalid);
        }
        if status == MZStatus::StreamEnd {
            prefix.truncate(decoded);
            return Ok((prefix, decoded));
        }
        if step.bytes_consumed == 0 && step.bytes_written == 0 {
            return Err(invalid);
        }
    }
}

impl Read {
    fn allows_clipped_zlib(&self) -> bool {
        static SCARLETT_PREFIX: OnceLock<bool> = OnceLock::new();
        let clipped_page = self.pages.is_some_and(|(count, offset)| {
            count == 1
                && offset.is_multiple_of(4096)
                && self.capacity.is_multiple_of(4096)
                && self.capacity < PHYSICAL_PAGE_BYTES
        });
        // Project Scarlett can ask for a prefix of a compressed tile run.
        self.options == (TO_PAGES | ZLIB)
            && self.intermediate_size == 0
            && clipped_page
            && *SCARLETT_PREFIX.get_or_init(|| {
                std::env::var_os("TUXBOX_SCARLETT_STORAGE_PREFIX").is_some_and(|v| v == "1")
            })
    }

    fn execute(&self) -> Result<(), HRESULT> {
        if self.cancelled {
            return Ok(());
        }
        // The title's staging size bounds one compressed read. Keep the limit,
        // but decode into a per-request buffer so reads can run in parallel.
        if self.options & 1 != 0
            && matches!(self.source, Source::File(..))
            && let Some((limit, _)) = lock(&STAGING).as_ref()
            && self.size > *limit
        {
            return Err(E_OUTOFMEMORY);
        }
        let mut local = Vec::new();
        let compressed = &mut local;
        let mut plain = Vec::new();
        let target = if self.options & 1 != 0 {
            compressed
                .try_reserve_exact(self.size)
                .map_err(|_| E_OUTOFMEMORY)?;
            compressed.spare_capacity_mut().as_mut_ptr().cast::<u8>()
        } else if self.pages.is_some() {
            plain.try_reserve_exact(self.size).map_err(|_| E_OUTOFMEMORY)?;
            plain.resize(self.size, 0);
            plain.as_mut_ptr()
        } else {
            self.dest as *mut u8
        };
        match &self.source {
            Source::File(file, offset) => {
                // A synchronous Win32 handle with an explicit 64-bit OVERLAPPED
                // offset makes each read atomic w.r.t. concurrent offset reads.
                let mut ov = Overlapped {
                    offset: *offset,
                    ..Default::default()
                };
                let mut got = 0;
                {
                    let mut stats = storage_stats();
                    stats.pending_reads += 1;
                    stats.total_reads = stats.total_reads.saturating_add(1);
                }
                let success = unsafe {
                    ReadFile(
                        file.file.as_raw_handle(),
                        target,
                        self.size as u32,
                        &mut got,
                        &mut ov,
                    )
                };
                let result = if success == 0 {
                    Err(HRESULT::from_win32(unsafe { GetLastError() }.0))
                } else if got as usize != self.size {
                    Err(HRESULT::from_win32(ERROR_HANDLE_EOF.0))
                } else {
                    Ok(())
                };
                let mut stats = storage_stats();
                stats.pending_reads -= 1;
                stats.read_errors = stats.read_errors.saturating_add(u32::from(result.is_err()));
                result?;
            }
            Source::Memory(p) => unsafe { core::ptr::copy(*p as *const u8, target, self.size) },
        }
        if self.options & 1 != 0 {
            // ReadFile or the copy above initialized every reported input byte.
            unsafe { compressed.set_len(self.size) };
            {
                let mut stats = storage_stats();
                stats.pending_decodes += 1;
                stats.total_decodes = stats.total_decodes.saturating_add(1);
            }
            let output = if self.allows_clipped_zlib() {
                inflate_page_prefix(compressed, self.capacity)
            } else {
                miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(compressed, self.capacity)
                    .map(|bytes| {
                        let decoded = bytes.len();
                        (bytes, decoded)
                    })
                    .map_err(|_| HRESULT::from_win32(ERROR_INVALID_DATA.0))
            };
            {
                let mut stats = storage_stats();
                stats.pending_decodes -= 1;
                stats.decode_errors = stats
                    .decode_errors
                    .saturating_add(u32::from(output.is_err()));
            }
            let (output, decoded) = output?;
            if decoded > self.capacity {
                trace::clipped(self, decoded);
            } else if decoded < self.capacity {
                trace::short_decode(self, decoded);
            }
            if let Some((count, offset)) = self.pages {
                let tokens = unsafe { core::slice::from_raw_parts(self.dest as *const u64, count) };
                return write_pages(tokens, offset, &output);
            }
            unsafe {
                core::ptr::copy_nonoverlapping(output.as_ptr(), self.dest as *mut u8, output.len())
            };
        } else if let Some((count, offset)) = self.pages {
            let tokens = unsafe { core::slice::from_raw_parts(self.dest as *const u64, count) };
            return write_pages(tokens, offset, &plain);
        }
        Ok(())
    }
}

fn signal_error(shared: &Shared, entry: &Entry, hr: HRESULT) {
    let mut state = lock(&shared.state);
    state.error_count = state.error_count.saturating_add(1);
    if state.first_error.is_none() {
        // The documented record consists exclusively of pointers, integers
        // and fixed-size strings, for which all-zero is an empty record.
        let mut r: Box<ErrorRecord> = Box::new(unsafe { core::mem::zeroed() });
        r.hr = hr.0;
        match entry {
            Entry::Read(read) => {
                r.options = read.options;
                r.destination = read.dest;
                r.destination_size = read.capacity as u32;
                r.size = read.size as u32;
                r.tag = read.tag;
                match read.source {
                    Source::File(_, offset) => r.offset = offset,
                    Source::Memory(p) => r.source = p,
                }
            }
            Entry::Invalid(_, kind) => r.kind = *kind,
            Entry::Status(..) => r.kind = 1,
            Entry::Event(_) => r.kind = 3,
            Entry::Signal(fence, value) => {
                r.kind = 2;
                r.options = fence.as_raw() as u64;
                r.destination = *value as usize;
            }
        }
        state.first_error = Some(r);
    }
    let _ = unsafe { SetEvent(HANDLE(shared.error_event.as_raw_handle())) };
}

fn consume(shared: &Shared) {
    let mut last_error = S_OK;
    loop {
        let mut state = lock(&shared.state);
        while state.ready == 0 && !state.closed {
            state = shared
                .changed
                .wait(state)
                .unwrap_or_else(|e| e.into_inner());
        }
        let Some(entry) = state.entries.pop_front() else {
            return;
        };
        state.ready -= 1;
        state.processing = true;
        // Reads between two status, signal or event entries may complete in
        // any order. Take the ready run of reads and execute it in parallel:
        // the console decompresses in hardware, one thread cannot keep up.
        if matches!(entry, Entry::Read(_)) {
            let mut batch = vec![entry];
            while batch.len() < 256 && state.ready > 0 && matches!(state.entries.front(), Some(Entry::Read(_))) {
                batch.extend(state.entries.pop_front());
                state.ready -= 1;
            }
            drop(state);
            let results = if batch.len() > 1 {
                execute_reads(shared, &batch)
            } else {
                vec![run_entry(shared, &batch[0], &mut last_error)]
            };
            for (entry, result) in batch.iter().zip(results) {
                finish(shared, entry, result, &mut last_error);
            }
            lock(&shared.state).processing = false;
            shared.changed.notify_all();
            continue;
        }
        drop(state);
        let result = run_entry(shared, &entry, &mut last_error);
        finish(shared, &entry, result, &mut last_error);
        lock(&shared.state).processing = false;
        shared.changed.notify_all();
    }
}

fn read_stats(shared: &Shared, req: &Read) -> Result<(), HRESULT> {
    {
        let mut stats = storage_stats();
        let p = &mut stats.priorities[shared.priority];
        p.pending += 1;
        p.requests = p.requests.saturating_add(1);
        p.source_bytes = p.source_bytes.saturating_add(req.size as u64);
        p.destination_bytes = p.destination_bytes.saturating_add(req.capacity as u64);
    }
    let result = req.execute();
    storage_stats().priorities[shared.priority].pending -= 1;
    result
}

fn execute_reads(shared: &Shared, batch: &[Entry]) -> Vec<Result<(), HRESULT>> {
    static WORKERS: std::sync::LazyLock<usize> = std::sync::LazyLock::new(|| {
        std::env::var("TUXBOX_STORAGE_THREADS").ok().and_then(|v| v.parse().ok())
            .unwrap_or_else(|| thread::available_parallelism().map_or(4, |n| n.get().min(8)))
            .max(1)
    });
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results: Vec<Mutex<Result<(), HRESULT>>> = batch.iter().map(|_| Mutex::new(Ok(()))).collect();
    thread::scope(|scope| {
        for _ in 0..(*WORKERS).min(batch.len()) {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(Entry::Read(req)) = batch.get(i) else { break };
                *lock(&results[i]) = read_stats(shared, req);
            });
        }
    });
    results.into_iter().map(|r| r.into_inner().unwrap_or_else(|e| e.into_inner())).collect()
}

fn finish(shared: &Shared, entry: &Entry, result: Result<(), HRESULT>, last_error: &mut HRESULT) {
    if let Err(hr) = result {
        if last_error.is_ok() {
            *last_error = hr;
        }
        {
            let mut stats = storage_stats();
            stats.errors = stats.errors.saturating_add(1);
        }
        trace::failure(entry, hr);
        signal_error(shared, entry, hr);
    }
}

fn run_entry(shared: &Shared, entry: &Entry, last_error: &mut HRESULT) -> Result<(), HRESULT> {
    {
        let result = match entry {
            Entry::Read(req) => read_stats(shared, req),
            Entry::Status(values, i) => {
                let mut stats = storage_stats();
                let p = &mut stats.priorities[shared.priority];
                p.statuses = p.statuses.saturating_add(1);
                values[*i].store(last_error.0, Ordering::Release);
                *last_error = S_OK;
                Ok(())
            }
            Entry::Signal(fence, value) => {
                {
                    let mut stats = storage_stats();
                    let p = &mut stats.priorities[shared.priority];
                    p.fences = p.fences.saturating_add(1);
                }
                unsafe { fence.Signal(*value) }.map_err(|e| e.code())
            }
            Entry::Invalid(hr, _) => Err(*hr),
            Entry::Event(event) => {
                unsafe { SetEvent(HANDLE(event.as_raw_handle())) }.map_err(|e| e.code())
            }
        };
        result
    }
}

pub(super) unsafe fn create(desc: &QueueDesc, manual: bool) -> Result<IQueue2, HRESULT> {
    if desc.source_type > 1
        || (desc.source_type == 1 && desc.priority != 2)
        || !(-1..=2).contains(&desc.priority)
        || !(0x80..=0x2000).contains(&desc.capacity)
    {
        return Err(E_INVALIDARG);
    }
    let handle = unsafe { CreateEventW(None, true, false, None) }.map_err(|e| e.code())?;
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            entries: VecDeque::new(),
            ready: 0,
            processing: false,
            closed: false,
            error_count: 0,
            first_error: None,
        }),
        changed: Condvar::new(),
        capacity: desc.capacity as usize,
        priority: (desc.priority + 1) as usize,
        error_event: unsafe { OwnedHandle::from_raw_handle(handle.0) },
    });
    let run = Arc::clone(&shared);
    let worker = thread::Builder::new()
        .name("TuxBox storage".into())
        .spawn(move || consume(&run))
        .map_err(|_| E_OUTOFMEMORY)?;
    let name = if desc.name.is_null() {
        None
    } else {
        Some(unsafe { CStr::from_ptr(desc.name) }.to_owned())
    };
    {
        let mut stats = storage_stats();
        let p = &mut stats.priorities[(desc.priority + 1) as usize];
        p.queues += 1;
        p.peak_queues = p.peak_queues.max(p.queues);
        p.total_queues = p.total_queues.saturating_add(1);
    }
    Ok(Queue {
        manual,
        shared,
        worker: Mutex::new(Some(worker)),
        name,
        source_type: desc.source_type,
        priority: desc.priority,
    }
    .into())
}

impl Queue {
    fn enqueue(&self, entry: Entry) {
        let mut state = lock(&self.shared.state);
        while !state.closed
            && state.entries.len() + usize::from(state.processing) >= self.shared.capacity - 1
        {
            if !self.manual {
                state.ready = state.entries.len();
                self.shared.changed.notify_all();
            }
            state = self
                .shared
                .changed
                .wait(state)
                .unwrap_or_else(|e| e.into_inner());
        }
        if state.closed {
            return;
        }
        if let Entry::Status(v, i) = &entry {
            v[*i].store(E_PENDING.0, Ordering::Release);
        }
        state.entries.push_back(entry);
        if !self.manual && state.entries.len() - state.ready >= self.shared.capacity / 2 {
            state.ready = state.entries.len();
            self.shared.changed.notify_all();
        }
    }
    fn close(&self) {
        {
            let mut state = lock(&self.shared.state);
            state.closed = true;
            state.ready = state.entries.len();
            self.shared.changed.notify_all();
        }
        if let Some(worker) = lock(&self.worker).take() {
            let _ = worker.join();
        }
    }
}

impl Drop for Queue {
    fn drop(&mut self) {
        self.close();
        storage_stats().priorities[(self.priority + 1) as usize].queues -= 1;
    }
}

impl IQueue_Impl for Queue_Impl {
    unsafe fn EnqueueRequest(&self, req: *const Request) {
        self.enqueue(
            unsafe { capture(self.source_type, req) }
                .map_or_else(|e| Entry::Invalid(e, 0), Entry::Read),
        );
    }
    unsafe fn EnqueueStatus(&self, array: *mut c_void, index: u32) {
        let entry = unsafe { IUnknown::from_raw_borrowed(&array) }
            .and_then(|u| u.cast_object_ref::<Status>().ok())
            .filter(|obj| (index as usize) < obj.values.len())
            .map(|obj| Entry::Status(Arc::clone(&obj.values), index as usize))
            .unwrap_or(Entry::Invalid(E_INVALIDARG, 1));
        self.enqueue(entry);
    }
    unsafe fn EnqueueSignal(&self, fence: *mut c_void, value: u64) {
        let entry = unsafe { IUnknown::from_raw_borrowed(&fence) }
            .and_then(|u| u.cast::<ID3D12Fence>().ok())
            .map(|f| Entry::Signal(f, value))
            .unwrap_or(Entry::Invalid(E_INVALIDARG, 2));
        self.enqueue(entry);
    }
    unsafe fn Submit(&self) {
        let mut s = lock(&self.shared.state);
        s.ready = s.entries.len();
        self.shared.changed.notify_all();
    }
    unsafe fn CancelRequestsWithTag(&self, mask: u64, value: u64) {
        for entry in &mut lock(&self.shared.state).entries {
            if let Entry::Read(r) = entry
                && (r.tag & mask) == value
            {
                r.cancelled = true;
            }
        }
    }
    unsafe fn Close(&self) {
        self.close()
    }
    unsafe fn GetErrorEvent(&self) -> *mut c_void {
        self.shared
            .error_event
            .try_clone()
            .map_or(core::ptr::null_mut(), IntoRawHandle::into_raw_handle)
    }
    unsafe fn RetrieveErrorRecord(&self, record: *mut ErrorRecord) {
        if record.is_null() {
            return;
        }
        let mut s = lock(&self.shared.state);
        if let Some(mut first) = s.first_error.take() {
            first.count = s.error_count;
            unsafe { record.write(*first) };
        } else {
            unsafe {
                record.write_bytes(0, 1);
                (*record).kind = u32::MAX;
            }
        }
        s.error_count = 0;
        let _ = unsafe { ResetEvent(HANDLE(self.shared.error_event.as_raw_handle())) };
    }
    unsafe fn Query(&self, info: *mut QueueInfo) {
        if info.is_null() {
            return;
        }
        let s = lock(&self.shared.state);
        let desc = QueueDesc {
            source_type: self.source_type,
            priority: self.priority,
            capacity: self.shared.capacity as u16,
            name: self.name.as_ref().map_or(core::ptr::null(), |n| n.as_ptr()),
        };
        unsafe {
            info.write(QueueInfo {
                desc,
                empty: (self.shared.capacity - 1 - s.entries.len() - usize::from(s.processing))
                    as u16,
                until_submit: if self.manual {
                    u16::MAX
                } else {
                    (self.shared.capacity / 2 - (s.entries.len() - s.ready)) as u16
                },
            })
        };
    }
}

impl IQueue2_Impl for Queue_Impl {
    unsafe fn EnqueueSetEvent(&self, event: *mut c_void) {
        use std::os::windows::io::BorrowedHandle;
        let entry = if event.is_null() || event as isize == -1 {
            Entry::Invalid(E_INVALIDARG, 3)
        } else {
            unsafe { BorrowedHandle::borrow_raw(event) }
                .try_clone_to_owned()
                .map_or_else(
                    |e| {
                        Entry::Invalid(HRESULT::from_win32(e.raw_os_error().unwrap_or(1) as u32), 3)
                    },
                    Entry::Event,
                )
        };
        self.enqueue(entry);
    }
    unsafe fn Query1(&self, info: *mut QueueInfo1) {
        if info.is_null() {
            return;
        }
        let mut base = std::mem::MaybeUninit::uninit();
        unsafe {
            self.Query(base.as_mut_ptr());
            let base: QueueInfo = base.assume_init();
            info.write(QueueInfo1 {
                desc: QueueDesc1 {
                    desc: base.desc,
                    options: self.manual as u64,
                },
                empty: base.empty,
                until_submit: base.until_submit,
            });
        }
    }
}


fn debug(text: &str) {
    let line = std::ffi::CString::new(format!("{text}\n")).unwrap_or_default();
    unsafe { windows::Win32::System::Diagnostics::Debug::OutputDebugStringA(windows::core::PCSTR(line.as_ptr().cast())) };
}

/// Write a decoded payload into physical pages through xmem's
/// `TuxBoxWritePhysicalPages`, which owns the page sections.
fn write_pages(tokens: &[u64], offset: usize, data: &[u8]) -> Result<(), HRESULT> {
    type Write = unsafe extern "system" fn(*const u64, usize, usize, *const u8, usize) -> i32;
    static WRITE: std::sync::OnceLock<Option<Write>> = std::sync::OnceLock::new();
    let write = WRITE.get_or_init(|| unsafe {
        use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
        let module = GetModuleHandleW(windows::core::w!("xmem.dll")).ok()?;
        let proc = GetProcAddress(module, windows::core::s!("TuxBoxWritePhysicalPages"))?;
        Some(core::mem::transmute::<_, Write>(proc))
    });
    let Some(write) = write else {
        debug("TuxBox xcrdapi: xmem.dll has no TuxBoxWritePhysicalPages; page destinations fail");
        return Err(E_NOTIMPL);
    };
    if unsafe { write(tokens.as_ptr(), tokens.len(), offset, data.as_ptr(), data.len()) } == 0 {
        return Err(HRESULT::from_win32(unsafe { GetLastError() }.0));
    }
    Ok(())
}

/// `TUXBOX_TRACE_STORAGE=1`: log each distinct failed request, and compressed
/// reads whose decoded size differs from the destination the title supplied.
/// Repeats are counted and logged again at power-of-two totals.
mod trace {
    use super::*;
    use std::collections::HashMap;
    use windows::Win32::Storage::FileSystem::{FILE_NAME_OPENED, GetFinalPathNameByHandleW};

    fn enabled() -> bool {
        static ENABLED: OnceLock<bool> = OnceLock::new();
        *ENABLED.get_or_init(|| std::env::var_os("TUXBOX_TRACE_STORAGE").as_deref() == Some(std::ffi::OsStr::new("1")))
    }

    /// Count one occurrence of `key`; true when the total is a power of two.
    fn due(key: (u8, i32, u64)) -> Option<u32> {
        static SEEN: Mutex<Option<HashMap<(u8, i32, u64), u32>>> = Mutex::new(None);
        let mut seen = lock(&SEEN);
        let count = seen.get_or_insert_with(HashMap::new).entry(key).or_default();
        *count = count.saturating_add(1);
        count.is_power_of_two().then_some(*count)
    }

    fn describe(read: &Read) -> String {
        let source = match &read.source {
            Source::File(file, offset) => {
                let handle = HANDLE(file.file.as_raw_handle());
                let required = unsafe { GetFinalPathNameByHandleW(handle, &mut [], FILE_NAME_OPENED) };
                let mut path = vec![0; required as usize];
                let length = unsafe { GetFinalPathNameByHandleW(handle, &mut path, FILE_NAME_OPENED) }
                    as usize;
                if length > 0 && length < path.len() {
                    let path = String::from_utf16_lossy(&path[..length]);
                    format!("file={path:?} offset={offset:#x}")
                } else {
                    format!("file=<unknown> offset={offset:#x}")
                }
            }
            Source::Memory(address) => format!("memory {address:#x}"),
        };
        format!(
            "{source} size={:#x} destination={:#x} options={:#x} compressed={} pages={:?} intermediate={:#x}",
            read.size, read.capacity, read.options, read.options & 1 != 0,
            read.pages.map(|(count, offset)| {
                let first = (count > 0).then(|| unsafe { *(read.dest as *const u64) });
                (count, first, offset)
            }),
            read.intermediate_size
        )
    }

    fn totals() -> String {
        let stats = storage_stats();
        format!(
            "reads={} read_errors={} decodes={} decode_errors={} errors={}",
            stats.total_reads, stats.read_errors, stats.total_decodes, stats.decode_errors, stats.errors
        )
    }

    pub(super) fn unsupported(options: u64) {
        if enabled() && let Some(count) = due((6, 0, options)) {
            debug(&format!("TuxBox xcrdapi: unsupported request options={options:#x} (#{count})"));
        }
    }

    pub(super) fn clipped(read: &Read, decoded: usize) {
        if enabled() && let Some(count) = due((7, 0, 0)) {
            debug(&format!(
                "TuxBox xcrdapi: validated clipped zlib block decoded={decoded:#x} (#{count}) {}",
                describe(read)
            ));
        }
    }

    pub(super) fn failure(entry: &Entry, hr: HRESULT) {
        if !enabled() {
            return;
        }
        let (kind, options) = match entry {
            Entry::Read(read) => (0, read.options),
            Entry::Invalid(_, at) => (1, u64::from(*at)),
            Entry::Status(..) => (2, 0),
            Entry::Signal(..) => (3, 0),
            Entry::Event(..) => (4, 0),
        };
        if let Some(count) = due((kind, hr.0, options)) {
            let detail = match entry {
                Entry::Read(read) => describe(read),
                Entry::Invalid(_, at) => format!("invalid request at check {at}"),
                Entry::Status(..) => "status".into(),
                Entry::Signal(..) => "fence signal".into(),
                Entry::Event(..) => "event".into(),
            };
            debug(&format!("TuxBox xcrdapi: request failed hr={:#010x} (#{count}) {detail}; {}", hr.0, totals()));
        }
    }

    pub(super) fn short_decode(read: &Read, decoded: usize) {
        if !enabled() {
            return;
        }
        let shortfall = (decoded < read.capacity) as u64;
        if let Some(count) = due((5, 0, shortfall)) {
            debug(&format!(
                "TuxBox xcrdapi: decoded {decoded:#x} bytes into a {:#x}-byte destination (#{count}) {}; {}",
                read.capacity, describe(read), totals()
            ));
        }
    }
}
