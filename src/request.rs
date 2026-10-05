//! What is checked about a queued request before it is sent, and how a response is written down.
//! Pure: no `unsafe`, no Postgres, tested with plain `#[test]`s and fuzzed.
#![forbid(unsafe_code)]

/// The HTTP methods the queue's `method` column allows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
	Get,
	Post,
	Delete,
}

impl Method {
	pub fn parse(text: &str) -> Option<Method> {
		if text.eq_ignore_ascii_case("get") {
			Some(Method::Get)
		} else if text.eq_ignore_ascii_case("post") {
			Some(Method::Post)
		} else if text.eq_ignore_ascii_case("delete") {
			Some(Method::Delete)
		} else {
			None
		}
	}
}

/// A request's timeout must be one the client can keep: curl reads 0 as "never", and a request
/// that never ends holds a slot for good.
pub fn check_timeout(timeout_ms: i32, max_ms: i32) -> Result<(), String> {
	if timeout_ms >= 1 && timeout_ms <= max_ms {
		Ok(())
	} else {
		Err(format!(
			"timeout_milliseconds must be between 1 and {max_ms} (snout_net.max_timeout_ms), got {timeout_ms}"
		))
	}
}

/// A header line (`name: value`, as the queue's headers are read) must not carry a line break: a
/// server would read what follows it as another header, or as the body.
pub fn check_header(line: &str) -> Result<(), String> {
	if line.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0) {
		let name = line.split(':').next().unwrap_or("").trim();
		Err(format!(
			"header \"{}\" contains a carriage return, line feed or NUL, which would end the header early",
			name.escape_default()
		))
	} else {
		Ok(())
	}
}

/// How long each step of a request that timed out took, from the client's own timings (seconds
/// since the request started, 0 for a step that never happened).
#[derive(Clone, Copy, Debug, Default)]
pub struct Timings {
	pub namelookup: f64,
	pub connect: f64,
	pub appconnect: f64,
	pub pretransfer: f64,
	pub total: f64,
}

/// The sentence written to `error_msg` for a timeout, naming the step it happened in. A step that
/// ran past the timeout reports 0, so the step that timed out is told apart by which timings are
/// set: no DNS answer and no connection means DNS; no pretransfer means the TCP or TLS handshake;
/// otherwise the HTTP exchange.
pub fn timeout_message(timeout_ms: i32, t: Timings) -> String {
	let at_dns = t.namelookup == 0.0 && t.connect == 0.0;
	let at_handshake = t.pretransfer == 0.0;
	let (dns, handshake, http) = if at_dns {
		(t.total, 0.0, 0.0)
	} else if at_handshake {
		(t.namelookup, t.total - t.namelookup, 0.0)
	} else {
		let tls = if t.appconnect > 0.0 {
			t.appconnect - t.connect
		} else {
			0.0
		};
		(
			t.namelookup,
			(t.connect - t.namelookup) + tls,
			t.total - t.pretransfer,
		)
	};
	format!(
		"Timeout of {timeout_ms} ms reached. Total time: {:.6} ms (DNS time: {:.6} ms, TCP/SSL handshake time: {:.6} ms, HTTP Request/Response time: {:.6} ms)",
		t.total * 1000.0,
		dns * 1000.0,
		handshake * 1000.0,
		http * 1000.0
	)
}

/// A response body as the `content` column holds it: text up to the first NUL (a text value cannot
/// hold one), `None` when that is nothing, and any byte sequence that is not UTF-8 replaced by U+FFFD
/// rather than stored as text the database's encoding does not allow.
pub fn content_text(body: &[u8]) -> Option<String> {
	let end = body.iter().position(|&b| b == 0).unwrap_or(body.len());
	if end == 0 {
		return None;
	}
	Some(String::from_utf8_lossy(&body[..end]).into_owned())
}

/// Response headers as the JSON object text the `headers` column is cast from. A name that
/// repeats keeps its last value, as a jsonb object does.
pub fn headers_json(headers: &[(String, String)]) -> String {
	let mut out = String::from("{");
	for (i, (name, value)) in headers.iter().enumerate() {
		if i > 0 {
			out.push_str(", ");
		}
		push_json_string(&mut out, name);
		out.push_str(": ");
		push_json_string(&mut out, value);
	}
	out.push('}');
	out
}

