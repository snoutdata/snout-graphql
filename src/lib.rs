//! snout_graphql: GraphQL resolved inside Postgres, from the database's own schema.
//!
//! One SQL function, `graphql.resolve(query, variables, "operationName", extensions)`, answers a
//! GraphQL request as the calling role: the schema is the tables, views and functions that role
//! can see, and every read and write runs under its privileges and row-level security. README.md
//! is the reference.
mod allowlist;
mod answers;
mod cache;
mod catalog;
mod codec;
mod coerce;
mod error;
mod exec;
mod intro;
mod limits;
mod pg;
mod plan;
mod report;
mod schema;
mod select;
mod sql;
mod validate;
mod value;

pgrx::pg_module_magic!();

#[allow(unsafe_code)]
mod entry {
	use pgrx::fcinfo::pg_getarg;
	use pgrx::pg_sys;

	/// The version-1 calling convention declaration for the entry point.
	#[unsafe(no_mangle)]
	pub extern "C" fn pg_finfo_resolve_wrapper() -> &'static pg_sys::Pg_finfo_record {
		const V1: pg_sys::Pg_finfo_record = pg_sys::Pg_finfo_record { api_version: 1 };
		&V1
	}

	/// `graphql._internal_resolve(query, variables, "operationName", extensions)`. The symbol name
	/// is the one databases created before this library call, so it is part of the contract.
	#[unsafe(no_mangle)]
	#[pgrx::pg_guard]
	pub unsafe extern "C-unwind" fn resolve_wrapper(
		fcinfo: pg_sys::FunctionCallInfo,
	) -> pg_sys::Datum {
		let query: Option<String> = unsafe { pg_getarg(fcinfo, 0) };
		let variables: Option<pgrx::JsonB> = unsafe { pg_getarg(fcinfo, 1) };
		let operation_name: Option<String> = unsafe { pg_getarg(fcinfo, 2) };
		let extensions: Option<pgrx::JsonB> = unsafe { pg_getarg(fcinfo, 3) };
		match crate::exec::resolve(
			query,
			variables.map(|v| v.0),
			operation_name,
			extensions.map(|v| v.0),
		) {
			crate::exec::Answer::Kept(bytes) => crate::pg::jsonb_from_bytes(&bytes),
			crate::exec::Answer::Text(text, keep) => {
				let datum = crate::pg::jsonb_datum(&text);
				if let Some(key) = keep {
					crate::answers::put(key, crate::pg::jsonb_bytes(datum));
				}
				datum
			}
		}
	}
}

/// Required by `cargo pgrx test`; must sit at the crate root.
#[cfg(test)]
pub mod pg_test {
	pub fn setup(_options: Vec<&str>) {}

	pub fn postgresql_conf_options() -> Vec<&'static str> {
		vec![]
	}
}
