//! Where each desktop queue has got to, for explaining a stalled memory wait.
//!
//! After every batch and every fence operation the queue signals a private
//! fence. When a wait stalls, the first operation past that fence's value is
//! the one its queue is stopped on.
use std::collections::VecDeque;
use std::sync::{LazyLock, Mutex, Once};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;

/// Operations kept per queue.
const KEEP: usize = 256;

struct Op {
    mark: u64,
    text: String,
    /// The fence and value a title wait or signal named.
    fence: Option<(ID3D12Fence, u64)>,
}

struct Track {
    queue: usize,
    kind: i32,
    progress: ID3D12Fence,
    mark: u64,
    armed: Option<u64>,
    ops: VecDeque<Op>,
}

// SAFETY: D3D12 fences are free-threaded.
unsafe impl Send for Track {}

static TRACKS: Mutex<Vec<Track>> = Mutex::new(Vec::new());

pub(crate) fn enabled() -> bool {
    static ENABLED: LazyLock<bool> = LazyLock::new(|| {
        std::env::var_os("TUXBOX_TRACE_QUEUES").as_deref() == Some(std::ffi::OsStr::new("1"))
    });
    *ENABLED
}

fn start_monitor() {
    static START: Once = Once::new();
    if enabled() {
        START.call_once(|| {
            let _ = std::thread::Builder::new().name("TuxBox queues".into()).spawn(|| loop {
                std::thread::sleep(Duration::from_secs(4));
                crate::diagnostics::log("queue trace snapshot");
                report_calls();
                if let Ok(tracks) = TRACKS.try_lock() {
                    report_tracks(&tracks);
                } else {
                    crate::diagnostics::log("queue trace progress tracker busy");
                }
            });
        });
    }
}

struct Call {
    id: u64,
    queue: Option<usize>,
    stage: &'static str,
    since: Instant,
    fence: Option<(ID3D12Fence, u64)>,
}

// SAFETY: the only native reference is a free-threaded D3D12 fence.
unsafe impl Send for Call {}

static CALLS: Mutex<Vec<Call>> = Mutex::new(Vec::new());

pub(crate) struct Span(Option<u64>);

impl Drop for Span {
    fn drop(&mut self) {
        if let Some(id) = self.0 {
            let mut calls = CALLS.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(index) = calls.iter().position(|call| call.id == id) {
                calls.swap_remove(index);
            }
        }
    }
}

/// Watch a potentially blocking CPU call without adding a GPU signal or wait.
pub(crate) fn watch(
    queue: Option<&ID3D12CommandQueue>,
    stage: &'static str,
    fence: Option<(&ID3D12Fence, u64)>,
) -> Span {
    if !enabled() {
        return Span(None);
    }
    start_monitor();
    let mut calls = CALLS.lock().unwrap_or_else(|e| e.into_inner());
    if calls.len() >= KEEP {
        return Span(None);
    }
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    calls.push(Call {
        id, queue: queue.map(|queue| queue.as_raw().addr()), stage, since: Instant::now(),
        fence: fence.map(|(fence, value)| (fence.clone(), value)),
    });
    Span(Some(id))
}

fn report_calls() {
    let calls: Vec<_> = CALLS.lock().unwrap_or_else(|e| e.into_inner()).iter()
        .filter(|call| call.since.elapsed() >= Duration::from_secs(2))
        .map(|call| (call.id, call.queue, call.stage, call.since.elapsed(), call.fence.clone()))
        .collect();
    for (id, queue, stage, age, fence) in calls {
        let queue = queue.map_or_else(|| "-".into(), |value| format!("{value:#x}"));
        // Print the wait before querying the driver for its fence value.
        crate::diagnostics::log(&format!("queue trace call #{id} queue={queue} {stage} pending={}ms", age.as_millis()));
        if let Some((fence, value)) = fence {
            let completed = unsafe { fence.GetCompletedValue() };
            crate::diagnostics::log(&format!("queue trace call #{id} fence={:#x} target={value} completed={completed}", fence.as_raw().addr()));
        }
    }
}

