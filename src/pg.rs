//! The one place this crate talks to Postgres: queries through SPI, and the per-backend cache of
//! prepared plans for the statements GraphQL documents compile to.
//!
//! A query here either returns rows or raises a Postgres ERROR, which unwinds through the caller
//! and reaches the client as Postgres raised it, exactly as a failing statement would.
#![allow(unsafe_code)]

use pgrx::datum::DatumWithOid;
use pgrx::pg_sys::{self, PgBuiltInOids, PgOid};
use pgrx::spi::{OwnedPreparedStatement, SpiClient, SpiHeapTupleData};
use pgrx::{IntoDatum, Spi};
use std::cell::RefCell;
use std::collections::HashMap;

/// A value sent with a query.
#[derive(Clone, Debug)]
pub enum Arg {
	Text(Option<String>),
	TextArray(Vec<Option<String>>),
	Int8Array(Vec<i64>),
}

impl Arg {
	fn oid(&self) -> PgOid {
		match self {
			Arg::Text(_) => PgOid::BuiltIn(PgBuiltInOids::TEXTOID),
			Arg::TextArray(_) => PgOid::BuiltIn(PgBuiltInOids::TEXTARRAYOID),
			Arg::Int8Array(_) => PgOid::BuiltIn(PgBuiltInOids::INT8ARRAYOID),
		}
	}

	/// The value as JSON, for showing what a statement was run with.
	pub fn to_json(&self) -> serde_json::Value {
		match self {
			Arg::Text(v) => serde_json::json!(v),
			Arg::TextArray(v) => serde_json::json!(v),
			Arg::Int8Array(v) => serde_json::json!(v),
		}
	}

	fn datum(&self) -> DatumWithOid<'static> {
		match self {
			Arg::Text(v) => v.clone().into(),
			Arg::TextArray(v) => v.clone().into(),
			Arg::Int8Array(v) => v.clone().into(),
		}
	}
}

/// One value of a result row, read by the column's type.
#[derive(Clone, Debug, Default)]
pub enum Cell {
	#[default]
	Null,
	Text(String),
	Int(i64),
	Bool(bool),
	TextArray(Vec<Option<String>>),
	IntArray(Vec<Option<i64>>),
}

#[derive(Clone, Debug, Default)]
pub struct Row(Vec<Cell>);

impl Row {
	fn cell(&self, i: usize) -> &Cell {
		self.0.get(i).unwrap_or(&Cell::Null)
	}

	pub fn text(&self, i: usize) -> Option<String> {
		match self.cell(i) {
			Cell::Text(s) => Some(s.clone()),
			Cell::Int(n) => Some(n.to_string()),
			Cell::Bool(b) => Some(b.to_string()),
			_ => None,
		}
	}

	pub fn int8(&self, i: usize) -> Option<i64> {
		match self.cell(i) {
			Cell::Int(n) => Some(*n),
			_ => None,
		}
	}

	pub fn int4(&self, i: usize) -> Option<i32> {
		self.int8(i).map(|n| n as i32)
	}

	pub fn bool(&self, i: usize) -> Option<bool> {
		match self.cell(i) {
			Cell::Bool(b) => Some(*b),
			_ => None,
		}
	}

	/// A text array with its NULL elements dropped.
	pub fn text_array(&self, i: usize) -> Vec<String> {
		match self.cell(i) {
			Cell::TextArray(v) => v.iter().flatten().cloned().collect(),
			_ => vec![],
		}
	}

	pub fn opt_text_array(&self, i: usize) -> Option<Vec<Option<String>>> {
		match self.cell(i) {
			Cell::TextArray(v) => Some(v.clone()),
			_ => None,
		}
	}

	pub fn int8_array(&self, i: usize) -> Vec<i64> {
		match self.cell(i) {
			Cell::IntArray(v) => v.iter().flatten().copied().collect(),
			_ => vec![],
		}
	}
}

