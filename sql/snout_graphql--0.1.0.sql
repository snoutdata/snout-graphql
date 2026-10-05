-- snout_graphql 0.1.0: the SQL objects. Their names and signatures are the interface a
-- database's own SQL, and every GraphQL client of it, reaches this extension through.

-- Raised from generated SQL when a write is refused (a mutation over its `atMost`).
create function graphql.exception(message text)
	returns text
	language plpgsql
as $$
begin
	raise exception using errcode = '22000', message = message;
end;
$$;

-- The directive in an object's comment: the JSON inside `@graphql(...)`, or an empty object.
create function graphql.comment_directive(comment_ text)
	returns jsonb
	language sql
	immutable
as $$
	select coalesce((regexp_match(comment_, '@graphql\((.+)\)'))[1]::jsonb, jsonb_build_object())
$$;

-- The schema's version: moved by every change that can alter what GraphQL reflects, so a
-- connection knows when the schema it keeps is out of date.
create sequence graphql.seq_schema_version as int cycle;

-- Both security definer functions below run as the extension's owner, which on a managed platform
-- is a superuser, and an event trigger runs for every role's DDL. So each fixes its own
-- search_path: an operator or function the caller put ahead of pg_catalog on theirs is never the
-- one these resolve to.
create function graphql.get_schema_version()
	returns int
	security definer
	set search_path = pg_catalog, pg_temp
	language sql
as $$
	select last_value from graphql.seq_schema_version;
$$;

-- Temporary objects and materialized view refreshes cannot change what GraphQL reflects, and a
-- function that creates a temporary table on every call would otherwise make every connection
-- rebuild its schema on every request.
create function graphql.increment_schema_version()
	returns event_trigger
	security definer
	set search_path = pg_catalog, pg_temp
	language plpgsql
as $$
begin
	if tg_tag operator(pg_catalog.=) 'REFRESH MATERIALIZED VIEW' then
		return;
	end if;
	if tg_event operator(pg_catalog.=) 'ddl_command_end'
		and exists (select 1 from pg_catalog.pg_event_trigger_ddl_commands())
		and not exists (
			select 1
			  from pg_catalog.pg_event_trigger_ddl_commands() c
			 where c.schema_name is null or c.schema_name operator(pg_catalog.!~~) 'pg\_temp%'
		)
	then
		return;
	end if;
	if tg_event operator(pg_catalog.=) 'sql_drop'
		and exists (select 1 from pg_catalog.pg_event_trigger_dropped_objects())
		and not exists (
			select 1 from pg_catalog.pg_event_trigger_dropped_objects() d where not d.is_temporary
		)
	then
		return;
	end if;
	perform pg_catalog.nextval('graphql.seq_schema_version');
end;
$$;

create event trigger graphql_watch_ddl
	on ddl_command_end
	execute procedure graphql.increment_schema_version();

create event trigger graphql_watch_drop
	on sql_drop
	execute procedure graphql.increment_schema_version();

create function graphql._internal_resolve(
	"query" text,
	"variables" jsonb default '{}',
	"operationName" text default null,
	"extensions" jsonb default null
)
	returns jsonb
	language c
as 'MODULE_PATHNAME', 'resolve_wrapper';

-- The entry point. An error anywhere in resolving a request, including one Postgres raises, comes
-- back as the response's `errors` with `data` null, and undoes whatever the request wrote.
create function graphql.resolve(
	"query" text,
	"variables" jsonb default '{}',
	"operationName" text default null,
	"extensions" jsonb default null
)
	returns jsonb
	language plpgsql
as $$
declare
	res jsonb;
	message_text text;
begin
	begin
		select graphql._internal_resolve(
			"query" := "query",
			"variables" := "variables",
			"operationName" := "operationName",
			"extensions" := "extensions"
		) into res;
		return res;
	exception
		when others then
			get stacked diagnostics message_text = message_text;
			return jsonb_build_object(
				'data', null,
				'errors', jsonb_build_array(jsonb_build_object('message', message_text))
			);
	end;
end;
$$;
