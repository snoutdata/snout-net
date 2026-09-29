//! The part of libcurl's C interface this library calls, declared here rather than through a
//! bindings crate so that exactly what is used is on one page. The numbers are libcurl's ABI
//! (`curl/curl.h`, `curl/urlapi.h`, `curl/header.h`), stable since the versions noted, and the
//! library is the system's, the one the database server's distribution ships.
#![allow(non_camel_case_types, clippy::upper_case_acronyms)]

use std::ffi::{c_char, c_double, c_int, c_long, c_uint, c_void};

pub type CURL = c_void;
pub type CURLM = c_void;
pub type CURLU = c_void;
pub type CURLcode = c_int;
pub type CURLMcode = c_int;
pub type CURLUcode = c_int;
pub type curl_socket_t = c_int;

pub const CURL_GLOBAL_ALL: c_long = 3;
pub const CURLE_OK: CURLcode = 0;
pub const CURLE_COULDNT_CONNECT: CURLcode = 7;
pub const CURLE_WRITE_ERROR: CURLcode = 23;
pub const CURLE_OPERATION_TIMEDOUT: CURLcode = 28;
pub const CURLM_OK: CURLMcode = 0;
pub const CURLMSG_DONE: c_int = 1;
pub const CURL_SOCKET_BAD: curl_socket_t = -1;

const LONG: c_int = 0;
const OBJECTPOINT: c_int = 10_000;
const FUNCTIONPOINT: c_int = 20_000;
const OFF_T: c_int = 30_000;

pub const CURLOPT_WRITEDATA: c_int = OBJECTPOINT + 1;
pub const CURLOPT_URL: c_int = OBJECTPOINT + 2;
pub const CURLOPT_PROXY: c_int = OBJECTPOINT + 4;
pub const CURLOPT_WRITEFUNCTION: c_int = FUNCTIONPOINT + 11;
pub const CURLOPT_POSTFIELDS: c_int = OBJECTPOINT + 15;
pub const CURLOPT_HTTPHEADER: c_int = OBJECTPOINT + 23;
pub const CURLOPT_CUSTOMREQUEST: c_int = OBJECTPOINT + 36;
pub const CURLOPT_HEADER: c_int = LONG + 42;
pub const CURLOPT_POST: c_int = LONG + 47;
pub const CURLOPT_FOLLOWLOCATION: c_int = LONG + 52;
pub const CURLOPT_POSTFIELDSIZE: c_int = LONG + 60;
pub const CURLOPT_MAXREDIRS: c_int = LONG + 68;
pub const CURLOPT_NOSIGNAL: c_int = LONG + 99;
pub const CURLOPT_PRIVATE: c_int = OBJECTPOINT + 103;
pub const CURLOPT_TIMEOUT_MS: c_int = LONG + 155;
pub const CURLOPT_OPENSOCKETFUNCTION: c_int = FUNCTIONPOINT + 163;
pub const CURLOPT_OPENSOCKETDATA: c_int = OBJECTPOINT + 164;
pub const CURLOPT_NOPROXY: c_int = OBJECTPOINT + 177;
pub const CURLOPT_POSTFIELDSIZE_LARGE: c_int = OFF_T + 120;
/// libcurl 7.85.
pub const CURLOPT_PROTOCOLS_STR: c_int = OBJECTPOINT + 318;
pub const CURLOPT_REDIR_PROTOCOLS_STR: c_int = OBJECTPOINT + 319;

const INFO_LONG: c_int = 0x20_0000;
const INFO_DOUBLE: c_int = 0x30_0000;
pub const CURLINFO_RESPONSE_CODE: c_int = INFO_LONG + 2;
pub const CURLINFO_TOTAL_TIME: c_int = INFO_DOUBLE + 3;
pub const CURLINFO_NAMELOOKUP_TIME: c_int = INFO_DOUBLE + 4;
pub const CURLINFO_CONNECT_TIME: c_int = INFO_DOUBLE + 5;
pub const CURLINFO_PRETRANSFER_TIME: c_int = INFO_DOUBLE + 6;
pub const CURLINFO_APPCONNECT_TIME: c_int = INFO_DOUBLE + 33;

pub const CURLUPART_URL: c_int = 0;
pub const CURLUPART_QUERY: c_int = 8;
pub const CURLU_APPENDQUERY: c_uint = 1 << 8;

/// `curl_easy_nextheader`'s origin for ordinary response headers (libcurl 7.83).
pub const CURLH_HEADER: c_uint = 1;

/// `CURLSOCKTYPE_IPCXN`: a socket for a connection, as opposed to one accepted.
pub const CURLSOCKTYPE_IPCXN: c_int = 0;

