//! The settings. Every one is `sighup` (a server's configuration or command line sets it, a reload
//! changes it, no session can) except the worker's type, which is fixed when the server starts.
use crate::egress::Policy;
use pgrx::pg_sys;
use std::ffi::{CStr, c_char, c_int, c_void};

static mut DATABASE_NAME: *mut c_char = std::ptr::null_mut();
static mut USERNAME: *mut c_char = std::ptr::null_mut();
static mut TTL: *mut c_char = std::ptr::null_mut();
static mut ALLOWED_NETWORKS: *mut c_char = std::ptr::null_mut();
static mut WORKER_TYPE: *mut c_char = std::ptr::null_mut();
static mut MAX_CONCURRENT: c_int = 200;
static mut MAX_TIMEOUT_MS: c_int = 600_000;
static mut MAX_RESPONSE_BYTES: c_int = 64 * 1024 * 1024;

fn read(setting: *const *mut c_char) -> Option<&'static str> {
	// SAFETY: Postgres owns the string and replaces the pointer only between statements.
	let value = unsafe { *setting };
	if value.is_null() {
		None
	} else {
		unsafe { CStr::from_ptr(value) }.to_str().ok()
	}
}

/// The database the worker serves. Its tables live there, so it is the one it connects to.
pub fn database_name() -> String {
	read(&raw const DATABASE_NAME)
		.unwrap_or("postgres")
		.to_owned()
}

/// The role the worker connects as; unset, the role that created the cluster.
pub fn username() -> Option<String> {
	read(&raw const USERNAME)
		.filter(|s| !s.is_empty())
		.map(str::to_owned)
}

/// How long a response is kept, as an interval.
pub fn ttl() -> String {
	read(&raw const TTL).unwrap_or("6 hours").to_owned()
}

/// Networks a request may reach although they are internal.
pub fn policy() -> Policy {
	read(&raw const ALLOWED_NETWORKS)
		.and_then(Policy::parse)
		.unwrap_or_default()
}

/// What the worker is called in `pg_stat_activity.backend_type`.
pub fn worker_type() -> String {
	read(&raw const WORKER_TYPE)
		.unwrap_or("snout_net worker")
		.to_owned()
}

pub fn max_concurrent() -> usize {
	unsafe { MAX_CONCURRENT }.max(1) as usize
}

pub fn max_timeout_ms() -> i32 {
	unsafe { MAX_TIMEOUT_MS }
}

pub fn max_response_bytes() -> usize {
	unsafe { MAX_RESPONSE_BYTES }.max(0) as usize
}

pub fn init() {
	unsafe {
		string(
			c"snout_net.database_name",
			c"The database whose request queue the worker serves",
			&raw mut DATABASE_NAME,
			c"postgres".as_ptr(),
			pg_sys::GucContext::PGC_SIGHUP,
			None,
		);
		string(
			c"snout_net.username",
			c"The role the worker connects as; unset, the bootstrap superuser",
			&raw mut USERNAME,
			std::ptr::null(),
			pg_sys::GucContext::PGC_SIGHUP,
			None,
		);
		string(
			c"snout_net.ttl",
			c"How long a response is kept in net._http_response, as an interval",
			&raw mut TTL,
			c"6 hours".as_ptr(),
			pg_sys::GucContext::PGC_SIGHUP,
			Some(check_ttl),
		);
		string(
			c"snout_net.allowed_networks",
			c"Internal networks a request may reach anyway, comma-separated (10.0.0.0/8, fd00::/8)",
			&raw mut ALLOWED_NETWORKS,
			c"".as_ptr(),
			pg_sys::GucContext::PGC_SIGHUP,
			Some(check_networks),
		);
		string(
			c"snout_net.worker_type",
			c"The worker's backend_type in pg_stat_activity",
			&raw mut WORKER_TYPE,
			c"snout_net worker".as_ptr(),
			pg_sys::GucContext::PGC_POSTMASTER,
			None,
		);
		int(
			c"snout_net.max_concurrent",
			c"How many requests may be on the network at once",
			&raw mut MAX_CONCURRENT,
			200,
			1,
			4096,
			0,
		);
		int(
			c"snout_net.max_timeout_ms",
			c"The longest timeout_milliseconds a request may ask for",
			&raw mut MAX_TIMEOUT_MS,
			600_000,
			1,
			86_400_000,
			pg_sys::GUC_UNIT_MS as c_int,
		);
		int(
			c"snout_net.max_response_bytes",
			c"The largest response body kept; a larger one is an error",
			&raw mut MAX_RESPONSE_BYTES,
			64 * 1024 * 1024,
			1024,
			1024 * 1024 * 1024 - 1,
			pg_sys::GUC_UNIT_BYTE as c_int,
		);
		pg_sys::MarkGUCPrefixReserved(c"snout_net".as_ptr());
	}
}

