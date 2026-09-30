# snout_graphql

GraphQL answered inside Postgres, from the database's own schema. One SQL function,
`graphql.resolve(query, variables, "operationName", extensions)`, takes a GraphQL request and
returns the response as `jsonb`, as the role that called it: the schema is the tables, views and
functions that role can see, and every read and write runs under its privileges and its row-level
security. There is no server to run and nothing to keep in step with the database.

It is what serves `/graphql/v1` on SnoutData Cloud, and it is a drop-in replacement for the
GraphQL extension most hosted Postgres stacks ship: the same SQL interface, the same reflected
schema, the same answers, so an existing application and an existing database notice nothing.
[DIVERGENCES.md](./DIVERGENCES.md) lists everything it does differently, with the reason for each.

Licensed under the [Apache License 2.0](./LICENSE). Security reports: [SECURITY.md](./SECURITY.md).

## Why another one

It answers the same questions faster, in less memory, and correctly in the cases that were wrong:

- **A schema change costs only what it changes.** Temporary tables and materialized view refreshes
  no longer invalidate the reflected schema, so a function that creates a temporary table on every
  call no longer makes every connection rebuild its schema on every request.
- **Memory does not grow with migrations.** A connection keeps one schema per role and search path,
  replaced when the schema changes, where a connection used to keep every version it had seen.
- **Introspection asked twice is answered once.** A request that only introspects (every root
  field `__schema`, `__type` or `__typename`) is kept per connection as its finished answer, so an
  IDE's schema view or a code generator in watch mode is not rebuilt on every refresh.
- **Each document is planned once per connection.** The SQL a document compiles to depends only on
  its shape (values are parameters), so its plan is prepared once and reused.
- **Values are checked before they reach SQL.** A `BigInt` that is not an integer or a `UUID` that
  is not a UUID is refused as a GraphQL error naming the type, and a list is never handed to
  Postgres where a single value is expected.
- **What a role sees follows its grants.** Granting a role takes effect on the next request, and a
  transaction that rolls back takes the schema it showed with it.

## Install

```sql
create extension snout_graphql;
comment on schema public is '@graphql({"inflect_names": true})';
select graphql.resolve($$ { __typename } $$);
```

Postgres 17. The extension creates the `graphql` schema and its objects (below); it needs a
superuser to install, because it installs two event triggers.

## Interface

| Object | What it is |
| --- | --- |
| `graphql.resolve(query text, variables jsonb default '{}', "operationName" text default null, extensions jsonb default null) returns jsonb` | The entry point. An error anywhere, including one Postgres raises, comes back as the response's `errors` with `data` null, and undoes what the request wrote |
| `graphql._internal_resolve(...)` | What `resolve` calls; raises instead of answering an error |
| `graphql.comment_directive(comment text) returns jsonb` | The JSON inside an object comment's `@graphql(...)` |
| `graphql.get_schema_version() returns int`, `graphql.increment_schema_version()`, `graphql.seq_schema_version` | The schema version, moved by every change that can alter what GraphQL reflects |
| `graphql.exception(message text)` | Raised by a mutation over its `atMost` |
| event triggers `graphql_watch_ddl`, `graphql_watch_drop` | Move the schema version |

Over HTTP, put the function behind any server that can call a Postgres function as the request's
role (SnoutData Cloud serves it at `/graphql/v1`).

## Configuration: comment directives

There are no settings. What the schema looks like is decided by comments on the objects, in the
form `@graphql({...})`:

| On | Key | Effect |
| --- | --- | --- |
| schema | `inflect_names: true` | `snake_case` names become `PascalCase` types and `camelCase` fields |
| schema | `max_rows: n` | The most rows a collection returns in one page (default 30) |
| schema | `introspection: true` | Allows `__schema` and `__type`, and shows the schema's types to them (off by default) |
| table, view | `name`, `description` | The type's name and description |
| table, view | `totalCount: {"enabled": true}` | Adds `totalCount` to the collection |
| table, view | `aggregate: {"enabled": true}` | Adds `aggregate { count sum avg min max }` |
| table, view | `max_rows: n` | Overrides the schema's page size |
| view | `primary_key_columns: [...]` | The view's key, which a type needs |
| view | `foreign_keys: [{local_columns, foreign_schema, foreign_table, foreign_columns, local_name?, foreign_name?}]` | Relationships to or from a view |
| column | `name`, `description` | The field's name and description |
| function | `name`, `description` | The field's name and description |
| foreign key | `local_name`, `foreign_name` | The relationship fields' names |
| enum | `name`, `mappings: {"db_label": "GRAPHQL_VALUE"}` | The type's name and its values' names |

