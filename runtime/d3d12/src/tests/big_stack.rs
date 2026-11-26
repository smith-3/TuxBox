#[test]
fn deep_recursion_fits_on_the_helper_stack() {
    fn depth(n: u32) -> u32 {
        let pad = std::hint::black_box([0u8; 1024]);
        if n == 0 { pad[0] as u32 } else { depth(n - 1) + std::hint::black_box(1) }
    }
    // About 8 MiB of frames: more than a 1 MiB thread stack holds.
    assert_eq!(super::run_logged("test", || depth(8000)), 8000);
}

#[test]
fn returns_borrowed_results() {
    let text = String::from("pso");
    assert_eq!(super::run_logged("test", || text.len()), 3);
}

#[test]
fn deadline_text_uses_two_seconds_unless_zero_or_invalid() {
    use std::time::Duration;
    assert_eq!(super::parse_deadline(None), Duration::from_secs(2));
    assert_eq!(super::parse_deadline(Some("0")), Duration::ZERO);
    assert_eq!(super::parse_deadline(Some("20")), Duration::from_secs(20));
    assert_eq!(super::parse_deadline(Some("nope")), Duration::from_secs(2));
}

#[test]
fn async_limit_accepts_a_single_worker_and_bounds_the_override() {
    assert_eq!(super::compile_limit(Some("1"), 32), 1);
    assert_eq!(super::compile_limit(Some("4"), 32), 4);
    assert_eq!(super::compile_limit(Some("0"), 32), 16);
    assert_eq!(super::compile_limit(Some("17"), 32), 16);
    assert_eq!(super::compile_limit(None, 1), 2);
}

#[test]
fn a_zero_deadline_waits_for_the_compile() {
    let _guard = lock();
    let wait = super::run_for("block", std::time::Duration::ZERO, || {
        std::thread::sleep(std::time::Duration::from_millis(30));
        4
    });
    assert!(matches!(wait, super::PipelineWait::Ready(4)));
    assert_eq!(super::inflight(), 0);
}

#[test]
fn a_finished_compile_releases_its_slot() {
    let _guard = lock();
    let wait = super::run_for("fast", std::time::Duration::from_secs(2), || 5);
    assert!(matches!(wait, super::PipelineWait::Ready(5)));
    assert_eq!(super::inflight(), 0);
}

#[test]
fn a_slow_compile_is_detached_and_still_finishes() {
    let _guard = lock();
    let gate = std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let gate_in_job = std::sync::Arc::clone(&gate);
    let done_in_job = std::sync::Arc::clone(&done);
    let started = std::time::Instant::now();
    let wait = super::run_for("slow", std::time::Duration::from_millis(40), move || {
        let (lock, wake) = &*gate_in_job;
        let mut open = lock.lock().unwrap_or_else(|error| error.into_inner());
        while !*open {
            open = wake.wait(open).unwrap_or_else(|error| error.into_inner());
        }
        done_in_job.store(true, std::sync::atomic::Ordering::SeqCst);
        7
    });
    let super::PipelineWait::Deferred(pending) = wait else { panic!("the compile should outlive the deadline") };
    assert!(started.elapsed() < std::time::Duration::from_millis(500));
    assert!(!done.load(std::sync::atomic::Ordering::SeqCst));
    {
        let (lock, wake) = &*gate;
        *lock.lock().unwrap_or_else(|error| error.into_inner()) = true;
        wake.notify_one();
    }
    // The first user receives the compiled result, as often as it asks.
    assert_eq!(pending.wait(), Some(7));
    assert_eq!(pending.wait(), Some(7));
    assert!(done.load(std::sync::atomic::Ordering::SeqCst));
    wait_until(|| super::inflight() == 0);
}

#[test]
fn first_use_prioritizes_its_compile_without_exceeding_the_limit() {
    let _guard = lock();
    let previous = super::MAX_LIVE.swap(1, std::sync::atomic::Ordering::SeqCst);
    struct Restore(usize);
    impl Drop for Restore {
        fn drop(&mut self) {
            super::MAX_LIVE.store(self.0, std::sync::atomic::Ordering::SeqCst);
        }
    }
    let _restore = Restore(previous);
    let gate = std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let gate_in_job = std::sync::Arc::clone(&gate);
    let holder = super::run_for("hold", std::time::Duration::from_millis(30), move || {
        let (lock, wake) = &*gate_in_job;
        let mut open = lock.lock().unwrap_or_else(|error| error.into_inner());
        while !*open {
            open = wake.wait(open).unwrap_or_else(|error| error.into_inner());
        }
        1
    });
    let super::PipelineWait::Deferred(holder) = holder else { panic!("the holder should outlive the deadline") };
    // The only slot is busy, so this compile waits in the queue.
    let (job_started, started) = std::sync::mpsc::channel();
    let queued = super::run_for("queued", std::time::Duration::from_secs(5), move || {
        job_started.send(()).unwrap();
        2
    });
    let super::PipelineWait::Deferred(queued) = queued else { panic!("a compile past the cap should be queued") };
    std::thread::scope(|scope| {
        let (wait_started, waiting) = std::sync::mpsc::channel();
        let user = scope.spawn(move || {
            wait_started.send(()).unwrap();
            queued.wait()
        });
        waiting.recv_timeout(std::time::Duration::from_secs(1)).unwrap();
        let before_release = started.recv_timeout(std::time::Duration::from_millis(150));
        let (lock, wake) = &*gate;
        *lock.lock().unwrap_or_else(|error| error.into_inner()) = true;
        wake.notify_all();
        assert_eq!(user.join().unwrap(), Some(2));
        assert_eq!(before_release, Err(std::sync::mpsc::RecvTimeoutError::Timeout));
    });
    assert_eq!(holder.wait(), Some(1));
    wait_until(|| super::inflight() == 0);
    assert!(super::queue().is_empty());
}

