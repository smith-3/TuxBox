//! Host pipeline creation on a thread with a large stack.
//!
//! Proton's vkd3d-proton compiles DXIL to SPIR-V synchronously inside
//! Create*PipelineState, on the calling thread. Its control-flow analysis
//! recurses deeply on large translated shaders. Xbox titles create pipelines
//! on worker threads with 1 MiB stacks, so those threads overflowed their stack
//! inside the compiler and died (ProjectScarlett stayed on a black screen).
//!
//! A cold NVIDIA disk cache then spends minutes inside that same call, turning
//! DXIL into a GPU binary. Titles create their pipelines up front: ProjectScarlett
//! creates about 59,000 before its first frame, on job threads its main thread
//! waits for, so a call that blocks on the driver holds the whole boot. A compile
//! that outlives the deadline therefore continues on a helper thread, and the
//! title gets its pipeline object at once. [`Pending::wait`] delivers the
//! compiled pipeline when a command list first binds it. The compile keeps the
//! device, root signature, and shader bytes.

/// Reserved, not committed: untouched pages cost only address space.
const STACK_BYTES: usize = 64 << 20;

/// `0` means the cap has not been chosen yet. Tests store a small cap here.
static MAX_LIVE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

struct AssertSend<T>(T);
// SAFETY: D3D12 devices and the descriptors they read are free-threaded, and
// the caller blocks until the helper thread has finished with them.
unsafe impl<T> Send for AssertSend<T> {}

/// Run `job` on a scoped helper thread with a 64 MiB stack and return its
/// result. Falls back to the current thread if the helper cannot be created.
///
/// `label` is reported while `CreatePipelineState` is still inside the driver.
/// Cached creates return in milliseconds and stay quiet. A compile that is
/// still going after five seconds is logged, then again every fifteen seconds,
/// because that call produces no other log line.
pub(crate) fn run_logged<R, F: FnOnce() -> R>(label: &str, job: F) -> R {
    let _watch = PipelineWatch::start(label);
    // The calling title thread waits here for the whole driver compile.
    let _pause = crate::title_watchdog::pause();
    let mut slot = Some(job);
    let pointer = AssertSend(&mut slot as *mut Option<F>);
    let result = std::thread::scope(|scope| {
        let spawned = std::thread::Builder::new()
            .name("tuxbox-pipeline".into())
            .stack_size(STACK_BYTES)
            .spawn_scoped(scope, move || {
                let pointer = pointer;
                // SAFETY: the spawning thread waits for this scope to end
                // before reading `slot` again.
                let job = unsafe { (*pointer.0).take() }.expect("pipeline job already taken");
                AssertSend(job())
            });
        match spawned {
            Ok(handle) => Some(handle.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic)).0),
            Err(_) => None,
        }
    });
    match result {
        Some(value) => value,
        None => (slot.take().expect("pipeline job already taken"))(),
    }
}

/// The compile finished, or it is still running after the deadline.
pub(crate) enum PipelineWait<T> {
    Ready(T),
    /// The driver compile continues on a helper thread, or waits in the queue
    /// for one. [`Pending::wait`] returns its result.
    Deferred(Pending<T>),
}

/// The result of a compile that outlived its deadline.
pub(crate) struct Pending<R> {
    cell: std::sync::Arc<Cell<R>>,
}

struct Cell<R> {
    /// Finds this compile in the queue if no helper has started it yet.
    id: u64,
    label: String,
    state: std::sync::Mutex<Outcome<R>>,
    settled: std::sync::Condvar,
}

enum Outcome<R> {
    Compiling,
    Done(R),
    /// The compile ended without a result, which only a panic can do.
    Lost,
}

impl<R> Cell<R> {
    fn settle(&self, outcome: Outcome<R>) {
        *self.state.lock().unwrap_or_else(|error| error.into_inner()) = outcome;
        self.settled.notify_all();
    }

    /// The finished result, or `None` once `timeout` passes first.
    fn take_within(&self, timeout: std::time::Duration) -> Option<Result<R, ()>> {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let (mut state, _) = self
            .settled
            .wait_timeout_while(state, timeout, |state| matches!(state, Outcome::Compiling))
            .unwrap_or_else(|error| error.into_inner());
        match std::mem::replace(&mut *state, Outcome::Lost) {
            Outcome::Compiling => {
                *state = Outcome::Compiling;
                None
            }
            Outcome::Done(value) => Some(Ok(value)),
            Outcome::Lost => Some(Err(())),
        }
    }
}

