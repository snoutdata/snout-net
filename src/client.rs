//! The HTTP client: one thread that owns a libcurl multi handle and runs every request in flight
//! at once. It never calls into Postgres (Postgres is single-threaded, and only the worker's main
//! thread may touch it); requests arrive on a channel, responses leave on another, and the main
//! thread is told a response is waiting through an eventfd it can wait on beside its latch.
use crate::curl::*;
use crate::egress::Policy;
use crate::request::{self, Method, Timings};
use std::collections::HashMap;
use std::ffi::{CString, c_char, c_int, c_long, c_void};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};

/// A request as read from the queue, already checked.
pub struct Request {
	pub id: i64,
	pub method: Method,
	pub url: String,
	/// `name: value` lines, the client's own `User-Agent` appended.
	pub headers: Vec<String>,
	pub body: Option<Vec<u8>>,
	pub timeout_ms: i32,
}

pub enum Outcome {
	Response {
		status: i32,
		headers: Vec<(String, String)>,
		content_type: Option<String>,
		body: Vec<u8>,
	},
	Failed {
		timed_out: bool,
		message: String,
	},
}

pub struct Response {
	pub id: i64,
	pub outcome: Outcome,
}

/// What each transfer is allowed to reach and how much it may read, fixed when it is added.
#[derive(Clone)]
pub struct Limits {
	pub policy: Policy,
	pub max_response_bytes: usize,
}

/// The main thread's end: send requests, collect responses.
pub struct Client {
	requests: Sender<(Request, Limits)>,
	responses: Receiver<Response>,
	waker: Arc<Waker>,
	notify: c_int,
}

struct Waker {
	multi: *mut CURLM,
	stopped: AtomicBool,
}

// SAFETY: the only call made through `multi` from another thread is curl_multi_wakeup, which libcurl
// documents as safe to call from any thread while the handle exists; the client thread owns the
// handle and never frees it (the process ends with it).
unsafe impl Send for Waker {}
unsafe impl Sync for Waker {}

impl Client {
	/// Starts the client thread. `notify` is an eventfd the thread writes to when responses wait.
	pub fn start() -> Client {
		unsafe { curl_global_init(CURL_GLOBAL_ALL) };
		let multi = unsafe { curl_multi_init() };
		assert!(!multi.is_null(), "curl_multi_init failed");
		let notify = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
		assert!(notify >= 0, "eventfd failed");
		let waker = Arc::new(Waker {
			multi,
			stopped: AtomicBool::new(false),
		});
		let (requests, incoming) = channel();
		let (outgoing, responses) = channel();
		let thread_waker = Arc::clone(&waker);
		std::thread::Builder::new()
			.name("snout_net http".into())
			.spawn(move || run(thread_waker, incoming, outgoing, notify))
			.expect("could not start the HTTP thread");
		Client {
			requests,
			responses,
			waker,
			notify,
		}
	}

	pub fn notify_fd(&self) -> c_int {
		self.notify
	}

	pub fn send(&self, request: Request, limits: Limits) {
		if self.requests.send((request, limits)).is_ok() {
			unsafe { curl_multi_wakeup(self.waker.multi) };
		}
	}

	/// Every response that has arrived, without waiting.
	pub fn collect(&self) -> Vec<Response> {
		let mut counter = 0u64;
		unsafe { libc::read(self.notify, (&raw mut counter).cast(), 8) };
		let mut out = Vec::new();
		while let Ok(r) = self.responses.try_recv() {
			out.push(r);
		}
		out
	}
}

impl Drop for Client {
	fn drop(&mut self) {
		self.waker.stopped.store(true, Ordering::Release);
		unsafe { curl_multi_wakeup(self.waker.multi) };
	}
}

