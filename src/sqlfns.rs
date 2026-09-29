//! The functions the SQL objects call into, under the names they are declared with (`as
//! 'MODULE_PATHNAME'` with no symbol means the function's own name). Those names are part of the
//! contract: a database created with an earlier install script calls them by name.
use crate::curl::*;
use crate::shared;
use pgrx::fcinfo::{pg_arg_is_null, pg_getarg, pg_return_null, pg_return_void};
use pgrx::{IntoDatum, pg_sys};
use std::ffi::{CString, c_int};

/// Version-1 calling convention declarations, one per function.
macro_rules! finfo {
	($($name:ident),*) => {$(
		#[unsafe(no_mangle)]
		pub extern "C" fn $name() -> &'static pg_sys::Pg_finfo_record {
			const V1: pg_sys::Pg_finfo_record = pg_sys::Pg_finfo_record { api_version: 1 };
			&V1
		}
	)*};
}

finfo!(
	pg_finfo_wake,
	pg_finfo_worker_restart,
	pg_finfo_wait_until_running,
	pg_finfo__urlencode_string,
	pg_finfo__encode_url_with_params_array
);

/// `net.wake()`: wake the worker when the current transaction commits.
#[unsafe(no_mangle)]
#[pgrx::pg_guard]
pub unsafe extern "C-unwind" fn wake(_fcinfo: pg_sys::FunctionCallInfo) -> pg_sys::Datum {
	shared::wake_at_commit();
	pg_return_void()
}

/// `net.worker_restart()`: re-read the configuration and start a new worker. Returns true, as the
/// reload it asks for is only a signal.
#[unsafe(no_mangle)]
#[pgrx::pg_guard]
pub unsafe extern "C-unwind" fn worker_restart(_fcinfo: pg_sys::FunctionCallInfo) -> pg_sys::Datum {
	unsafe { libc::kill(pg_sys::PostmasterPid, libc::SIGHUP) };
	shared::request_restart();
	true.into_datum().expect("a bool is never null")
}

/// `net.wait_until_running()`: return once the worker is serving requests.
#[unsafe(no_mangle)]
#[pgrx::pg_guard]
pub unsafe extern "C-unwind" fn wait_until_running(
	_fcinfo: pg_sys::FunctionCallInfo,
) -> pg_sys::Datum {
	while shared::status() != shared::RUNNING {
		unsafe {
			let events =
				(pg_sys::WL_LATCH_SET | pg_sys::WL_TIMEOUT | pg_sys::WL_EXIT_ON_PM_DEATH) as c_int;
			pg_sys::WaitLatch(pg_sys::MyLatch, events, 10, pg_sys::PG_WAIT_EXTENSION);
			pg_sys::ResetLatch(pg_sys::MyLatch);
		}
		pgrx::check_for_interrupts!();
	}
	pg_return_void()
}

/// `net._urlencode_string(varchar)`: every byte percent-encoded except letters, digits and `-._~`.
#[unsafe(no_mangle)]
#[pgrx::pg_guard]
pub unsafe extern "C-unwind" fn _urlencode_string(
	fcinfo: pg_sys::FunctionCallInfo,
) -> pg_sys::Datum {
	if unsafe { pg_arg_is_null(fcinfo, 0) } {
		return unsafe { pg_return_null(fcinfo) };
	}
	let s: String = unsafe { pg_getarg(fcinfo, 0) }.unwrap_or_default();
	encode_component(&s)
		.into_datum()
		.expect("text is never null")
}

pub fn encode_component(s: &str) -> String {
	let mut out = String::with_capacity(s.len());
	for &b in s.as_bytes() {
		if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
			out.push(b as char);
		} else {
			out.push_str(&format!("%{b:02X}"));
		}
	}
	out
}

/// `net._encode_url_with_params_array(url, params)`: the URL, parsed and put back together by
/// libcurl's URL parser (which is what makes a bad URL an error here, when it is queued, rather
/// than a failed request later), with each already-encoded `key=value` appended to its query.
#[unsafe(no_mangle)]
#[pgrx::pg_guard]
pub unsafe extern "C-unwind" fn _encode_url_with_params_array(
	fcinfo: pg_sys::FunctionCallInfo,
) -> pg_sys::Datum {
	if unsafe { pg_arg_is_null(fcinfo, 0) || pg_arg_is_null(fcinfo, 1) } {
		return unsafe { pg_return_null(fcinfo) };
	}
	let url: String = unsafe { pg_getarg(fcinfo, 0) }.unwrap_or_default();
	let params: Vec<Option<String>> = unsafe { pg_getarg(fcinfo, 1) }.unwrap_or_default();
	match build_url(&url, params.iter().flatten()) {
		Ok(full) => full.into_datum().expect("text is never null"),
		Err(message) => pgrx::error!("{message}"),
	}
}

fn build_url<'a>(url: &str, params: impl Iterator<Item = &'a String>) -> Result<String, String> {
	let c_url =
		CString::new(url).map_err(|_| format!("invalid URL \"{url}\": it contains a NUL byte"))?;
	unsafe {
		let h = curl_url();
		if h.is_null() {
			return Err("out of memory".into());
		}
		let result = (|| {
			let rc = curl_url_set(h, CURLUPART_URL, c_url.as_ptr(), 0);
			if rc != 0 {
				return Err(format!(
					"invalid URL \"{url}\": {}",
					string(curl_url_strerror(rc))
				));
			}
			for p in params {
				let c_p = CString::new(p.as_str()).map_err(|_| {
					format!("invalid URL \"{url}\": a parameter contains a NUL byte")
				})?;
				let rc = curl_url_set(h, CURLUPART_QUERY, c_p.as_ptr(), CURLU_APPENDQUERY);
				if rc != 0 {
					return Err(format!(
						"invalid URL \"{p}\": {}",
						string(curl_url_strerror(rc))
					));
				}
			}
			let mut out = std::ptr::null_mut();
			let rc = curl_url_get(h, CURLUPART_URL, &mut out, 0);
			if rc != 0 {
				return Err(format!(
					"failed to encode URL \"{url}\": {}",
					string(curl_url_strerror(rc))
				));
			}
			let full = string(out);
			curl_free(out.cast());
			Ok(full)
		})();
		curl_url_cleanup(h);
		result
	}
}