/// Settles the cell even if the compile unwinds, so no waiter blocks forever.
struct Delivery<R> {
    cell: std::sync::Arc<Cell<R>>,
    finished: bool,
}

impl<R> Delivery<R> {
    fn finish(mut self, value: R) {
        self.cell.settle(Outcome::Done(value));
        self.finished = true;
    }
}

impl<R> Drop for Delivery<R> {
    fn drop(&mut self) {
        if !self.finished {
            self.cell.settle(Outcome::Lost);
        }
    }
}

impl<R: Clone> Pending<R> {
    /// Block until the compile finishes, giving its queued job priority.
    pub(crate) fn wait(&self) -> Option<R> {
        if let Some(outcome) = self.finished() {
            return outcome;
        }
        let started = std::time::Instant::now();
        if trace_waits() {
            crate::diagnostics::log(&format!(
                "pipeline wait begin id={} {}", self.cell.id, self.cell.label
            ));
        }
        prioritize(self.cell.id);
        on_slot_released();
        let state = self.cell.state.lock().unwrap_or_else(|error| error.into_inner());
        let state = self
            .cell
            .settled
            .wait_while(state, |state| matches!(state, Outcome::Compiling))
            .unwrap_or_else(|error| error.into_inner());
        let outcome = match &*state {
            Outcome::Done(value) => Some(value.clone()),
            _ => None,
        };
        drop(state);
        if trace_waits() {
            crate::diagnostics::log(&format!(
                "pipeline wait end id={} {} elapsed_ms={} ready={}",
                self.cell.id, self.cell.label, started.elapsed().as_millis(), outcome.is_some()
            ));
        }
        note_first_use(&self.cell.label, started.elapsed());
        outcome
    }

    fn finished(&self) -> Option<Option<R>> {
        match &*self.cell.state.lock().unwrap_or_else(|error| error.into_inner()) {
            Outcome::Compiling => None,
            Outcome::Done(value) => Some(Some(value.clone())),
            Outcome::Lost => Some(None),
        }
    }

    pub(crate) fn inspect_completed<T>(&self, inspect: impl FnOnce(u64, &str, &R) -> T) -> Option<T> {
        match &*self.cell.state.lock().unwrap_or_else(|error| error.into_inner()) {
            Outcome::Done(result) => Some(inspect(self.cell.id, &self.cell.label, result)),
            _ => None,
        }
    }
}

/// Wait up to `TUXBOX_PIPELINE_DEADLINE` seconds (default 2). `0` waits until
/// the driver returns. Past the deadline the helper is detached, not cancelled.
/// `bytes` is what the job holds while it waits in the queue (see `enqueue`).
pub(crate) fn run_deadline<R, F>(label: &str, bytes: usize, job: F) -> PipelineWait<R>
where
    R: Send + 'static,
    F: FnOnce() -> R + Send + 'static,
{
    let deadline = pipeline_deadline();
    static ANNOUNCED: std::sync::Once = std::sync::Once::new();
    if deadline.is_zero() {
        crate::diagnostics::log_once(&ANNOUNCED, || {
            "pipelines wait for the driver compile".to_string()
        });
    } else {
        crate::diagnostics::log_once(&ANNOUNCED, || {
            format!(
                "pipelines wait {}s for the driver, then finish compiling while the title continues ({} at a time); command lists wait for them on first use",
                deadline.as_secs(),
                max_live()
            )
        });
    }
    run_for(label, deadline, bytes, job)
}

fn note_first_use(label: &str, waited: std::time::Duration) {
    if waited >= std::time::Duration::from_millis(100) {
        crate::diagnostics::log(&format!(
            "first use of {label} waited {}ms for its compile",
            waited.as_millis()
        ));
    }
}

fn trace_waits() -> bool {
    static TRACE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *TRACE.get_or_init(|| std::env::var_os("TUXBOX_TRACE_PIPELINE_WAITS").as_deref() == Some(std::ffi::OsStr::new("1")))
}

fn pipeline_deadline() -> std::time::Duration {
    static DEADLINE: std::sync::OnceLock<std::time::Duration> = std::sync::OnceLock::new();
    *DEADLINE.get_or_init(|| parse_deadline(std::env::var("TUXBOX_PIPELINE_DEADLINE").ok().as_deref()))
}

