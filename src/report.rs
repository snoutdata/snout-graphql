//! The schema report: every table, view and function in the schemas on the search path, whether
//! GraphQL shows it, and if not, why. The most common question about a reflected schema is "why is
//! my table not there", and every answer to it is a rule in `schema.rs`; this says which one.
//!
//! Asked for with `extensions: {"schemaReport": true}`, where a schema's directive allows it
//! (`Extras::schema_report`). It names objects the caller cannot read, so it is for development.
use crate::catalog::{Table, Volatility};
use crate::schema::{FieldKind, Schema, is_valid_name};
use serde_json::{Value as Json, json};

pub fn report(schema: &Schema) -> Json {
	let tables: Vec<Json> = schema
		.catalog
		.tables
		.iter()
		.map(|t| table(schema, t))
		.collect();
	let functions: Vec<Json> = schema
		.catalog
		.functions
		.iter()
		.map(|f| {
			let computed = f.args.len() == 1
				&& schema
					.catalog
					.types
					.get(&f.args[0].type_oid)
					.and_then(|t| t.table)
					.is_some();
			let reason = if computed {
				(!f.executable).then(|| "the role may not execute it".to_string())
			} else {
				schema.function_reason(f)
			};
			let place = match f.volatility {
				Volatility::Volatile => "Mutation",
				_ => "Query",
			};
			json!({
				"schema": f.schema_name,
				"name": f.name,
				"reflected": reason.is_none(),
				"as": if computed {
					format!("a field of the type of {}", f.args[0].type_name)
				} else {
					format!("a field of {place}")
				},
				"reason": reason,
			})
		})
		.collect();
	json!({ "tables": tables, "functions": functions })
}

fn table(schema: &Schema, t: &Table) -> Json {
	let name = schema.table_name(t);
	let reason = if !t.selectable {
		Some("the role has no SELECT privilege on it".to_string())
	} else if t.primary_key().is_none() {
		Some(match &t.primary_key_directive {
			Some(cols) => format!(
				"its primary_key_columns directive names {cols:?}, and it has no such column the role can read"
			),
			None => "it has no primary key (a view or a foreign table needs a primary_key_columns directive)".into(),
		})
	} else if !is_valid_name(&name) {
		Some(format!(
			"its type name, {name:?}, is not a valid GraphQL name"
		))
	} else if !t.columns.iter().any(|c| c.selectable) {
		Some("the role may read none of its columns".into())
	} else {
		None
	};
	let listed = schema.table_listed(t);
	let columns: Vec<Json> = t
		.columns
		.iter()
		.filter_map(|c| {
			let why = if !c.selectable {
				"the role has no SELECT privilege on it".to_string()
			} else if schema.column_type(c).is_none() {
				format!("its type, {}, is not one GraphQL can read", c.type_name)
			} else if !is_valid_name(&schema.column_name(c)) {
				format!(
					"its field name, {:?}, is not a valid GraphQL name",
					schema.column_name(c)
				)
			} else {
				return None;
			};
			Some(json!({ "name": c.name, "reason": why }))
		})
		.collect();
	let roots = [Some(schema.query), schema.mutation_type()];
	let offers: Vec<&str> = if listed {
		roots
			.iter()
			.flatten()
			.flat_map(|id| schema.fields(*id).unwrap_or(&[]))
			.filter_map(|f| match &f.kind {
				FieldKind::Collection(x)
				| FieldKind::ByPk(x)
				| FieldKind::Insert(x)
				| FieldKind::Update(x)
				| FieldKind::Delete(x)
					if x.oid == t.oid =>
				{
					Some(f.name.as_str())
				}
				_ => None,
			})
			.collect()
	} else {
		vec![]
	};
	// Two fields of one name (two foreign keys between the same tables name their reverse sides
	// alike, upstream #502): a client sees one of them.
	let mut collisions: Vec<Json> = vec![];
	if listed {
		let fields = schema.fields(schema.node_type(t)).unwrap_or(&[]);
		let mut seen: Vec<&str> = vec![];
		for f in fields {
			let n = f.name.as_str();
			if seen.contains(&n) {
				continue;
			}
			seen.push(n);
			let count = fields.iter().filter(|g| g.name == n).count();
			if count > 1 {
				collisions.push(json!({
					"field": n,
					"reason": format!(
						"{count} fields are named {n}; name each relation with foreign_name and local_name in a comment on its foreign key"
					),
				}));
			}
		}
	}
	json!({
		"schema": t.schema,
		"name": t.name,
		"type": listed.then_some(name),
		"collisions": collisions,
		"reflected": listed,
		"reason": if listed { None } else { reason.or(Some("not reflected".into())) },
		"fields": offers,
		"omittedColumns": columns,
	})
}