fn push_json_string(out: &mut String, s: &str) {
	out.push('"');
	for c in s.chars() {
		match c {
			'"' => out.push_str("\\\""),
			'\\' => out.push_str("\\\\"),
			'\n' => out.push_str("\\n"),
			'\r' => out.push_str("\\r"),
			'\t' => out.push_str("\\t"),
			// jsonb refuses \u0000, so a NUL is dropped rather than written.
			'\0' => {}
			c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
			c => out.push(c),
		}
	}
	out.push('"');
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn methods_are_the_three_the_queue_allows() {
		assert_eq!(Method::parse("GET"), Some(Method::Get));
		assert_eq!(Method::parse("post"), Some(Method::Post));
		assert_eq!(Method::parse("DeLeTe"), Some(Method::Delete));
		assert_eq!(Method::parse("PUT"), None);
		assert_eq!(Method::parse(""), None);
	}

	#[test]
	fn a_timeout_outside_the_bounds_is_refused() {
		assert!(check_timeout(1, 600_000).is_ok());
		assert!(check_timeout(600_000, 600_000).is_ok());
		assert!(check_timeout(0, 600_000).unwrap_err().contains("got 0"));
		assert!(check_timeout(-5, 600_000).is_err());
		assert!(check_timeout(600_001, 600_000).is_err());
	}

	#[test]
	fn a_header_with_a_line_break_is_refused_by_name() {
		assert!(check_header("X-Token: abc").is_ok());
		let e = check_header("X-Evil: a\r\nHost: other").unwrap_err();
		assert!(e.contains("\"X-Evil\""), "{e}");
		assert!(check_header("X-Evil: a\n").is_err());
		assert!(check_header("X\0: a").is_err());
	}

	#[test]
	fn a_timeout_names_the_step_it_happened_in() {
		let dns = timeout_message(
			800,
			Timings {
				total: 0.801,
				..Default::default()
			},
		);
		assert_eq!(
			dns,
			"Timeout of 800 ms reached. Total time: 801.000000 ms (DNS time: 801.000000 ms, TCP/SSL handshake time: 0.000000 ms, HTTP Request/Response time: 0.000000 ms)"
		);
		let handshake = timeout_message(
			800,
			Timings {
				namelookup: 0.1,
				total: 0.8,
				..Default::default()
			},
		);
		assert!(handshake.contains("DNS time: 100.000000 ms, TCP/SSL handshake time: 700.000000 ms, HTTP Request/Response time: 0.000000 ms"), "{handshake}");
		let http = timeout_message(
			800,
			Timings {
				namelookup: 0.1,
				connect: 0.2,
				appconnect: 0.3,
				pretransfer: 0.3,
				total: 0.8,
			},
		);
		assert!(http.contains("DNS time: 100.000000 ms, TCP/SSL handshake time: 200.000000 ms, HTTP Request/Response time: 500.000000 ms"), "{http}");
	}

	#[test]
	fn a_body_is_text_up_to_its_first_nul() {
		assert_eq!(content_text(b""), None);
		assert_eq!(content_text(b"\0abc"), None);
		assert_eq!(content_text(b"ok\0rest").as_deref(), Some("ok"));
		assert_eq!(content_text(b"caf\xc3\xa9").as_deref(), Some("café"));
		assert_eq!(content_text(b"a\xffb").as_deref(), Some("a\u{fffd}b"));
	}

	#[test]
	fn headers_become_a_json_object() {
		let h = vec![
			("Content-Type".to_string(), "text/plain".to_string()),
			("X-Q".to_string(), "a\"b\\c\u{1}".to_string()),
		];
		assert_eq!(
			headers_json(&h),
			r#"{"Content-Type": "text/plain", "X-Q": "a\"b\\c\u0001"}"#
		);
		assert_eq!(headers_json(&[]), "{}");
	}
}
