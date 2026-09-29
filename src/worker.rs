//! The background worker: reads the request queue, hands requests to the HTTP thread, and writes
//! each response the moment it arrives.
//!
//! The shape that matters, and the reason this is not a batch loop: no transaction is open while a
//! request is on the network. A request is read from the queue WITHOUT being deleted, remembered as
//! in flight, and its queue row is deleted in the same short transaction that writes its response.
//! So a slow request delays nobody else's, a response is visible as soon as it is written, the
//! database's xmin horizon is never held by a request, and a worker that dies re-sends what it had
//! in flight when it restarts (its queue rows are still there), as a batch rolled back would.
//!
//! The queue has no index (its definition is the contract), so the worker never looks a row up by
//! id: it remembers each row's physical address (ctid) when it reads it and deletes by that, checking
//! the id too, and reads ahead in batches so the queue is scanned once per batch, not per request.
use crate::client::{Client, Limits, Outcome, Request, Response};
use crate::request::{self, Method};
use crate::{settings, shared};
use pgrx::bgworkers::{BackgroundWorker, SignalWakeFlags};
use pgrx::{FromDatum, IntoDatum, PgTryBuilder, pg_sys};
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{CStr, CString, c_char, c_int, c_long};
use std::panic::AssertUnwindSafe;
use std::time::{Duration, Instant};

const USER_AGENT: &str = concat!("User-Agent: snout_net/", env!("CARGO_PKG_VERSION"));
/// How many expired responses one statement deletes, and how many statements one pass runs.
const EXPIRE_BATCH: i32 = 1000;
const EXPIRE_ROUNDS: usize = 10;
/// How many queued requests one read takes, beyond those that go straight out.
const READ_AHEAD: usize = 1000;

#[pgrx::pg_guard]
#[unsafe(no_mangle)]
pub extern "C-unwind" fn snout_net_worker(_arg: pg_sys::Datum) {
	BackgroundWorker::attach_signal_handlers(SignalWakeFlags::SIGHUP | SignalWakeFlags::SIGTERM);
	let database = settings::database_name();
	let user = settings::username();
	BackgroundWorker::connect_worker_to_spi(Some(&database), user.as_deref());
	let client = Client::start();
	let curl = unsafe { crate::curl::string(crate::curl::curl_version()) };
	let appname = CString::new(format!("snout_net {}", env!("CARGO_PKG_VERSION"))).expect("no NUL");
	unsafe {
		pg_sys::pgstat_report_appname(appname.as_ptr());
		shared::publish_running();
	}
	pgrx::log!(
		"snout_net {} worker started on database \"{database}\" ({curl}), up to {} requests at once",
		env!("CARGO_PKG_VERSION"),
		settings::max_concurrent()
	);
	let worker = Worker {
		client,
		in_flight: HashMap::new(),
		pending: VecDeque::new(),
		backlog: Vec::new(),
		more: true,
		expire_at: Some(Instant::now()),
		retry_at: None,
		plans: Plans::default(),
	};
	worker.run();
}

#[derive(Default)]
struct Plans {
	fetch: Option<pg_sys::SPIPlanPtr>,
	respond: Option<pg_sys::SPIPlanPtr>,
	forget: Option<pg_sys::SPIPlanPtr>,
	forget_by_id: Option<pg_sys::SPIPlanPtr>,
	expire: Option<pg_sys::SPIPlanPtr>,
	next_expiry: Option<pg_sys::SPIPlanPtr>,
}

/// Where a request came from: the queue table (so a response for a queue that has since been
/// dropped, and perhaps created again with its ids starting over, is recognised) and its row.
#[derive(Clone)]
struct Row {
	queue: pg_sys::Oid,
	ctid: String,
}

struct Worker {
	client: Client,
	/// Requests on the network.
	in_flight: HashMap<i64, Row>,
	/// Requests read and not yet sent, oldest first.
	pending: VecDeque<(Request, Row)>,
	/// Responses that could not be written yet because the tables were locked.
	backlog: Vec<Response>,
	/// The last read was full, so there may be more queued.
	more: bool,
	expire_at: Option<Instant>,
	/// The tables were held by someone else; try again then.
	retry_at: Option<Instant>,
	plans: Plans,
}