/// One request on the wire. Boxed, so the pointers libcurl holds to it stay put.
struct Transfer {
	id: i64,
	easy: *mut CURL,
	headers: *mut c_void,
	_url: CString,
	_method: Option<CString>,
	_body: Option<Vec<u8>>,
	timeout_ms: i32,
	received: Vec<u8>,
	max_response_bytes: usize,
	too_large: bool,
	policy: Policy,
	refused: Option<(IpAddr, &'static str)>,
	/// Whether any connection was allowed, so a refusal is named only when it is why nothing
	/// connected (a host with one refused address and one that is down is "couldn't connect").
	opened: bool,
}

fn run(
	waker: Arc<Waker>,
	incoming: Receiver<(Request, Limits)>,
	outgoing: Sender<Response>,
	notify: c_int,
) {
	let multi = waker.multi;
	let mut transfers: HashMap<usize, Box<Transfer>> = HashMap::new();
	loop {
		loop {
			match incoming.try_recv() {
				Ok((request, limits)) => match start(multi, request, limits) {
					Ok(t) => {
						transfers.insert(t.easy as usize, t);
					}
					Err(response) => {
						let _ = outgoing.send(response);
						signal(notify);
					}
				},
				Err(TryRecvError::Empty) => break,
				Err(TryRecvError::Disconnected) => return,
			}
		}
		let mut running: c_int = 0;
		unsafe { curl_multi_perform(multi, &mut running) };
		let mut done = false;
		loop {
			let mut left: c_int = 0;
			let msg = unsafe { curl_multi_info_read(multi, &mut left) };
			if msg.is_null() {
				break;
			}
			let msg = unsafe { &*msg };
			if msg.msg != CURLMSG_DONE {
				continue;
			}
			let (easy, code) = (msg.easy_handle, msg.result());
			if let Some(mut t) = transfers.remove(&(easy as usize)) {
				let response = finish(&mut t, code);
				unsafe {
					curl_multi_remove_handle(multi, easy);
					curl_easy_cleanup(easy);
					curl_slist_free_all(t.headers);
				}
				let _ = outgoing.send(response);
				done = true;
			}
		}
		if done {
			signal(notify);
		}
		if waker.stopped.load(Ordering::Acquire) {
			return;
		}
		// Sleeps until a socket is ready, a timer is due, or the main thread wakes it.
		unsafe { curl_multi_poll(multi, std::ptr::null_mut(), 0, 1000, std::ptr::null_mut()) };
	}
}

fn signal(fd: c_int) {
	let one: u64 = 1;
	unsafe { libc::write(fd, (&raw const one).cast(), 8) };
}

fn failed(id: i64, message: String) -> Response {
	Response {
		id,
		outcome: Outcome::Failed {
			timed_out: false,
			message,
		},
	}
}

fn start(multi: *mut CURLM, request: Request, limits: Limits) -> Result<Box<Transfer>, Response> {
	let id = request.id;
	let Ok(url) = CString::new(request.url) else {
		return Err(failed(id, "the URL contains a NUL byte".into()));
	};
	let easy = unsafe { curl_easy_init() };
	if easy.is_null() {
		return Err(failed(
			id,
			"could not start a request (curl_easy_init)".into(),
		));
	}
	let mut headers: *mut c_void = std::ptr::null_mut();
	for line in &request.headers {
		// Checked for NUL already (request::check_header); a line that still has one is skipped.
		if let Ok(line) = CString::new(line.as_str()) {
			headers = unsafe { curl_slist_append(headers, line.as_ptr()) };
		}
	}
	let method = match request.method {
		Method::Get if request.body.is_some() => Some(c"GET".to_owned()),
		Method::Delete => Some(c"DELETE".to_owned()),
		_ => None,
	};
	let mut t = Box::new(Transfer {
		id,
		easy,
		headers,
		_url: url,
		_method: method,
		_body: request.body,
		timeout_ms: request.timeout_ms,
		received: Vec::new(),
		max_response_bytes: limits.max_response_bytes,
		too_large: false,
		policy: limits.policy,
		refused: None,
		opened: false,
	});
	let this: *mut Transfer = &mut *t;
	unsafe {
		curl_easy_setopt(easy, CURLOPT_NOSIGNAL, 1 as c_long);
		curl_easy_setopt(easy, CURLOPT_URL, t._url.as_ptr());
		if let Some(m) = &t._method {
			curl_easy_setopt(easy, CURLOPT_CUSTOMREQUEST, m.as_ptr());
		}
		match (&t._body, request.method) {
			(Some(body), _) => {
				curl_easy_setopt(easy, CURLOPT_POSTFIELDSIZE_LARGE, body.len() as i64);
				curl_easy_setopt(easy, CURLOPT_POSTFIELDS, body.as_ptr());
			}
			(None, Method::Post) => {
				curl_easy_setopt(easy, CURLOPT_POST, 1 as c_long);
				curl_easy_setopt(easy, CURLOPT_POSTFIELDSIZE, 0 as c_long);
			}
			(None, _) => {}
		}
		curl_easy_setopt(easy, CURLOPT_WRITEFUNCTION, write_cb as WriteCallback);
		curl_easy_setopt(easy, CURLOPT_WRITEDATA, this);
		curl_easy_setopt(easy, CURLOPT_HEADER, 0 as c_long);
		curl_easy_setopt(easy, CURLOPT_HTTPHEADER, t.headers);
		curl_easy_setopt(easy, CURLOPT_TIMEOUT_MS, c_long::from(t.timeout_ms));
		curl_easy_setopt(easy, CURLOPT_PRIVATE, this);
		curl_easy_setopt(easy, CURLOPT_FOLLOWLOCATION, 1 as c_long);
		curl_easy_setopt(easy, CURLOPT_MAXREDIRS, 30 as c_long);
		curl_easy_setopt(easy, CURLOPT_PROTOCOLS_STR, c"http,https".as_ptr());
		curl_easy_setopt(easy, CURLOPT_REDIR_PROTOCOLS_STR, c"http,https".as_ptr());
		// No proxy, whatever the environment says: a proxy would be the only address checked.
		curl_easy_setopt(easy, CURLOPT_PROXY, c"".as_ptr());
		curl_easy_setopt(easy, CURLOPT_NOPROXY, c"*".as_ptr());
		curl_easy_setopt(
			easy,
			CURLOPT_OPENSOCKETFUNCTION,
			open_socket as OpenSocketCallback,
		);
		curl_easy_setopt(easy, CURLOPT_OPENSOCKETDATA, this);
		let code = curl_multi_add_handle(multi, easy);
		if code != CURLM_OK {
			curl_easy_cleanup(easy);
			curl_slist_free_all(t.headers);
			return Err(failed(
				id,
				format!(
					"could not start a request: {}",
					string(curl_multi_strerror(code))
				),
			));
		}
	}
	Ok(t)
}

unsafe extern "C" fn write_cb(
	data: *mut c_char,
	size: usize,
	n: usize,
	userdata: *mut c_void,
) -> usize {
	let t = unsafe { &mut *(userdata as *mut Transfer) };
	let len = size * n;
	if t.received.len() + len > t.max_response_bytes {
		t.too_large = true;
		// Anything other than `len` makes libcurl stop with CURLE_WRITE_ERROR.
		return 0;
	}
	t.received
		.extend_from_slice(unsafe { std::slice::from_raw_parts(data as *const u8, len) });
	len
}

/// Called for every connection libcurl is about to make, with the address it resolved: the one
/// place a request cannot get past, whatever its URL, its DNS answer or its redirects said.
unsafe extern "C" fn open_socket(
	clientp: *mut c_void,
	purpose: c_int,
	address: *mut curl_sockaddr,
) -> curl_socket_t {
	let t = unsafe { &mut *(clientp as *mut Transfer) };
	let a = unsafe { &*address };
	if purpose != CURLSOCKTYPE_IPCXN {
		return CURL_SOCKET_BAD;
	}
	let ip = match a.family {
		libc::AF_INET => {
			let sin = unsafe { &*(&raw const a.addr as *const libc::sockaddr_in) };
			IpAddr::V4(Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr)))
		}
		libc::AF_INET6 => {
			let sin6 = unsafe { &*(&raw const a.addr as *const libc::sockaddr_in6) };
			IpAddr::V6(Ipv6Addr::from(sin6.sin6_addr.s6_addr))
		}
		_ => return CURL_SOCKET_BAD,
	};
	if let Some(why) = t.policy.refuses(ip) {
		t.refused = Some((ip, why));
		return CURL_SOCKET_BAD;
	}
	t.opened = true;
	unsafe { libc::socket(a.family, a.socktype | libc::SOCK_CLOEXEC, a.protocol) }
}