fn read_row(row: &SpiHeapTupleData, columns: &[pg_sys::Oid]) -> Row {
	let mut cells = Vec::with_capacity(columns.len());
	for (i, oid) in columns.iter().enumerate() {
		let ord = i + 1;
		let cell = match PgOid::from(*oid) {
			PgOid::BuiltIn(PgBuiltInOids::BOOLOID) => {
				row.get::<bool>(ord).ok().flatten().map(Cell::Bool)
			}
			PgOid::BuiltIn(PgBuiltInOids::INT8OID) => {
				row.get::<i64>(ord).ok().flatten().map(Cell::Int)
			}
			PgOid::BuiltIn(PgBuiltInOids::INT4OID) => row
				.get::<i32>(ord)
				.ok()
				.flatten()
				.map(|n| Cell::Int(n.into())),
			PgOid::BuiltIn(PgBuiltInOids::INT2OID) => row
				.get::<i16>(ord)
				.ok()
				.flatten()
				.map(|n| Cell::Int(n.into())),
			PgOid::BuiltIn(PgBuiltInOids::TEXTARRAYOID) => row
				.get::<Vec<Option<String>>>(ord)
				.ok()
				.flatten()
				.map(Cell::TextArray),
			PgOid::BuiltIn(PgBuiltInOids::INT8ARRAYOID) => row
				.get::<Vec<Option<i64>>>(ord)
				.ok()
				.flatten()
				.map(Cell::IntArray),
			// `EXPLAIN (FORMAT JSON)`'s one column.
			PgOid::BuiltIn(PgBuiltInOids::JSONOID) => row
				.get::<pgrx::Json>(ord)
				.ok()
				.flatten()
				.map(|j| Cell::Text(j.0.to_string())),
			_ => row.get::<String>(ord).ok().flatten().map(Cell::Text),
		};
		cells.push(cell.unwrap_or(Cell::Null));
	}
	Row(cells)
}

fn collect(client: &SpiClient, table: pgrx::spi::SpiTupleTable) -> Vec<Row> {
	let _ = client;
	let ncols = table.columns().unwrap_or(0);
	let columns: Vec<pg_sys::Oid> = (1..=ncols)
		.map(|i| {
			table
				.column_type_oid(i)
				.map(|o| o.value())
				.unwrap_or(pg_sys::InvalidOid)
		})
		.collect();
	table.map(|row| read_row(&row, &columns)).collect()
}

/// Run a read-only statement and return its rows. For the catalogue queries: their text never
/// varies, so each is planned once per connection.
pub fn query(sql: &str, args: &[Arg]) -> Vec<Row> {
	execute(sql, args, false)
}

/// Run a statement SPI will not run read-only (`EXPLAIN` is a utility statement), and return its
/// rows.
pub fn query_utility(sql: &str, args: &[Arg]) -> Vec<Row> {
	execute(sql, args, true)
}

thread_local! {
	/// Prepared plans by statement text. A document compiles to the same text every time it is
	/// sent (block names are positional, values are parameters), so a plan is prepared once per
	/// backend and reused.
	static PLANS: RefCell<PlanCache> = RefCell::new(PlanCache::default());
}

/// How many plans a backend keeps. Each is a few kilobytes of cached plan tree.
const PLAN_CACHE_SIZE: usize = 256;

#[derive(Default)]
struct PlanCache {
	plans: HashMap<String, (OwnedPreparedStatement, u64)>,
	clock: u64,
}

impl PlanCache {
	/// Take a plan out while it runs, so a statement that re-enters this extension (a computed
	/// field that itself resolves GraphQL) finds the cache free.
	fn take(&mut self, sql: &str) -> Option<OwnedPreparedStatement> {
		self.plans.remove(sql).map(|(plan, _)| plan)
	}