#[test]
fn queue_limit_accepts_a_bounded_override() {
    assert_eq!(super::queue_limit(None), 64);
    assert_eq!(super::queue_limit(Some(" 8 ")), 8);
    assert_eq!(super::queue_limit(Some("0")), 64);
    assert_eq!(super::queue_limit(Some("4097")), 64);
    assert_eq!(super::queue_limit(Some("many")), 64);
}

#[test]
fn creation_past_the_queue_limit_waits_for_room() {
    let _guard = lock();
    struct Restore(&'static std::sync::atomic::AtomicUsize, usize);
    impl Drop for Restore {
        fn drop(&mut self) {
            self.0.store(self.1, std::sync::atomic::Ordering::SeqCst);
        }
    }
    let _live = Restore(&super::MAX_LIVE, super::MAX_LIVE.swap(1, std::sync::atomic::Ordering::SeqCst));
    let _queued = Restore(&super::MAX_QUEUED, super::MAX_QUEUED.swap(1, std::sync::atomic::Ordering::SeqCst));
    let gate = std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let gate_in_job = std::sync::Arc::clone(&gate);
    let holder = super::run_for("hold", std::time::Duration::from_millis(30), move || {
        let (lock, wake) = &*gate_in_job;
        let mut open = lock.lock().unwrap_or_else(|error| error.into_inner());
        while !*open {
            open = wake.wait(open).unwrap_or_else(|error| error.into_inner());
        }
        1
    });
    let super::PipelineWait::Deferred(holder) = holder else { panic!("the holder should outlive the deadline") };
    // The only slot is busy, so this fills the one queue entry.
    let first = super::run_for("first queued", std::time::Duration::from_secs(5), || 2);
    let super::PipelineWait::Deferred(first) = first else { panic!("a compile past the cap should be queued") };
    std::thread::scope(|scope| {
        let (returned, creation) = std::sync::mpsc::channel();
        scope.spawn(move || {
            let wait = super::run_for("second queued", std::time::Duration::from_secs(5), || 3);
            let super::PipelineWait::Deferred(second) = wait else { panic!("expected a queued compile") };
            returned.send(()).unwrap();
            assert_eq!(second.wait(), Some(3));
        });
        let while_full = creation.recv_timeout(std::time::Duration::from_millis(250));
        let queued_while_full = super::queue().len();
        // Release before asserting, so a failure reports instead of hanging.
        let (lock, wake) = &*gate;
        *lock.lock().unwrap_or_else(|error| error.into_inner()) = true;
        wake.notify_all();
        assert_eq!(
            while_full,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout),
            "creation returned while the queue was full"
        );
        assert_eq!(queued_while_full, 1);
        creation.recv_timeout(std::time::Duration::from_secs(2)).expect("room was made");
    });
    assert_eq!(holder.wait(), Some(1));
    assert_eq!(first.wait(), Some(2));
    wait_until(|| super::inflight() == 0);
    assert!(super::queue().is_empty());
}

#[test]
fn dropping_a_completed_deferred_pipeline_releases_its_result() {
    let _guard = lock();
    let (release, gate) = std::sync::mpsc::channel();
    let result = std::sync::Arc::new(7);
    let retained = std::sync::Arc::downgrade(&result);
    let wait = super::run_for("release result", std::time::Duration::from_millis(20), move || {
        gate.recv_timeout(std::time::Duration::from_secs(1)).unwrap();
        result
    });
    let super::PipelineWait::Deferred(pending) = wait else { panic!("expected a deferred result") };
    release.send(()).unwrap();
    assert_eq!(pending.wait().as_deref(), Some(&7));
    drop(pending);
    wait_until(|| super::inflight() == 0);
    assert!(retained.upgrade().is_none());
}

#[test]
fn compiles_past_the_cap_are_queued_and_still_run() {
    let _guard = lock();
    let previous = super::MAX_LIVE.swap(2, std::sync::atomic::Ordering::SeqCst);
    struct Restore(usize);
    impl Drop for Restore {
        fn drop(&mut self) {
            super::MAX_LIVE.store(self.0, std::sync::atomic::Ordering::SeqCst);
        }
    }
    let _restore = Restore(previous);
    let gate = std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    for _ in 0..2 {
        let gate_in_job = std::sync::Arc::clone(&gate);
        let wait = super::run_for("hold", std::time::Duration::from_millis(30), move || {
            let (lock, wake) = &*gate_in_job;
            let mut open = lock.lock().unwrap_or_else(|error| error.into_inner());
            while !*open {
                open = wake.wait(open).unwrap_or_else(|error| error.into_inner());
            }
        });
        assert!(matches!(wait, super::PipelineWait::Deferred(_)));
    }
    let ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ran_in_job = std::sync::Arc::clone(&ran);
    let started = std::time::Instant::now();
    let wait = super::run_for("ninth", std::time::Duration::from_secs(5), move || {
        ran_in_job.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    assert!(matches!(wait, super::PipelineWait::Deferred(_)));
    assert!(started.elapsed() < std::time::Duration::from_millis(500));
    assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
    {
        let (lock, wake) = &*gate;
        *lock.lock().unwrap_or_else(|error| error.into_inner()) = true;
        wake.notify_all();
    }
    wait_until(|| ran.load(std::sync::atomic::Ordering::SeqCst));
    wait_until(|| super::inflight() == 0);
}

fn lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|error| error.into_inner())
}

fn wait_until(done: impl Fn() -> bool) {
    let end = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while !done() {
        assert!(std::time::Instant::now() < end, "pipeline helper did not finish");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}