/// Arm a shared wake event on every queue that has uncompleted operations.
pub(super) fn arm_progress_wake(event: windows::Win32::Foundation::HANDLE) {
    let mut tracks = TRACKS.lock().unwrap_or_else(|e| e.into_inner());
    for track in tracks.iter_mut() {
        // SAFETY: each private progress fence stays alive in TRACKS.
        let done = unsafe { track.progress.GetCompletedValue() };
        if done < track.mark && done != u64::MAX {
            let target = done.saturating_add(1);
            if track.armed != Some(target)
                && unsafe { track.progress.SetEventOnCompletion(target, event) }.is_ok()
            {
                track.armed = Some(target);
            }
        }
    }
}

/// Note a wait TuxBox itself just put on `queue`.
pub(crate) fn waited(queue: &ID3D12CommandQueue, what: &str, fence: &ID3D12Fence, value: u64) {
    // SAFETY: the queue is live for this call.
    let kind = unsafe { queue.GetDesc() }.Type.0;
    let _ = note(queue, kind, what.into(), Some((fence.clone(), value)));
}

/// Note an operation just given to `queue`, returning the signaled progress
/// fence and value when recorded.
pub(crate) fn note(
    queue: &ID3D12CommandQueue,
    kind: i32,
    text: String,
    fence: Option<(ID3D12Fence, u64)>,
) -> Option<(ID3D12Fence, u64)> {
    start_monitor();
    let key = queue.as_raw().addr();
    let mut tracks = TRACKS.lock().unwrap_or_else(|e| e.into_inner());
    let at = match tracks.iter().position(|track| track.queue == key) {
        Some(at) => at,
        None => {
            let mut device: Option<ID3D12Device> = None;
            // SAFETY: the queue is live for this call.
            if unsafe { queue.GetDevice(&mut device) }.is_err() {
                return None;
            }
            let device = device?;
            // SAFETY: a plain fence on the queue's device.
            let Ok(progress) =
                (unsafe { device.CreateFence::<ID3D12Fence>(0, D3D12_FENCE_FLAG_NONE) })
            else {
                return None;
            };
            tracks.push(Track { queue: key, kind, progress, mark: 0, armed: None, ops: VecDeque::new() });
            tracks.len() - 1
        }
    };
    let track = &mut tracks[at];
    track.mark += 1;
    // SAFETY: the queue and its private fence are live.
    if unsafe { queue.Signal(&track.progress, track.mark) }.is_err() {
        return None;
    }
    let marker = (track.progress.clone(), track.mark);
    if track.ops.len() >= KEEP {
        track.ops.pop_front();
    }
    track.ops.push_back(Op { mark: track.mark, text, fence });
    drop(tracks);
    super::memory_wait::notify_waiters();
    Some(marker)
}

/// Log, for each queue that is behind, the operations it has not passed.
pub(crate) fn report() {
    let tracks = TRACKS.lock().unwrap_or_else(|e| e.into_inner());
    report_tracks(&tracks);
}

fn report_tracks(tracks: &[Track]) {
    for track in tracks {
        // SAFETY: the private fence is live.
        let done = unsafe { track.progress.GetCompletedValue() };
        let behind = track.mark.saturating_sub(done);
        let mut summary = format!("queue {:#x} type {} passed {done} of {} operations",
                                  track.queue, track.kind, track.mark);
        if enabled() {
            use std::fmt::Write;
            let _ = write!(summary, " (progress fence {:#x})", track.progress.as_raw().addr());
        }
        crate::diagnostics::log(&summary);
        if behind == 0 {
            continue;
        }
        for op in track.ops.iter().filter(|op| op.mark > done).take(6) {
            let fence = op.fence.as_ref().map_or_else(String::new, |(fence, value)| {
                // SAFETY: the clone keeps the title's fence alive.
                let now = unsafe { fence.GetCompletedValue() };
                format!(" fence {:#x} value {value} (now {now})", fence.as_raw().addr())
            });
            crate::diagnostics::log(&format!(
                "queue {:#x} pending #{}: {}{fence}",
                track.queue, op.mark, op.text
            ));
            if enabled() && op.text == "title wait" && let Some((fence, value)) = &op.fence {
                for producer in tracks {
                    if let Some(signal) = producer.ops.iter().find(|signal| {
                        signal.text == "title signal"
                            && signal.fence.as_ref().is_some_and(|(target, reached)| target == fence && reached >= value)
                    }) {
                        crate::diagnostics::log(&format!(
                            "queue trace producer wait={:#x}/{} signal={:#x}/{} fence={:#x}",
                            track.queue, op.mark, producer.queue, signal.mark, fence.as_raw().addr()
                        ));
                    }
                }
            }
        }
    }
}