/// The default only bounds how long creation blocks. A pipeline still compiling
/// is waited for when it is first bound, so no title work is dropped.
fn parse_deadline(text: Option<&str>) -> std::time::Duration {
    const DEFAULT: std::time::Duration = std::time::Duration::from_secs(2);
    match text {
        None => DEFAULT,
        Some(text) => text.parse::<u64>().map_or(DEFAULT, std::time::Duration::from_secs),
    }
}

fn max_live() -> usize {
    let current = MAX_LIVE.load(std::sync::atomic::Ordering::Relaxed);
    if current != 0 {
        return current;
    }
    // Each live compile reserves a 64 MiB stack and occupies a driver thread.
    // One per CPU, capped, so a title that keeps submitting cannot pile up
    // hundreds of detached compiles.
    let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8);
    let configured = std::env::var("TUXBOX_PIPELINE_MAX_LIVE").ok();
    let chosen = compile_limit(configured.as_deref(), cpus);
    match MAX_LIVE.compare_exchange(0, chosen, std::sync::atomic::Ordering::AcqRel, std::sync::atomic::Ordering::Acquire) {
        Ok(_) => chosen,
        Err(existing) => existing,
    }
}

fn compile_limit(configured: Option<&str>, cpus: usize) -> usize {
    configured
        .and_then(|text| text.parse().ok())
        .filter(|value| (1..=16).contains(value))
        .unwrap_or_else(|| cpus.clamp(2, 16))
}

fn run_for<R, F>(label: &str, deadline: std::time::Duration, bytes: usize, job: F) -> PipelineWait<R>
where
    R: Send + 'static,
    F: FnOnce() -> R + Send + 'static,
{
    if deadline.is_zero() {
        return PipelineWait::Ready(run_logged(label, job));
    }
    static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let cell = std::sync::Arc::new(Cell {
        id: NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        label: label.to_string(),
        state: std::sync::Mutex::new(Outcome::Compiling),
        settled: std::sync::Condvar::new(),
    });
    let delivery = Delivery { cell: std::sync::Arc::clone(&cell), finished: false };
    let deliver = move || {
        if trace_waits() {
            crate::diagnostics::log(&format!(
                "pipeline job begin id={} {}", delivery.cell.id, delivery.cell.label
            ));
        }
        let value = job();
        if trace_waits() {
            crate::diagnostics::log(&format!(
                "pipeline job end id={} {}", delivery.cell.id, delivery.cell.label
            ));
        }
        delivery.finish(value);
    };
    if !reserve_slot() {
        enqueue(label, cell.id, bytes, deliver);
        // A slot may have freed between the reservation attempt and the push.
        on_slot_released();
        note_deferred(label, None);
        return PipelineWait::Deferred(Pending { cell });
    }
    // The calling title thread waits here, up to the deadline.
    let _pause = crate::title_watchdog::pause();
    let shared = std::sync::Arc::new(std::sync::Mutex::new(Some(deliver)));
    let for_thread = std::sync::Arc::clone(&shared);
    let label_owned = label.to_string();
    let spawned = spawn_pipeline(move || {
        let _slot = Slot;
        let _watch = PipelineWatch::start(&label_owned);
        take_job(&for_thread)();
    });
    let handle = match spawned {
        Ok(handle) => handle,
        Err(_) => {
            let _slot = Slot;
            run_logged(label, take_job(&shared));
            return match cell.take_within(std::time::Duration::ZERO) {
                Some(Ok(value)) => PipelineWait::Ready(value),
                _ => panic!("pipeline compile ended without a result"),
            };
        }
    };
    match cell.take_within(deadline) {
        Some(Ok(value)) => {
            join_or_resume(handle);
            PipelineWait::Ready(value)
        }
        Some(Err(())) => {
            join_or_resume(handle);
            panic!("pipeline compile ended without a result");
        }
        None => {
            note_deferred(label, Some(deadline));
            // Detach. The helper keeps running until the driver returns.
            drop(handle);
            PipelineWait::Deferred(Pending { cell })
        }
    }
}


fn note_deferred(label: &str, waited: Option<std::time::Duration>) {
    match waited {
        None => crate::diagnostics::log(&format!("CreatePipelineState {label} queued; compile continues")),
        Some(waited) => crate::diagnostics::log(&format!(
            "CreatePipelineState {label} deferred after {}s; compile continues",
            waited.as_secs()
        )),
    }
}

