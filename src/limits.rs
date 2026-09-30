//! How much a document may ask for, where a schema's directive sets it:
//! `{"limits": {"fields": 2000, "rows": 100000}}`. Upstream limits depth (32) and fragment
//! recursion only, so one document of a few hundred bytes can ask for a join of millions of rows.
//! Counted before any SQL exists, over the document as it will run (fragments spread, `@skip` and
//! `@include` applied), and refused with a sentence naming the limit.
//!
//! `fields` counts every field selected. `rows` counts, for every collection, the most rows it can
//! return: its page (`first`, `last`, or the table's `max_rows`) times the pages of the collections
//! it sits in, so `{ a(first: 10) { edges { node { b(first: 10) { ... } } } } }` is 10 + 100.
use crate::error::{Error, Result};
use crate::schema::{Schema, Source, TypeId};
use crate::select::{self, Fragments};
use graphql_parser::query::{Selection, SelectionSet, TypeCondition, Value};
use serde_json::Value as Json;

pub struct Limits {
	pub fields: Option<u64>,
	pub rows: Option<u64>,
}

impl Limits {
	/// The tightest of the limits the schemas on the search path set, if any sets one.
	pub fn of(schema: &Schema) -> Option<Limits> {
		let schemas = schema.catalog.schemas.values();
		let fields = schemas.clone().filter_map(|s| s.limit_fields).min();
		let rows = schemas.filter_map(|s| s.limit_rows).min();
		(fields.is_some() || rows.is_some()).then_some(Limits { fields, rows })
	}
}

struct Count<'l> {
	limits: &'l Limits,
	fields: u64,
	rows: u64,
}

pub fn check(
	schema: &Schema,
	limits: &Limits,
	set: &SelectionSet<'_, String>,
	root: TypeId,
	fragments: &Fragments<'_>,
	variables: &serde_json::Map<String, Json>,
) -> Result<()> {
	let mut count = Count {
		limits,
		fields: 0,
		rows: 0,
	};
	walk(schema, &mut count, set, root, 1, fragments, variables)
}

fn walk(
	schema: &Schema,
	count: &mut Count<'_>,
	set: &SelectionSet<'_, String>,
	ty: TypeId,
	pages: u64,
	fragments: &Fragments<'_>,
	variables: &serde_json::Map<String, Json>,
) -> Result<()> {
	for selection in &set.items {
		// A directive that is wrong is resolution's to report, in its own words.
		if select::skipped(selection, variables).unwrap_or(false) {
			continue;
		}
		match selection {
			Selection::Field(f) => {
				count.fields += 1;
				if let Some(max) = count.limits.fields
					&& count.fields > max
				{
					return Err(Error::new(format!(
						"The document selects more than {max} fields, the most this schema allows"
					)));
				}
				let Some(def) = schema.field(ty, &f.name) else {
					continue;
				};
				let inner = def.ty.base();
				let mut inner_pages = pages;
				if let Source::Connection(table) = &schema.ty(inner).source {
					let max_rows = schema
						.catalog
						.schema(table.schema_oid)
						.map(|s| table.max_rows.unwrap_or(s.max_rows))
						.unwrap_or(30);
					let page = ["first", "last"]
						.iter()
						.find_map(|name| {
							let (_, v) = f.arguments.iter().find(|(n, _)| n == name)?;
							match v {
								Value::Int(n) => n.as_i64(),
								Value::Variable(var) => variables.get(var).and_then(Json::as_i64),
								_ => None,
							}
						})
						.map(|n| (n.max(0) as u64).min(max_rows))
						.unwrap_or(max_rows);
					inner_pages = pages.saturating_mul(page);
					count.rows = count.rows.saturating_add(inner_pages);
					if let Some(max) = count.limits.rows
						&& count.rows > max
					{
						return Err(Error::new(format!(
							"The document could read more than {max} rows, the most this schema allows (each collection counts its page, times the pages of the collections around it)"
						)));
					}
				}
				walk(
					schema,
					count,
					&f.selection_set,
					inner,
					inner_pages,
					fragments,
					variables,
				)?;
			}
			Selection::FragmentSpread(s) => {
				if let Some(d) = fragments.iter().find(|d| d.name == s.fragment_name) {
					let TypeCondition::On(on) = &d.type_condition;
					let ty = schema.lookup(on).unwrap_or(ty);
					walk(
						schema,
						count,
						&d.selection_set,
						ty,
						pages,
						fragments,
						variables,
					)?;
				}
			}
			Selection::InlineFragment(i) => {
				let ty = match &i.type_condition {
					Some(TypeCondition::On(on)) => schema.lookup(on).unwrap_or(ty),
					None => ty,
				};
				walk(
					schema,
					count,
					&i.selection_set,
					ty,
					pages,
					fragments,
					variables,
				)?;
			}
		}
	}
	Ok(())
}