enum Opened {
	/// The queue table's OID.
	Ready(pg_sys::Oid),
	/// No `net` schema or no tables: nothing is installed in this database.
	Missing,
	/// Held exclusively by another statement (a DROP EXTENSION, a TRUNCATE) for now.
	Busy,
}

impl Worker {
	fn run(mut self) -> ! {
		loop {
			if BackgroundWorker::sigterm_received() || shared::take_restart() {
				// A failure exit, so Postgres starts a new worker (after a second), which is what a
				// restart request asks for; at shutdown nothing is restarted anyway.
				unsafe { pg_sys::proc_exit(1) };
			}
			if BackgroundWorker::sighup_received() {
				unsafe { pg_sys::ProcessConfigFile(pg_sys::GucContext::PGC_SIGHUP) };
				// The TTL may have changed.
				self.expire_at = Some(Instant::now());
			}
			if self.retry_at.is_some_and(|t| t <= Instant::now()) {
				self.retry_at = None;
				self.more = true;
			}

			let mut responses = std::mem::take(&mut self.backlog);
			responses.extend(self.client.collect());
			if !responses.is_empty() {
				self.write(responses);
			}

			self.dispatch();
			// A commit that queued a request since the last read. Taken on every pass, before the
			// read's snapshot, and remembered in `more`, so a wake that arrives while there is no
			// room (or while there is nothing installed yet) is kept rather than left in the flag.
			if shared::take_wake() {
				self.more = true;
			}
			let free = settings::max_concurrent().saturating_sub(self.in_flight.len());
			if free > 0 && self.pending.is_empty() && self.retry_at.is_none() && self.more {
				self.more = self.fetch(free + READ_AHEAD);
				self.dispatch();
				if self.more && self.in_flight.len() < settings::max_concurrent() {
					continue;
				}
			}

			if self.expire_at.is_some_and(|t| t <= Instant::now()) {
				self.expire();
			}

			self.wait();
		}
	}

	fn wait(&self) {
		let due = [self.expire_at, self.retry_at].into_iter().flatten().min();
		let timeout: c_long = match due {
			Some(t) => t
				.saturating_duration_since(Instant::now())
				.as_millis()
				.min(i32::MAX as u128) as c_long,
			None => -1,
		};
		let mut events =
			pg_sys::WL_LATCH_SET | pg_sys::WL_SOCKET_READABLE | pg_sys::WL_EXIT_ON_PM_DEATH;
		if timeout >= 0 {
			events |= pg_sys::WL_TIMEOUT;
		}
		unsafe {
			pg_sys::pgstat_report_activity(pg_sys::BackendState::STATE_IDLE, std::ptr::null());
			pg_sys::WaitLatchOrSocket(
				pg_sys::MyLatch,
				events as c_int,
				self.client.notify_fd(),
				timeout,
				pg_sys::PG_WAIT_EXTENSION,
			);
			pg_sys::ResetLatch(pg_sys::MyLatch);
			pg_sys::pgstat_report_activity(pg_sys::BackendState::STATE_RUNNING, std::ptr::null());
		}
		pgrx::check_for_interrupts!();
	}

	/// Sends read requests while there is room on the network.
	fn dispatch(&mut self) {
		let max = settings::max_concurrent();
		if self.pending.is_empty() || self.in_flight.len() >= max {
			return;
		}
		let limits = Limits {
			policy: settings::policy(),
			max_response_bytes: settings::max_response_bytes(),
		};
		while self.in_flight.len() < max {
			let Some((request, row)) = self.pending.pop_front() else {
				break;
			};
			self.in_flight.insert(request.id, row);
			self.client.send(request, limits.clone());
		}
	}