/// Each queued compile owns copies of its shader bytes and root signature, and
/// a title creates tens of thousands of pipelines faster than the driver
/// compiles them. Once the queued compiles hold `queue_budget` bytes, the
/// creating thread waits for one to start, so memory stays bounded while
/// small compiles still queue freely, and `max_live` still caps the driver's
/// concurrent compiles. A compile larger than the budget queues alone.
fn enqueue<F>(label: &str, id: u64, bytes: usize, job: F)
where
    F: FnOnce() + Send + 'static,
{
    let mut job = Some(Queued { label: label.to_string(), id, bytes, job: Box::new(job) });
    let mut _pause = None;
    loop {
        {
            let mut queued = queue();
            if queued.has_room(bytes, queue_budget()) {
                queued.push_back(job.take().expect("queued once"));
                return;
            }
            if _pause.is_none() {
                static FULL: std::sync::Once = std::sync::Once::new();
                crate::diagnostics::log_once(&FULL, || format!(
                    "pipeline compile queue holds {} compiles in {} MiB; creation waits for room",
                    queued.jobs.len(), queued.bytes >> 20
                ));
                _pause = Some(crate::title_watchdog::pause());
            }
            // Bounded, so a slot released without a pump still gets one below.
            drop(QUEUE_ROOM.wait_timeout(queued, std::time::Duration::from_millis(100)));
        }
        on_slot_released();
    }
}

/// Signalled whenever a compile leaves the queue.
static QUEUE_ROOM: std::sync::Condvar = std::sync::Condvar::new();

/// Bytes queued compiles may hold. `0` means not chosen yet; tests store one.
static QUEUE_BUDGET: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn queue_budget() -> usize {
    let current = QUEUE_BUDGET.load(std::sync::atomic::Ordering::Relaxed);
    if current != 0 {
        return current;
    }
    let configured = std::env::var("TUXBOX_PIPELINE_QUEUE_MIB").ok();
    let chosen = queue_budget_bytes(configured.as_deref());
    match QUEUE_BUDGET.compare_exchange(0, chosen, std::sync::atomic::Ordering::AcqRel, std::sync::atomic::Ordering::Acquire) {
        Ok(_) => chosen,
        Err(existing) => existing,
    }
}

fn queue_budget_bytes(configured: Option<&str>) -> usize {
    const DEFAULT_MIB: usize = 256;
    configured
        .and_then(|text| text.trim().parse().ok())
        .filter(|mib| (1..=16384).contains(mib))
        .unwrap_or(DEFAULT_MIB)
        << 20
}

/// Give a pipeline needed by a command list the next available compile slot.
fn prioritize(id: u64) {
    let mut queue = queue();
    if let Some(index) = queue.jobs.iter().position(|queued| queued.id == id)
        && let Some(job) = queue.jobs.remove(index)
    {
        queue.jobs.push_front(job);
    }
}

fn spawn_pipeline<F>(job: F) -> std::io::Result<std::thread::JoinHandle<()>>
where
    F: FnOnce() + Send + 'static,
{
    std::thread::Builder::new()
        .name("tuxbox-pipeline".into())
        .stack_size(STACK_BYTES)
        .spawn(job)
}

fn take_job<T>(slot: &std::sync::Mutex<Option<T>>) -> T {
    slot.lock().unwrap_or_else(|error| error.into_inner()).take().expect("pipeline job already taken")
}

fn join_or_resume(handle: std::thread::JoinHandle<()>) {
    if let Err(panic) = handle.join() {
        std::panic::resume_unwind(panic);
    }
}

struct Queued {
    label: String,
    id: u64,
    bytes: usize,
    job: Box<dyn FnOnce() + Send>,
}

/// Queued compiles and the bytes they hold.
struct Queue {
    jobs: std::collections::VecDeque<Queued>,
    bytes: usize,
}

impl Queue {
    fn has_room(&self, bytes: usize, budget: usize) -> bool {
        self.jobs.is_empty() || self.bytes.saturating_add(bytes) <= budget
    }

    fn push_back(&mut self, job: Queued) {
        self.bytes += job.bytes;
        self.jobs.push_back(job);
    }

    fn pop_front(&mut self) -> Option<Queued> {
        let job = self.jobs.pop_front()?;
        self.bytes -= job.bytes;
        Some(job)
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.jobs.is_empty()
    }
}

fn queue() -> std::sync::MutexGuard<'static, Queue> {
    static QUEUE: std::sync::Mutex<Queue> =
        std::sync::Mutex::new(Queue { jobs: std::collections::VecDeque::new(), bytes: 0 });
    QUEUE.lock().unwrap_or_else(|error| error.into_inner())
}