fn finish(t: &mut Transfer, code: CURLcode) -> Response {
	let easy = t.easy;
	let id = t.id;
	if code == CURLE_OK {
		let mut status: c_long = 0;
		unsafe { curl_easy_getinfo(easy, CURLINFO_RESPONSE_CODE, &mut status as *mut c_long) };
		// The final response's headers: request index -1 is the last one after any redirects.
		let mut headers = Vec::new();
		let mut content_type = None;
		let mut prev: *mut curl_header = std::ptr::null_mut();
		loop {
			prev = unsafe { curl_easy_nextheader(easy, CURLH_HEADER, -1, prev) };
			if prev.is_null() {
				break;
			}
			let h = unsafe { &*prev };
			let (name, value) = unsafe { (string(h.name), string(h.value)) };
			if content_type.is_none() && name.eq_ignore_ascii_case("content-type") {
				content_type = Some(value.clone());
			}
			headers.push((name, value));
		}
		return Response {
			id,
			outcome: Outcome::Response {
				status: status as i32,
				headers,
				content_type,
				body: std::mem::take(&mut t.received),
			},
		};
	}
	let timed_out = code == CURLE_OPERATION_TIMEDOUT;
	let message = if timed_out {
		let timings = unsafe {
			Timings {
				namelookup: info_double(easy, CURLINFO_NAMELOOKUP_TIME),
				connect: info_double(easy, CURLINFO_CONNECT_TIME),
				appconnect: info_double(easy, CURLINFO_APPCONNECT_TIME),
				pretransfer: info_double(easy, CURLINFO_PRETRANSFER_TIME),
				total: info_double(easy, CURLINFO_TOTAL_TIME),
			}
		};
		request::timeout_message(t.timeout_ms, timings)
	} else if let (CURLE_COULDNT_CONNECT, Some((ip, why)), false) = (code, t.refused, t.opened) {
		format!("Refused to connect to {ip}: it is {why} (snout_net.allowed_networks)")
	} else if code == CURLE_WRITE_ERROR && t.too_large {
		format!(
			"Response body is larger than {} bytes (snout_net.max_response_bytes)",
			t.max_response_bytes
		)
	} else {
		unsafe { string(curl_easy_strerror(code)) }
	};
	Response {
		id,
		outcome: Outcome::Failed { timed_out, message },
	}
}