	/// Reads up to `limit` queued requests that are neither in flight nor already read, oldest
	/// first; a request that may not be sent gets its refusal written straight away. Returns
	/// whether the read was full (so more may be waiting).
	fn fetch(&mut self, limit: usize) -> bool {
		let mut read: Vec<(Request, Row)> = Vec::new();
		let mut full = false;
		let in_flight = &self.in_flight;
		let pending = &self.pending;
		let plans = &mut self.plans;
		let outcome = transaction(|| {
			let queue = match open_tables() {
				Opened::Ready(queue) => queue,
				other => return other,
			};
			let exclude: Vec<i64> = in_flight
				.iter()
				.filter(|(_, row)| row.queue == queue)
				.map(|(id, _)| *id)
				.chain(pending.iter().map(|(r, _)| r.id))
				.collect();
			let plan = prepare(
				&mut plans.fetch,
				c"select q.id, q.method::text, q.url, q.timeout_milliseconds, \
				  array(select key || ': ' || value from jsonb_each_text(q.headers)), q.body, q.ctid::text \
				  from net.http_request_queue q where not (q.id = any($1)) order by q.id limit $2",
				&mut [pg_sys::INT8ARRAYOID, pg_sys::INT8OID],
			);
			let rows = read_queue(
				plan,
				&mut [exclude.into_datum(), (limit as i64).into_datum()],
			);
			full = rows.len() == limit;
			let mut refused = Vec::new();
			for (row, ctid) in rows {
				match row {
					Ok(request) => read.push((request, Row { queue, ctid })),
					Err(response) => refused.push((response, ctid)),
				}
			}
			if !refused.is_empty() {
				let rows: Vec<(i64, String)> = refused
					.iter()
					.map(|(r, ctid)| (r.id, ctid.clone()))
					.collect();
				forget(plans, &rows);
				let respond = prepare_respond(&mut plans.respond);
				for (response, _) in &refused {
					respond_now(respond, response);
				}
			}
			Opened::Ready(queue)
		});
		match outcome {
			Some(Opened::Ready(_)) => {}
			Some(Opened::Missing) => {
				self.in_flight.clear();
				self.pending.clear();
				return false;
			}
			Some(Opened::Busy) | None => {
				self.retry_at = Some(Instant::now() + Duration::from_secs(1));
				return false;
			}
		}
		self.pending.extend(read);
		full
	}

	/// Writes responses and deletes their queue rows, in one transaction. If that fails (a
	/// customer's trigger or constraint on the response table raised), one transaction each, so
	/// one response that cannot be written costs only itself, and its request is not sent again.
	fn write(&mut self, responses: Vec<Response>) {
		// A response to a request this worker no longer tracks (its queue was dropped) is dropped.
		let responses: Vec<Response> = responses
			.into_iter()
			.filter(|r| self.in_flight.contains_key(&r.id))
			.collect();
		if responses.is_empty() {
			return;
		}
		let in_flight = &self.in_flight;
		let plans = &mut self.plans;
		let write_some = |plans: &mut Plans, which: &[&Response]| {
			let queue = match open_tables() {
				Opened::Ready(queue) => queue,
				other => return other,
			};
			let mine: Vec<&Response> = which
				.iter()
				.copied()
				.filter(|r| in_flight.get(&r.id).is_some_and(|row| row.queue == queue))
				.collect();
			let rows: Vec<(i64, String)> = mine
				.iter()
				.map(|r| (r.id, in_flight[&r.id].ctid.clone()))
				.collect();
			forget(plans, &rows);
			let respond = prepare_respond(&mut plans.respond);
			for r in mine {
				respond_now(respond, r);
			}
			Opened::Ready(queue)
		};
		let all: Vec<&Response> = responses.iter().collect();
		match transaction(|| write_some(plans, &all)) {
			Some(Opened::Ready(_)) | Some(Opened::Missing) => {}
			Some(Opened::Busy) => {
				self.retry_at = Some(Instant::now() + Duration::from_secs(1));
				self.backlog = responses;
				return;
			}
			None => {
				for r in &responses {
					if transaction(|| write_some(plans, &[r])).is_none() {
						pgrx::warning!(
							"snout_net: the response to request {} could not be written, and the request is not sent again",
							r.id
						);
						let row = (r.id, in_flight[&r.id].ctid.clone());
						transaction(|| {
							if let Opened::Ready(_) = open_tables() {
								forget(plans, std::slice::from_ref(&row));
							}
							Opened::Missing
						});
					}
				}
			}
		}
		for r in &responses {
			self.in_flight.remove(&r.id);
		}
		if self.expire_at.is_none() {
			self.expire_at = Some(Instant::now() + Duration::from_secs(1));
		}
	}

