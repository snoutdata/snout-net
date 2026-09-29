//! snout_net: asynchronous HTTP requests from SQL.
//!
//! `select net.http_post(url, body)` queues a request and returns its id straight away; a
//! background worker sends it, and the response lands in `net._http_response`. The SQL objects are
//! the install script's (sql/); this library is the worker, the HTTP client it drives, and the
//! handful of functions the SQL calls into. README.md is the reference.
mod client;
mod curl;
pub mod egress;
pub mod request;
mod settings;
mod shared;
mod sqlfns;
mod worker;

use pgrx::bgworkers::{BackgroundWorkerBuilder, BgWorkerStartTime};
use pgrx::pg_sys;
use std::time::Duration;

pgrx::pg_module_magic!();

#[pgrx::pg_guard]
pub extern "C-unwind" fn _PG_init() {
	unsafe {
		if pg_sys::IsBinaryUpgrade {
			return;
		}
		if !pg_sys::process_shared_preload_libraries_in_progress {
			pgrx::error!(
				"snout_net must be loaded by shared_preload_libraries: its worker starts with the server"
			);
		}
	}
	settings::init();
	unsafe { shared::install() };
	let worker_type = settings::worker_type();
	BackgroundWorkerBuilder::new(&worker_type)
		.set_type(&worker_type)
		.set_library("snout_net")
		.set_function("snout_net_worker")
		.enable_spi_access()
		.set_start_time(BgWorkerStartTime::RecoveryFinished)
		.set_restart_time(Some(Duration::from_secs(1)))
		.load();
}

/// Required by `cargo pgrx test`; must sit at the crate root.
#[cfg(test)]
pub mod pg_test {
	pub fn setup(_options: Vec<&str>) {}

	pub fn postgresql_conf_options() -> Vec<&'static str> {
		vec!["shared_preload_libraries = 'snout_net'"]
	}
}
