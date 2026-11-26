//! Publish finished GPU copies on a per-queue thread. A native queue wait
//! places that CPU publication before the title's next fence signal.
use super::writeback::Readback;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{E_FAIL, E_OUTOFMEMORY, HANDLE, S_OK};
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Result;

/// Cap unheld in-flight `D3D12_HEAP_TYPE_READBACK` buffers per queue worker to
/// 512 MiB so rapid submissions cannot pile up gigabytes of pinned host RAM
/// while still allowing a full 3D frame's render-target writebacks without
/// stalling the title's submission thread.
const MAX_INFLIGHT_BYTES: usize = 512 * 1024 * 1024;

/// Short bounded wait for backpressure so the title's submission ring never
/// stalls or wraps while waiting on CPU texture publication.
const BUDGET_WAIT: Duration = Duration::from_millis(250);

struct Budget {
    unheld_bytes: Mutex<usize>,
    changed: Condvar,
}

impl Budget {
    fn new() -> Self {
        Self {
            unheld_bytes: Mutex::new(0),
            changed: Condvar::new(),
        }
    }

    fn wait_below(&self, limit: usize) {
        let mut guard = self.unheld_bytes.lock().unwrap_or_else(|e| e.into_inner());
        if *guard < limit {
            return;
        }
        let deadline = Instant::now() + BUDGET_WAIT;
        while *guard >= limit {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            let (next, timeout) = self
                .changed
                .wait_timeout(guard, remaining)
                .unwrap_or_else(|e| e.into_inner());
            guard = next;
            if timeout.timed_out() {
                break;
            }
        }
    }

    fn add(&self, bytes: usize) {
        let mut guard = self.unheld_bytes.lock().unwrap_or_else(|e| e.into_inner());
        *guard = guard.saturating_add(bytes);
    }

    fn release(&self, bytes: usize) {
        let mut guard = self.unheld_bytes.lock().unwrap_or_else(|e| e.into_inner());
        *guard = guard.saturating_sub(bytes);
        self.changed.notify_all();
    }
}

struct Job {
    readback: Readback,
    ready: ID3D12Fence,
    tick: u64,
    tracked_bytes: usize,
}

pub(super) struct Worker {
    pub(super) published: ID3D12Fence,
    sender: mpsc::Sender<Job>,
    budget: Arc<Budget>,
}

impl Worker {
    pub(super) fn wait_for_budget(&self) {
        self.budget.wait_below(MAX_INFLIGHT_BYTES);
    }

    pub(super) fn enqueue(
        &self,
        readback: Readback,
        ready: &ID3D12Fence,
        tick: u64,
        held: bool,
    ) -> Result<()> {
        let tracked_bytes = if held { 0 } else { readback.byte_size() };
        if tracked_bytes != 0 {
            self.budget.add(tracked_bytes);
        }
        let job = Job {
            readback,
            ready: ready.clone(),
            tick,
            tracked_bytes,
        };
        // Losing the worker is fatal to this queue. Keep the recording alive
        // until the driver retires the already-issued work.
        if let Err(error) = self.sender.send(job) {
            let job = error.0;
            if job.tracked_bytes != 0 {
                self.budget.release(job.tracked_bytes);
            }
            if unsafe { job.ready.GetCompletedValue() } < tick {
                std::mem::forget(job);
            }
            return Err(E_FAIL.into());
        }
        Ok(())
    }
}

#[derive(Default)]
pub(crate) struct QueueAccess {
    pub(crate) error: Arc<AtomicI32>,
    reader: OnceLock<ID3D12Fence>,
    worker: OnceLock<Worker>,
}

impl QueueAccess {
    pub(super) fn reader(&self, device: &ID3D12Device) -> Result<&ID3D12Fence> {
        if let Some(fence) = self.reader.get() {
            return Ok(fence);
        }
        let created = unsafe { device.CreateFence(0, D3D12_FENCE_FLAG_NONE)? };
        Ok(self.reader.get_or_init(|| created))
    }

    pub(super) fn worker(&self, device: &ID3D12Device, failed: &Arc<AtomicI32>) -> Result<&Worker> {
        if let Some(worker) = self.worker.get() {
            return Ok(worker);
        }
        let published: ID3D12Fence = unsafe { device.CreateFence(0, D3D12_FENCE_FLAG_NONE)? };
        let gate = published.clone();
        let error = self.error.clone();
        let failed = failed.clone();
        let budget = Arc::new(Budget::new());
        let worker_budget = budget.clone();
        let (sender, jobs) = mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("TuxBox texture readback".into())
            .spawn(move || {
                let remember = |result: Result<()>| {
                    if let Err(cause) = result {
                        for slot in [&error, &failed] {
                            let _ = slot.compare_exchange(
                                S_OK.0,
                                cause.code().0,
                                Ordering::AcqRel,
                                Ordering::Acquire,
                            );
                        }
                        crate::diagnostics::recording_error(format_args!(
                            "placed texture publication failed: {:?}",
                            cause.code()
                        ));
                    }
                };
                for job in jobs {
                    let Job {
                        readback,
                        ready,
                        tick,
                        tracked_bytes,
                    } = job;
                    if unsafe { ready.GetCompletedValue() } < tick
                        && unsafe { ready.SetEventOnCompletion(tick, HANDLE::default()) }.is_err()
                    {
                        // Event registration can run out of OS resources. The
                        // recording still belongs to the GPU until this fence.
                        while unsafe { ready.GetCompletedValue() } < tick {
                            std::thread::sleep(std::time::Duration::from_millis(2));
                        }
                    }
                    if unsafe { ready.GetCompletedValue() } == u64::MAX {
                        drop(readback);
                        remember(Err(E_FAIL.into()));
                    } else {
                        let result = readback.complete();
                        if result.is_ok() {
                            static FIRST_PUBLICATION: std::sync::Once = std::sync::Once::new();
                            crate::diagnostics::log_once(&FIRST_PUBLICATION, || {
                                "completed GPU writeback for a guest-backed placed texture".into()
                            });
                        }
                        remember(result);
                    }
                    if tracked_bytes != 0 {
                        worker_budget.release(tracked_bytes);
                    }
                    // Release this queue's GPU gate even after failed mapping or
                    // publication, so guest fence waiters can observe its error.
                    remember(unsafe { gate.Signal(tick) });
                }
            })
            .map_err(|_| E_OUTOFMEMORY)?;
        Ok(self.worker.get_or_init(|| Worker {
            published,
            sender,
            budget,
        }))
    }
}