	/// Deletes responses older than the TTL, oldest first and a bounded number at a time, then
	/// works out when the next one is due. No statement runs while nothing is due.
	fn expire(&mut self) {
		let plans = &mut self.plans;
		let mut next: Option<f64> = None;
		let done = transaction(|| {
			let queue = match open_tables() {
				Opened::Ready(queue) => queue,
				other => return other,
			};
			let ttl = settings::ttl();
			let expire = prepare(
				&mut plans.expire,
				c"delete from only net._http_response where ctid = any(array( \
				  select ctid from only net._http_response where created < now() - $1::interval order by created limit $2))",
				&mut [pg_sys::TEXTOID, pg_sys::INT4OID],
			);
			for _ in 0..EXPIRE_ROUNDS {
				if execute(
					expire,
					&mut [ttl.as_str().into_datum(), EXPIRE_BATCH.into_datum()],
				) < EXPIRE_BATCH as u64
				{
					break;
				}
			}
			let next_plan = prepare(
				&mut plans.next_expiry,
				c"select extract(epoch from (min(created) + $1::interval - now()))::float8 from only net._http_response",
				&mut [pg_sys::TEXTOID],
			);
			execute(next_plan, &mut [ttl.as_str().into_datum()]);
			next = first_f64();
			Opened::Ready(queue)
		});
		self.expire_at = match done {
			Some(Opened::Ready(_)) => {
				next.map(|s| Instant::now() + Duration::from_secs_f64(s.clamp(1.0, 86_400.0)))
			}
			Some(Opened::Missing) => None,
			Some(Opened::Busy) | None => Some(Instant::now() + Duration::from_secs(1)),
		};
	}
}

/// Deletes queue rows by where they were read from, checking the id so a row that has moved (a
/// VACUUM FULL, a restore) is never mistaken for another; any not found there are deleted by id.
fn forget(plans: &mut Plans, rows: &[(i64, String)]) {
	if rows.is_empty() {
		return;
	}
	let ids: Vec<i64> = rows.iter().map(|(id, _)| *id).collect();
	let ctids: Vec<String> = rows.iter().map(|(_, ctid)| ctid.clone()).collect();
	let plan = prepare(
		&mut plans.forget,
		c"delete from net.http_request_queue where ctid = any($1::tid[]) and id = any($2) returning id",
		&mut [pg_sys::TEXTARRAYOID, pg_sys::INT8ARRAYOID],
	);
	let n = execute(plan, &mut [ctids.into_datum(), ids.clone().into_datum()]);
	if (n as usize) < ids.len() {
		let gone: HashSet<i64> = returned_i64s(n).into_iter().collect();
		let left: Vec<i64> = ids.into_iter().filter(|id| !gone.contains(id)).collect();
		let plan = prepare(
			&mut plans.forget_by_id,
			c"delete from net.http_request_queue where id = any($1)",
			&mut [pg_sys::INT8ARRAYOID],
		);
		execute(plan, &mut [left.into_datum()]);
	}
}

