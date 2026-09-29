//! What the worker and every backend share: a flag saying the queue may have grown, the worker's
//! latch to wake it with, its state, and a restart request. Nothing else is in shared memory, and
//! in particular nothing a process allocated for itself, so no process can ever free another's.
use pgrx::pg_sys;
use std::ffi::c_void;
use std::sync::atomic::{AtomicPtr, AtomicU32, Ordering};

#[repr(C)]
pub struct Shared {
	/// Set when a committed transaction queued a request; the worker clears it before it reads.
	should_wake: AtomicU32,
	/// Set by `net.worker_restart()`.
	restart: AtomicU32,
	/// [`NOT_YET`], [`RUNNING`] or [`EXITED`].
	status: AtomicU32,
	/// The running worker's latch (in shared memory, part of its PGPROC), or null.
	latch: AtomicPtr<pg_sys::Latch>,
}

pub const NOT_YET: u32 = 1;
pub const RUNNING: u32 = 2;
pub const EXITED: u32 = 3;

static mut SHARED: *mut Shared = std::ptr::null_mut();
static mut PREV_REQUEST: pg_sys::shmem_request_hook_type = None;
static mut PREV_STARTUP: pg_sys::shmem_startup_hook_type = None;

fn get() -> &'static Shared {
	// SAFETY: set once in the postmaster's startup hook, before any backend or worker exists, and
	// inherited by every process it forks.
	let p = unsafe { SHARED };
	assert!(
		!p.is_null(),
		"snout_net's shared memory is not set up: is it in shared_preload_libraries?"
	);
	unsafe { &*p }
}

pub unsafe fn install() {
	unsafe {
		PREV_REQUEST = pg_sys::shmem_request_hook;
		pg_sys::shmem_request_hook = Some(request);
		PREV_STARTUP = pg_sys::shmem_startup_hook;
		pg_sys::shmem_startup_hook = Some(startup);
	}
}

#[pgrx::pg_guard]
unsafe extern "C-unwind" fn request() {
	unsafe {
		if let Some(prev) = PREV_REQUEST {
			prev();
		}
		pg_sys::RequestAddinShmemSpace(std::mem::size_of::<Shared>());
	}
}

#[pgrx::pg_guard]
unsafe extern "C-unwind" fn startup() {
	unsafe {
		if let Some(prev) = PREV_STARTUP {
			prev();
		}
		let mut found = false;
		let p = pg_sys::ShmemInitStruct(
			c"snout_net".as_ptr(),
			std::mem::size_of::<Shared>(),
			&mut found,
		) as *mut Shared;
		if !found {
			p.write(Shared {
				// The queue is read once when the worker starts, whatever is in it.
				should_wake: AtomicU32::new(1),
				restart: AtomicU32::new(0),
				status: AtomicU32::new(NOT_YET),
				latch: AtomicPtr::new(std::ptr::null_mut()),
			});
		}
		SHARED = p;
	}
}

/// Wakes the worker. The flag and the latch are both set every time: setting a latch that is
/// already set costs one read, and setting it only when the flag flips from 0 left a worker asleep
/// for good whenever the flag was still 1 from before (a worker that started before the extension
/// existed never cleared it).
pub fn wake_worker() {
	let s = get();
	s.should_wake.store(1, Ordering::Release);
	set_latch(s);
}

fn set_latch(s: &Shared) {
	let latch = s.latch.load(Ordering::Acquire);
	if !latch.is_null() {
		unsafe { pg_sys::SetLatch(latch) };
	}
}

/// Asks the worker to exit, which Postgres answers by starting a new one.
pub fn request_restart() {
	let s = get();
	s.restart.store(1, Ordering::Release);
	set_latch(s);
}

/// The worker's side: take the wake flag (true if it was set).
pub fn take_wake() -> bool {
	get().should_wake.swap(0, Ordering::AcqRel) == 1
}

/// The worker's side: take a restart request.
pub fn take_restart() -> bool {
	get().restart.swap(0, Ordering::AcqRel) == 1
}

pub fn status() -> u32 {
	get().status.load(Ordering::Acquire)
}

/// The worker's side: publish that it is running, with the latch to wake it by.
pub unsafe fn publish_running() {
	let s = get();
	s.latch.store(unsafe { pg_sys::MyLatch }, Ordering::Release);
	s.status.store(RUNNING, Ordering::Release);
	unsafe { pg_sys::on_proc_exit(Some(on_exit), pg_sys::Datum::from(0usize)) };
}

#[pgrx::pg_guard]
unsafe extern "C-unwind" fn on_exit(_code: std::ffi::c_int, _arg: pg_sys::Datum) {
	let s = get();
	s.latch.store(std::ptr::null_mut(), Ordering::Release);
	s.status.store(EXITED, Ordering::Release);
	// Whatever was queued is read again by the next worker.
	s.should_wake.store(1, Ordering::Release);
}

// ---- The commit-time wake, registered once per backend.

static mut CALLBACK_REGISTERED: bool = false;
static mut WAKE_AT_COMMIT: bool = false;

/// Called by `net.wake()`: wake the worker when this transaction commits, and not before, so a
/// statement that queues 100,000 requests wakes it once, and a rolled-back one not at all.
pub fn wake_at_commit() {
	unsafe {
		if !CALLBACK_REGISTERED {
			pg_sys::RegisterXactCallback(Some(at_transaction_end), std::ptr::null_mut());
			CALLBACK_REGISTERED = true;
		}
		WAKE_AT_COMMIT = true;
	}
}

#[pgrx::pg_guard]
unsafe extern "C-unwind" fn at_transaction_end(event: pg_sys::XactEvent::Type, _arg: *mut c_void) {
	unsafe {
		match event {
			pg_sys::XactEvent::XACT_EVENT_COMMIT
			| pg_sys::XactEvent::XACT_EVENT_PARALLEL_COMMIT => {
				if WAKE_AT_COMMIT {
					WAKE_AT_COMMIT = false;
					wake_worker();
				}
			}
			pg_sys::XactEvent::XACT_EVENT_ABORT
			| pg_sys::XactEvent::XACT_EVENT_PARALLEL_ABORT
			| pg_sys::XactEvent::XACT_EVENT_PREPARE => {
				WAKE_AT_COMMIT = false;
			}
			_ => {}
		}
	}
}
