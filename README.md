# snout_net

Asynchronous HTTP requests from SQL. `select net.http_post(url, body)` queues a request and returns
its id at once; a background worker sends it once the calling transaction commits, and the
response lands in `net._http_response`. A Postgres extension written in Rust with
[pgrx](https://github.com/pgcentralfoundation/pgrx), over the system's libcurl, built to run in
every SnoutData Cloud project.

```sql
create extension snout_net;

select net.http_post(
  url  := 'https://example.com/hook',
  body := jsonb_build_object('event', 'signup')
) as request_id;

-- once the transaction has committed and the request has been answered:
select status_code, content, error_msg from net._http_response where id = 1;
```

- **Nothing waits on the network.** A request is read from the queue without being deleted and is
  deleted in the same short transaction that writes its response. No transaction is open while a
  request is on the network, so a slow endpoint delays nobody else's request, a response is
  visible the moment it is written, and vacuum is never held back by an HTTP call. A worker that
  stops mid-request sends it again when it restarts, because its queue row is still there.
- **Sent after COMMIT.** A transaction that rolls back sends nothing; one that queues 100,000
  requests wakes the worker once.
- **Egress is refused, not just routed away.** Every address the client is about to connect to
  (after DNS, for an IP literal, and again on every redirect) is checked, and loopback,
  link-local (where cloud instance metadata lives), private, shared-address, unique-local and
  multicast ranges are refused before a socket exists, with a sentence in `error_msg`, unless
  `snout_net.allowed_networks` names them.
- **Refusals are responses.** A timeout outside the allowed range, or a header containing a line
  break, is never sent: the request gets a response row saying why.

## Functions

| Function | Returns |
|---|---|
| `net.http_get(url text, params jsonb default '{}', headers jsonb default '{}', timeout_milliseconds int default 5000)` | the request id |
| `net.http_post(url text, body jsonb default '{}', params jsonb default '{}', headers jsonb default '{"Content-Type": "application/json"}', timeout_milliseconds int default 5000)` | the request id |
| `net.http_delete(url text, params jsonb default '{}', headers jsonb default '{}', timeout_milliseconds int default 5000, body jsonb default null)` | the request id |
| `net._http_collect_response(request_id bigint, async bool default true)` | `(status, message, response)`; with `async => false` it waits (only useful in a later transaction than the one that queued it) |
| `net.check_worker_is_up()` | raises if the worker is not running |
| `net.wait_until_running()` | returns once the worker is running |
| `net.worker_restart()` | reloads the configuration and starts a new worker |

`params` are percent-encoded and appended to the URL's query. A URL the client cannot parse is an
error when it is queued. `http_post` refuses a `Content-Type` other than `application/json`.

## Tables

`net.http_request_queue` holds requests not yet answered. `net._http_response` holds one row per
request sent: `status_code`, `headers` (the final response's, after any redirects), `content`
(the body as text, up to its first NUL byte, with any bytes that are not UTF-8 replaced by
U+FFFD), `content_type`, `timed_out`, `error_msg` and `created`. Responses are deleted
`snout_net.ttl` after they were written; a table that inherits from `net._http_response` is not.

Concurrent requests to one endpoint arrive in any order.

## Configuration

| Setting | Default | What |
|---|---|---|
| `snout_net.database_name` | `postgres` | The database whose queue the worker serves |
| `snout_net.username` | the bootstrap superuser | The role the worker connects as |
| `snout_net.ttl` | `6 hours` | How long a response is kept; a negative or unreadable interval is refused when set |
| `snout_net.max_concurrent` | `200` | Requests on the network at once |
| `snout_net.max_timeout_ms` | `600000` | The longest `timeout_milliseconds` a request may ask for |
| `snout_net.max_response_bytes` | `64MB` | A larger response body is an error, not a truncated body |
| `snout_net.allowed_networks` | empty | Internal networks a request may reach anyway, comma-separated (`172.18.0.0/16, fd00::/8`) |
| `snout_net.worker_type` | `snout_net worker` | The worker's `backend_type` in `pg_stat_activity`; set at server start |

Every setting but `worker_type` is `sighup`: the configuration file or command line sets it, a
reload changes it, and no session can. The library must be in `shared_preload_libraries`. It needs
libcurl 7.85 or later at run time. None of the settings is secret.

## Operations

- **Health:** `select net.check_worker_is_up()`; the worker is the `pg_stat_activity` row whose
  `backend_type` is `snout_net.worker_type`, and its start line in the server log names the
  libcurl in use.
- **Logs:** a response that cannot be written (a trigger or constraint on `net._http_response`
  raised) is a WARNING naming the request; the request is not sent again.
- **Upgrade:** replace the library and restart the server; the worker has no state of its own
  beyond the two tables.

## Building

```sh
bash scripts/dev.sh cargo pgrx test pg17            # unit tests
bash scripts/dev.sh cargo clippy --lib -- -D warnings
bash scripts/dev.sh cargo deny --locked check       # licences, bans, advisories, sources
bash scripts/build-dist.sh tools 17 && bash scripts/build-dist.sh build 17 /out
```

Licensed under the [Apache License 2.0](./LICENSE). Security reports: [SECURITY.md](./SECURITY.md).