/// Runs `body` in its own transaction. `None` when it raised an ERROR (logged as a warning, and the
/// transaction rolled back); what `body` returned otherwise.
fn transaction(body: impl FnOnce() -> Opened) -> Option<Opened> {
	unsafe {
		pg_sys::SetCurrentStatementStartTimestamp();
		pg_sys::StartTransactionCommand();
		pg_sys::PushActiveSnapshot(pg_sys::GetTransactionSnapshot());
	}
	let result = PgTryBuilder::new(AssertUnwindSafe(move || {
		unsafe { pg_sys::SPI_connect() };
		let r = body();
		unsafe {
			pg_sys::SPI_finish();
			pg_sys::PopActiveSnapshot();
			pg_sys::CommitTransactionCommand();
		}
		Some(r)
	}))
	.catch_others(|e| {
		pgrx::warning!("snout_net: {e:?}");
		unsafe { pg_sys::AbortCurrentTransaction() };
		None
	})
	.execute();
	unsafe { pg_sys::pgstat_report_stat(false) };
	result
}

/// The request tables, locked against a concurrent DROP for the rest of the transaction.
fn open_tables() -> Opened {
	unsafe {
		let net = pg_sys::get_namespace_oid(c"net".as_ptr(), true);
		if net == pg_sys::InvalidOid {
			return Opened::Missing;
		}
		let queue = pg_sys::get_relname_relid(c"http_request_queue".as_ptr(), net);
		let responses = pg_sys::get_relname_relid(c"_http_response".as_ptr(), net);
		if queue == pg_sys::InvalidOid || responses == pg_sys::InvalidOid {
			return Opened::Missing;
		}
		let lock = pg_sys::AccessShareLock as pg_sys::LOCKMODE;
		if !pg_sys::ConditionalLockRelationOid(queue, lock)
			|| !pg_sys::ConditionalLockRelationOid(responses, lock)
		{
			return Opened::Busy;
		}
		Opened::Ready(queue)
	}
}

fn prepare(
	slot: &mut Option<pg_sys::SPIPlanPtr>,
	sql: &'static CStr,
	types: &mut [pg_sys::Oid],
) -> pg_sys::SPIPlanPtr {
	if let Some(plan) = *slot {
		return plan;
	}
	unsafe {
		let plan = pg_sys::SPI_prepare(sql.as_ptr(), types.len() as c_int, types.as_mut_ptr());
		assert!(!plan.is_null(), "SPI_prepare failed");
		pg_sys::SPI_keepplan(plan);
		*slot = Some(plan);
		plan
	}
}

fn prepare_respond(slot: &mut Option<pg_sys::SPIPlanPtr>) -> pg_sys::SPIPlanPtr {
	prepare(
		slot,
		c"insert into net._http_response (id, status_code, content, headers, content_type, timed_out, error_msg) \
		  values ($1, $2, $3, $4::jsonb, $5, $6, $7)",
		&mut [
			pg_sys::INT8OID,
			pg_sys::INT4OID,
			pg_sys::TEXTOID,
			pg_sys::TEXTOID,
			pg_sys::TEXTOID,
			pg_sys::BOOLOID,
			pg_sys::TEXTOID,
		],
	)
}

/// Writes one response.
fn respond_now(plan: pg_sys::SPIPlanPtr, r: &Response) {
	let mut values = match &r.outcome {
		Outcome::Response {
			status,
			headers,
			content_type,
			body,
		} => [
			r.id.into_datum(),
			(*status).into_datum(),
			request::content_text(body).into_datum(),
			request::headers_json(headers).into_datum(),
			content_type.clone().into_datum(),
			false.into_datum(),
			None::<String>.into_datum(),
		],
		Outcome::Failed { timed_out, message } => [
			r.id.into_datum(),
			None::<i32>.into_datum(),
			None::<String>.into_datum(),
			None::<String>.into_datum(),
			None::<String>.into_datum(),
			(*timed_out).into_datum(),
			message.clone().into_datum(),
		],
	};
	execute(plan, &mut values);
}

/// Runs a plan with arguments (`None` is SQL null) and returns how many rows it returned or changed.
fn execute(plan: pg_sys::SPIPlanPtr, args: &mut [Option<pg_sys::Datum>]) -> u64 {
	let mut datums: Vec<pg_sys::Datum> = args
		.iter()
		.map(|a| a.unwrap_or(pg_sys::Datum::from(0usize)))
		.collect();
	let nulls: Vec<c_char> = args
		.iter()
		.map(|a| {
			if a.is_some() {
				b' ' as c_char
			} else {
				b'n' as c_char
			}
		})
		.collect();
	unsafe {
		let rc = pg_sys::SPI_execute_plan(plan, datums.as_mut_ptr(), nulls.as_ptr(), false, 0);
		assert!(rc >= 0, "SPI_execute_plan failed: {rc}");
		pg_sys::SPI_processed
	}
}