unsafe fn string(
	name: &'static CStr,
	description: &'static CStr,
	variable: *mut *mut c_char,
	boot: *const c_char,
	context: pg_sys::GucContext::Type,
	check: pg_sys::GucStringCheckHook,
) {
	unsafe {
		pg_sys::DefineCustomStringVariable(
			name.as_ptr(),
			description.as_ptr(),
			std::ptr::null(),
			variable,
			boot,
			context,
			0,
			check,
			None,
			None,
		)
	};
}

unsafe fn int(
	name: &'static CStr,
	description: &'static CStr,
	variable: *mut c_int,
	boot: c_int,
	min: c_int,
	max: c_int,
	flags: c_int,
) {
	unsafe {
		pg_sys::DefineCustomIntVariable(
			name.as_ptr(),
			description.as_ptr(),
			std::ptr::null(),
			variable,
			boot,
			min,
			max,
			pg_sys::GucContext::PGC_SIGHUP,
			flags,
			None,
			None,
			None,
		)
	};
}

unsafe fn check_detail(detail: &'static CStr) {
	unsafe { pg_sys::GUC_check_errdetail_string = pg_sys::pstrdup(detail.as_ptr()) };
}

/// A list of networks that does not parse is refused, and the old value stays.
#[pgrx::pg_guard]
unsafe extern "C-unwind" fn check_networks(
	value: *mut *mut c_char,
	_extra: *mut *mut c_void,
	_source: pg_sys::GucSource::Type,
) -> bool {
	let text = unsafe { *value };
	if text.is_null() {
		return true;
	}
	let ok = unsafe { CStr::from_ptr(text) }
		.to_str()
		.ok()
		.and_then(Policy::parse)
		.is_some();
	if !ok {
		unsafe {
			check_detail(c"The value must be a comma-separated list of networks, such as 172.18.0.0/16 or fd00::/8.")
		};
	}
	ok
}

unsafe extern "C-unwind" {
	fn interval_in(fcinfo: pg_sys::FunctionCallInfo) -> pg_sys::Datum;
	fn DirectInputFunctionCallSafe(
		func: pg_sys::PGFunction,
		str_: *mut c_char,
		typioparam: pg_sys::Oid,
		typmod: i32,
		escontext: *mut pg_sys::Node,
		result: *mut pg_sys::Datum,
	) -> bool;
}

/// The TTL must be an interval that is not negative, checked when it is set rather than found out
/// by the worker. Parsed with soft errors: this runs in the postmaster too, where an ERROR is not
/// a refusal but a server that stops.
#[pgrx::pg_guard]
unsafe extern "C-unwind" fn check_ttl(
	value: *mut *mut c_char,
	_extra: *mut *mut c_void,
	_source: pg_sys::GucSource::Type,
) -> bool {
	let text = unsafe { *value };
	if text.is_null() {
		return true;
	}
	let mut escontext: pg_sys::ErrorSaveContext = unsafe { std::mem::zeroed() };
	escontext.type_ = pg_sys::NodeTag::T_ErrorSaveContext;
	let mut result = pg_sys::Datum::from(0usize);
	let ok = unsafe {
		DirectInputFunctionCallSafe(
			Some(interval_in),
			text,
			pg_sys::InvalidOid,
			-1,
			(&raw mut escontext).cast(),
			&mut result,
		)
	};
	if !ok {
		unsafe { check_detail(c"The value must be an interval, such as '6 hours'.") };
		return false;
	}
	let interval = unsafe { &*(result.cast_mut_ptr::<pg_sys::Interval>()) };
	if interval.time < 0 || interval.day < 0 || interval.month < 0 {
		unsafe { check_detail(c"The interval must not be negative.") };
		return false;
	}
	true
}