fn reserve_slot() -> bool {
    let live = live_counter();
    let mut current = live.load(std::sync::atomic::Ordering::Relaxed);
    loop {
        if current >= max_live() {
            return false;
        }
        match live.compare_exchange_weak(
            current,
            current + 1,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        ) {
            Ok(_) => return true,
            Err(observed) => current = observed,
        }
    }
}

fn live_counter() -> &'static std::sync::atomic::AtomicUsize {
    static LIVE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    &LIVE
}

#[cfg(test)]
fn inflight() -> usize {
    live_counter().load(std::sync::atomic::Ordering::Acquire)
}

/// Releases one reserved compile slot and starts a queued compile if one fits.
struct Slot;

impl Drop for Slot {
    fn drop(&mut self) {
        live_counter().fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        on_slot_released();
    }
}

static PUMP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn on_slot_released() {
    loop {
        if PUMP.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return;
        }
        start_one_queued();
        PUMP.store(false, std::sync::atomic::Ordering::Release);
        // A submission or slot release may have arrived while the pump was active.
        if live_counter().load(std::sync::atomic::Ordering::Acquire) >= max_live() || queue().jobs.is_empty() {
            return;
        }
    }
}

fn start_one_queued() {
    if !reserve_slot() {
        return;
    }
    let Some(queued) = queue().pop_front() else {
        live_counter().fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        return;
    };
    QUEUE_ROOM.notify_all();
    let shared = std::sync::Arc::new(std::sync::Mutex::new(Some(queued)));
    let for_thread = std::sync::Arc::clone(&shared);
    match spawn_pipeline(move || {
        let _slot = Slot;
        let queued = take_job(&for_thread);
        let _watch = PipelineWatch::start(&queued.label);
        (queued.job)();
    }) {
        Ok(handle) => drop(handle),
        Err(error) => {
            crate::diagnostics::log(&format!("pipeline compile thread was not created: {error}"));
            let _slot = Slot;
            let queued = take_job(&shared);
            // Detached failures settle their pending result through Delivery::drop.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_logged(&queued.label, queued.job);
            }));
        }
    }
}

struct Flight {
    id: u64,
    label: String,
    started: std::time::Instant,
    /// Seconds from the start at which the next progress line is due.
    next_due: u64,
}

struct PipelineWatch {
    id: Option<u64>,
    label: String,
    started: std::time::Instant,
}

impl PipelineWatch {
    fn start(label: &str) -> Self {
        if !crate::diagnostics::logging() {
            return Self { id: None, label: String::new(), started: std::time::Instant::now() };
        }
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        static WATCH: std::sync::Once = std::sync::Once::new();
        WATCH.call_once(|| {
            let _ = std::thread::Builder::new().name("tuxbox-pso-log".into()).spawn(watch);
        });
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let started = std::time::Instant::now();
        flights().push(Flight { id, label: label.to_string(), started, next_due: 5 });
        Self { id: Some(id), label: label.to_string(), started }
    }
}

impl Drop for PipelineWatch {
    fn drop(&mut self) {
        let Some(id) = self.id else { return };
        let elapsed = self.started.elapsed().as_secs();
        flights().retain(|flight| flight.id != id);
        if elapsed >= 5 {
            crate::diagnostics::log(&format!(
                "CreatePipelineState #{id} {} finished in {elapsed}s",
                self.label
            ));
        }
    }
}

fn flights() -> std::sync::MutexGuard<'static, Vec<Flight>> {
    static FLIGHTS: std::sync::Mutex<Vec<Flight>> = std::sync::Mutex::new(Vec::new());
    FLIGHTS.lock().unwrap_or_else(|error| error.into_inner())
}

fn watch() {
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
        let mut notes = Vec::new();
        {
            let mut open = flights();
            for flight in open.iter_mut() {
                let elapsed = flight.started.elapsed().as_secs();
                if elapsed < flight.next_due {
                    continue;
                }
                notes.push((flight.id, format!(
                    "CreatePipelineState #{} {} still running {elapsed}s",
                    flight.id, flight.label
                )));
                flight.next_due = elapsed.saturating_add(15);
            }
        }
        for (id, note) in notes {
            // The compile can finish between the snapshot and this write.
            if flights().iter().any(|flight| flight.id == id) {
                crate::diagnostics::log(&note);
            }
        }
    }
}

#[cfg(test)]
#[path = "tests/big_stack.rs"]
mod tests;