/// The first column of each of the `n` rows the last statement returned, as bigints.
fn returned_i64s(n: u64) -> Vec<i64> {
	let mut out = Vec::with_capacity(n as usize);
	unsafe {
		let table = pg_sys::SPI_tuptable;
		if table.is_null() {
			return out;
		}
		for i in 0..n as usize {
			let mut is_null = false;
			let d = pg_sys::SPI_getbinval(*(*table).vals.add(i), (*table).tupdesc, 1, &mut is_null);
			if let Some(v) = i64::from_datum(d, is_null) {
				out.push(v);
			}
		}
	}
	out
}

/// The first column of the first row the last statement returned, as a float8.
fn first_f64() -> Option<f64> {
	unsafe {
		let table = pg_sys::SPI_tuptable;
		if table.is_null() || pg_sys::SPI_processed == 0 {
			return None;
		}
		let mut is_null = false;
		let d = pg_sys::SPI_getbinval(*(*table).vals, (*table).tupdesc, 1, &mut is_null);
		f64::from_datum(d, is_null)
	}
}

/// Runs the queue read and turns each row into a request to send, or a response refusing it,
/// each with the row's ctid.
fn read_queue(
	plan: pg_sys::SPIPlanPtr,
	args: &mut [Option<pg_sys::Datum>],
) -> Vec<(Result<Request, Response>, String)> {
	let n = execute(plan, args) as usize;
	let max_timeout = settings::max_timeout_ms();
	let mut out = Vec::with_capacity(n);
	unsafe {
		let table = pg_sys::SPI_tuptable;
		let desc = (*table).tupdesc;
		for i in 0..n {
			let tuple = *(*table).vals.add(i);
			let col = |c: c_int| {
				let mut is_null = false;
				let d = pg_sys::SPI_getbinval(tuple, desc, c, &mut is_null);
				(d, is_null)
			};
			let (d, null) = col(1);
			let id = i64::from_datum(d, null).unwrap_or(0);
			let (d, null) = col(2);
			let method = String::from_datum(d, null).unwrap_or_default();
			let (d, null) = col(3);
			let url = String::from_datum(d, null).unwrap_or_default();
			let (d, null) = col(4);
			let timeout_ms = i32::from_datum(d, null).unwrap_or(0);
			let (d, null) = col(5);
			let headers = Vec::<Option<String>>::from_datum(d, null).unwrap_or_default();
			let (d, null) = col(6);
			let body = Vec::<u8>::from_datum(d, null);
			let (d, null) = col(7);
			let ctid = String::from_datum(d, null).unwrap_or_default();
			out.push((
				check(id, &method, url, timeout_ms, headers, body, max_timeout),
				ctid,
			));
		}
	}
	out
}

/// A queued request made ready to send, or the response that refuses it.
fn check(
	id: i64,
	method: &str,
	url: String,
	timeout_ms: i32,
	headers: Vec<Option<String>>,
	body: Option<Vec<u8>>,
	max_timeout: i32,
) -> Result<Request, Response> {
	let refuse = |message: String| Response {
		id,
		outcome: Outcome::Failed {
			timed_out: false,
			message,
		},
	};
	let Some(method) = Method::parse(method) else {
		return Err(refuse(format!("Unsupported request method {method}")));
	};
	request::check_timeout(timeout_ms, max_timeout).map_err(refuse)?;
	let mut lines: Vec<String> = headers.into_iter().flatten().collect();
	for line in &lines {
		request::check_header(line).map_err(refuse)?;
	}
	lines.push(USER_AGENT.to_owned());
	Ok(Request {
		id,
		method,
		url,
		headers: lines,
		body,
		timeout_ms,
	})
}