	fn put(&mut self, sql: String, plan: OwnedPreparedStatement) {
		self.clock += 1;
		if self.plans.len() >= PLAN_CACHE_SIZE
			&& let Some(oldest) = self
				.plans
				.iter()
				.min_by_key(|(_, (_, used))| *used)
				.map(|(k, _)| k.clone())
		{
			self.plans.remove(&oldest);
		}
		self.plans.insert(sql, (plan, self.clock));
	}
}

/// Run a generated statement through the plan cache. `mutating` statements run writable.
/// Returns the first column of the first row, as text.
pub fn run(sql: &str, args: &[Arg], mutating: bool) -> Option<String> {
	execute(sql, args, mutating).first().and_then(|r| r.text(0))
}

fn execute(sql: &str, args: &[Arg], mutating: bool) -> Vec<Row> {
	let datums: Vec<DatumWithOid> = args.iter().map(Arg::datum).collect();
	Spi::connect_mut(|client| {
		let plan = match PLANS.with(|c| c.borrow_mut().take(sql)) {
			Some(plan) => plan,
			None => {
				let types: Vec<PgOid> = args.iter().map(Arg::oid).collect();
				if mutating {
					client.prepare_mut(sql, &types)
				} else {
					client.prepare(sql, &types)
				}
				.unwrap_or_else(|e| spi_failed(e))
				.keep()
			}
		};
		let table = if mutating {
			client.update(&plan, None, &datums)
		} else {
			client.select(&plan, None, &datums)
		}
		.unwrap_or_else(|e| spi_failed(e));
		let rows = collect(client, table);
		PLANS.with(|c| c.borrow_mut().put(sql.to_string(), plan));
		rows
	})
}

/// The bytes of a `jsonb` datum made by `jsonb_datum`, to keep past the call that made it.
pub fn jsonb_bytes(datum: pg_sys::Datum) -> Vec<u8> {
	unsafe {
		let ptr = datum.cast_mut_ptr::<u8>();
		let len = pgrx::varsize_any(ptr.cast());
		std::slice::from_raw_parts(ptr, len).to_vec()
	}
}

/// A `jsonb` datum, in the current memory context, from bytes `jsonb_bytes` kept.
pub fn jsonb_from_bytes(bytes: &[u8]) -> pg_sys::Datum {
	unsafe {
		let ptr = pg_sys::palloc(bytes.len()).cast::<u8>();
		std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
		pg_sys::Datum::from(ptr)
	}
}

/// Runs `f` with every plan it uses forced generic, as a function's `SET plan_cache_mode` clause
/// would, and the setting put back afterwards (by the transaction's abort, if `f` raises). For the
/// catalogue queries: their parameters are lists of schema oids that never change which plan is
/// best, and left to choose, Postgres re-plans them on every schema read.
pub fn generic_plans<T>(f: impl FnOnce() -> T) -> T {
	let nest = unsafe { pg_sys::NewGUCNestLevel() };
	unsafe {
		pg_sys::set_config_option(
			c"plan_cache_mode".as_ptr(),
			c"force_generic_plan".as_ptr(),
			pg_sys::GucContext::PGC_USERSET,
			pg_sys::GucSource::PGC_S_SESSION,
			pg_sys::GucAction::GUC_ACTION_SAVE,
			true,
			0,
			false,
		);
	}
	let out = f();
	unsafe { pg_sys::AtEOXact_GUC(true, nest) };
	out
}

fn spi_failed(e: pgrx::spi::SpiError) -> ! {
	pgrx::error!("{e}")
}

/// Parse text as `jsonb`, the way a `jsonb` column's input does, and return the datum.
pub fn jsonb_datum(text: &str) -> pg_sys::Datum {
	let cstring = std::ffi::CString::new(text).unwrap_or_else(|_| {
		std::ffi::CString::new("{\"errors\": [{\"message\": \"response contained a NUL byte\"}]}")
			.expect("no NUL in a literal")
	});
	unsafe {
		pgrx::direct_function_call_as_datum(pg_sys::jsonb_in, &[cstring.as_c_str().into_datum()])
			.expect("jsonb_in returns a value for valid input")
	}
}