#[repr(C)]
pub struct CURLMsg {
	pub msg: c_int,
	pub easy_handle: *mut CURL,
	/// A union of a pointer and a `CURLcode`; for `CURLMSG_DONE` the code is its first bytes.
	pub data: *mut c_void,
}

impl CURLMsg {
	pub fn result(&self) -> CURLcode {
		// SAFETY: for CURLMSG_DONE libcurl stores the CURLcode in the union, whose storage is at
		// least a pointer wide; reading its first c_int is reading that member.
		unsafe { *(&raw const self.data as *const CURLcode) }
	}
}

#[repr(C)]
pub struct curl_header {
	pub name: *mut c_char,
	pub value: *mut c_char,
	pub amount: usize,
	pub index: usize,
	pub origin: c_uint,
	pub anchor: *mut c_void,
}

#[repr(C)]
pub struct curl_sockaddr {
	pub family: c_int,
	pub socktype: c_int,
	pub protocol: c_int,
	pub addrlen: c_uint,
	/// The address itself, `addrlen` bytes; declared as a `struct sockaddr` but as long as the
	/// family needs.
	pub addr: libc::sockaddr,
}

pub type WriteCallback = unsafe extern "C" fn(*mut c_char, usize, usize, *mut c_void) -> usize;
pub type OpenSocketCallback =
	unsafe extern "C" fn(*mut c_void, c_int, *mut curl_sockaddr) -> curl_socket_t;

#[link(name = "curl")]
unsafe extern "C" {
	pub fn curl_global_init(flags: c_long) -> CURLcode;
	pub fn curl_version() -> *const c_char;
	pub fn curl_free(p: *mut c_void);
	pub fn curl_easy_init() -> *mut CURL;
	pub fn curl_easy_cleanup(h: *mut CURL);
	pub fn curl_easy_setopt(h: *mut CURL, option: c_int, ...) -> CURLcode;
	pub fn curl_easy_getinfo(h: *mut CURL, info: c_int, ...) -> CURLcode;
	pub fn curl_easy_strerror(code: CURLcode) -> *const c_char;
	pub fn curl_easy_nextheader(
		h: *mut CURL,
		origin: c_uint,
		request: c_int,
		prev: *mut curl_header,
	) -> *mut curl_header;
	pub fn curl_slist_append(list: *mut c_void, s: *const c_char) -> *mut c_void;
	pub fn curl_slist_free_all(list: *mut c_void);
	pub fn curl_multi_init() -> *mut CURLM;
	pub fn curl_multi_add_handle(m: *mut CURLM, h: *mut CURL) -> CURLMcode;
	pub fn curl_multi_remove_handle(m: *mut CURLM, h: *mut CURL) -> CURLMcode;
	pub fn curl_multi_perform(m: *mut CURLM, running: *mut c_int) -> CURLMcode;
	pub fn curl_multi_info_read(m: *mut CURLM, left: *mut c_int) -> *mut CURLMsg;
	/// libcurl 7.66.
	pub fn curl_multi_poll(
		m: *mut CURLM,
		extra: *mut c_void,
		extra_nfds: c_uint,
		timeout_ms: c_int,
		numfds: *mut c_int,
	) -> CURLMcode;
	/// libcurl 7.68. The one call that may be made from another thread.
	pub fn curl_multi_wakeup(m: *mut CURLM) -> CURLMcode;
	pub fn curl_multi_strerror(code: CURLMcode) -> *const c_char;
	pub fn curl_url() -> *mut CURLU;
	pub fn curl_url_cleanup(u: *mut CURLU);
	pub fn curl_url_set(
		u: *mut CURLU,
		part: c_int,
		content: *const c_char,
		flags: c_uint,
	) -> CURLUcode;
	pub fn curl_url_get(
		u: *mut CURLU,
		part: c_int,
		content: *mut *mut c_char,
		flags: c_uint,
	) -> CURLUcode;
	/// libcurl 7.80.
	pub fn curl_url_strerror(code: CURLUcode) -> *const c_char;
}

/// `curl_easy_getinfo` for a double.
pub unsafe fn info_double(h: *mut CURL, info: c_int) -> f64 {
	let mut v: c_double = 0.0;
	unsafe { curl_easy_getinfo(h, info, &mut v as *mut c_double) };
	v
}

/// A C string libcurl owns, copied.
pub unsafe fn string(p: *const c_char) -> String {
	if p.is_null() {
		String::new()
	} else {
		unsafe { std::ffi::CStr::from_ptr(p) }
			.to_string_lossy()
			.into_owned()
	}
}
