-- snout_net 0.1.0: asynchronous HTTP requests from SQL.
--
-- A call queues a request and returns its id at once; the background worker sends it after the
-- calling transaction commits (a rolled-back transaction sends nothing) and writes the response to
-- net._http_response, where it is kept for snout_net.ttl.

create schema if not exists net;

create domain net.http_method as text
	check (value ilike 'get' or value ilike 'post' or value ilike 'delete');

-- Requests waiting to be sent. A row stays here until its response is written, and is deleted in
-- the same transaction, so a request the worker had not finished when it stopped is sent again.
create unlogged table net.http_request_queue (
	id bigserial,
	method net.http_method not null,
	url text not null,
	headers jsonb,
	body bytea,
	timeout_milliseconds int not null
);

-- One row per request sent: the response, or why there is none.
create unlogged table net._http_response (
	id bigint,
	status_code integer,
	content_type text,
	headers jsonb,
	content text,
	timed_out bool,
	error_msg text,
	created timestamptz not null default now()
);

create index on net._http_response (created);

create type net.request_status as enum ('PENDING', 'SUCCESS', 'ERROR');

create type net.http_response as (
	status_code integer,
	headers jsonb,
	body text
);

create type net.http_response_result as (
	status net.request_status,
	message text,
	response net.http_response
);

-- Library functions. None is STRICT: each decides for itself what a null argument means.

create function net._urlencode_string(string varchar)
	returns text
	language c
	immutable
	as 'MODULE_PATHNAME';

create function net._encode_url_with_params_array(url text, params_array text[])
	returns text
	language c
	immutable
	as 'MODULE_PATHNAME';

create function net.worker_restart()
	returns bool
	language c
	as 'MODULE_PATHNAME';

create function net.wait_until_running()
	returns void
	language c
	as 'MODULE_PATHNAME';
comment on function net.wait_until_running() is 'waits until the worker is running';

create function net.wake()
	returns void
	language c
	as 'MODULE_PATHNAME';

create function net.check_worker_is_up()
	returns void
	language plpgsql
	as $$
begin
	if not exists (
		select from pg_stat_activity
		where backend_type = current_setting('snout_net.worker_type', true)
	) then
		raise exception using
			message = 'the snout_net background worker is not up',
			detail = 'the snout_net background worker is down due to an internal error and cannot process requests',
			hint = 'make sure that you didn''t modify any of snout_net internal tables';
	end if;
end
$$;
comment on function net.check_worker_is_up() is 'raises an exception if the snout_net background worker is not up, otherwise it doesn''t return anything';

create function net.http_get(
	url text,
	params jsonb default '{}'::jsonb,
	headers jsonb default '{}'::jsonb,
	timeout_milliseconds int default 5000
)
	returns bigint
	language plpgsql
	as $$
declare
	request_id bigint;
	params_array text[];
begin
	-- Each parameter's key and value percent-encoded, then appended to the URL's query.
	select coalesce(array_agg(net._urlencode_string(key) || '=' || net._urlencode_string(value)), '{}')
	into params_array
	from jsonb_each_text(params);

	insert into net.http_request_queue (method, url, headers, timeout_milliseconds)
	values ('GET', net._encode_url_with_params_array(url, params_array), headers, timeout_milliseconds)
	returning id into request_id;
	perform net.wake();
	return request_id;
end
$$;

create function net.http_post(
	url text,
	body jsonb default '{}'::jsonb,
	params jsonb default '{}'::jsonb,
	headers jsonb default '{"Content-Type": "application/json"}'::jsonb,
	timeout_milliseconds int default 5000
)
	returns bigint
	language plpgsql
	as $$
declare
	request_id bigint;
	params_array text[];
	content_type text;
begin
	select value into content_type
	from jsonb_each_text(coalesce(headers, '{}'::jsonb))
	where lower(key) = 'content-type'
	limit 1;

	-- A body is JSON, so a request without a Content-Type is sent as JSON, and one that names any
	-- other type is refused.
	if content_type is null then
		headers := headers || '{"Content-Type": "application/json"}'::jsonb;
	elsif content_type <> 'application/json' then
		raise exception 'Content-Type header must be "application/json"';
	end if;

	select coalesce(array_agg(net._urlencode_string(key) || '=' || net._urlencode_string(value)), '{}')
	into params_array
	from jsonb_each_text(params);

	insert into net.http_request_queue (method, url, headers, body, timeout_milliseconds)
	values ('POST', net._encode_url_with_params_array(url, params_array), headers, convert_to(body::text, 'UTF8'), timeout_milliseconds)
	returning id into request_id;
	perform net.wake();
	return request_id;
end
$$;

create function net.http_delete(
	url text,
	params jsonb default '{}'::jsonb,
	headers jsonb default '{}'::jsonb,
	timeout_milliseconds int default 5000,
	body jsonb default null
)
	returns bigint
	language plpgsql
	as $$
declare
	request_id bigint;
	params_array text[];
begin
	select coalesce(array_agg(net._urlencode_string(key) || '=' || net._urlencode_string(value)), '{}')
	into params_array
	from jsonb_each_text(params);

	insert into net.http_request_queue (method, url, headers, body, timeout_milliseconds)
	values ('DELETE', net._encode_url_with_params_array(url, params_array), headers, convert_to(body::text, 'UTF8'), timeout_milliseconds)
	returning id into request_id;
	perform net.wake();
	return request_id;
end
$$;

-- Waits for a response to be written. Only useful across transactions: a request is not sent
-- until the transaction that queued it commits.
create function net._await_response(request_id bigint)
	returns bool
	language plpgsql
	as $$
begin
	while not exists (select from net._http_response r where r.id = request_id) loop
		perform pg_sleep(0.05);
	end loop;
	return true;
end
$$;

create function net._http_collect_response(request_id bigint, async bool default true)
	returns net.http_response_result
	language plpgsql
	as $$
declare
	rec net._http_response;
begin
	if not async then
		perform net._await_response(request_id);
	end if;

	select * into rec from net._http_response r where r.id = request_id;

	-- A request still in flight and one that does not exist look the same here.
	if rec is null or rec.error_msg is not null then
		return ('ERROR', coalesce(rec.error_msg, 'request matching request_id not found'), null)::net.http_response_result;
	end if;

	return ('SUCCESS', 'ok', (rec.status_code, rec.headers, rec.content)::net.http_response)::net.http_response_result;
end
$$;

create function net.http_collect_response(request_id bigint, async bool default true)
	returns net.http_response_result
	language plpgsql
	as $$
begin
	raise notice 'The net.http_collect_response function is deprecated.';
	return net._http_collect_response(request_id, async);
end
$$;

-- Everyone may queue requests and read responses, as with any extension that does not say
-- otherwise; a platform that wants less revokes it after CREATE EXTENSION. Every table privilege
-- but TRIGGER: the worker writes these tables, and a trigger runs its function as the role the
-- worker writes as, which is a superuser wherever the database's owner is one.
grant usage on schema net to public;
grant all on all sequences in schema net to public;
grant select, insert, update, delete, truncate, references, maintain on all tables in schema net to public;