### Additions, off until asked for

Each is `{"<key>": {"enabled": true}}` in a comment. On a table it is that table's; on a schema it
is every table's there unless the table's own comment says otherwise. A database that asks for none
of them is reflected exactly as it was, which is what keeps generated clients working.

| On | Key | Effect |
| --- | --- | --- |
| table, schema | `relationFilters` | A filter field per relation: a related row's filter, or `some` / `every` / `none` over a collection. Compiled to `EXISTS` subqueries run as the caller, so the related table's policies decide what matches |
| table, schema | `orderByRelated` | Order by a related row's field, or by `count` of a collection; cursors carry the value |
| table, schema | `upsert` | `onConflict: {constraint, updateFields, filter}` on the insert; `constraint` is an enum of the table's whole-column unique indexes |
| table, schema | `distinctOn` | `distinctOn: [<Table>Field]` on the table's collections: the first row per value in the collection's order |
| table | `root` (`enabled: false`) | No collection or by-key field on `Query`; still reached through relations |
| table, schema | `enumArrays` | Filters on arrays of an enum |
| schema | `domains` | A domain is its base type's GraphQL type (upstream reads `Opaque`), its checks still applied on write |
| schema | `composites` | Composite types as object types: columns selected into and filtered by attribute, functions returning one |
| schema | `functionShapes` | Overloads told apart by a `name` directive, unnamed arguments as `arg1`..., enum arguments and results, computed fields with arguments, filters on computed fields |
| schema | `postgis` | `geometry` and `geography` as a `GeoJSON` scalar, with `intersects`, `contains`, `within`, `dWithin` |
| schema | `explain` | `extensions: {"explain": true}` returns each statement, its parameters and its plan |
| schema | `schemaReport` | `extensions: {"schemaReport": true}` returns every table and function, reflected or not, and why not |

And one that is ON unless a schema turns it off: `validation`, the specification's validation rules
before anything runs, in the reference implementation's words (`{"validation": {"enabled": false}}`
restores upstream's behaviour; DIVERGENCES.md D13).

And two that take values, on a schema:

| Key | Effect |
| --- | --- |
| `limits: {"fields": n, "rows": n}` | The most one document may select: fields, and rows as each collection's page times the pages around it |
| `allowlist: {"table": "schema.table", "roles": [...]}` | For those roles (default `anon` and `authenticated`), only documents whose SHA-256 is in the table's `hash` column run. Any role may send only `extensions.persistedQuery.sha256Hash`, and the table's `document` runs |

Whatever is configured, a document may expand to at most 1,000,000 selections once its fragments
are spread.

## Operations

Nothing runs in the background. Each connection keeps its reflected schema and its prepared plans
in its own memory: at most 8 schemas (one per role and search path), 256 plans, and 4 introspection
answers of up to 4 MB each. A schema is read from the catalogue on a connection's first request and
again after any change that can alter it.

## Building

```sh
bash scripts/dev.sh cargo pgrx test pg17            # unit tests
bash scripts/dev.sh cargo clippy --lib -- -D warnings
bash scripts/dev.sh cargo deny --locked check       # licences, bans, advisories, sources
bash scripts/build-dist.sh tools 17 && bash scripts/build-dist.sh build 17 /out
```

The document reader and the decoders are fuzzed by the stack's `graphql_document` and
`graphql_codec` targets.

## Threat model

It runs inside the database, as the role that called it, so it can do nothing that role cannot:
every statement it runs is a query or write that Postgres checks against that role's privileges
and row-level security policies, and it holds no credential of its own. What it adds is a parser of
untrusted text (the GraphQL document and its variables) in the database process: a bug there is a
bug in a Postgres backend. So the document parser, argument coercion and the cursor and node id
decoders are fuzzed, values reach SQL only as parameters (never spliced into statement text), and a
document's depth and fragment recursion are bounded.
