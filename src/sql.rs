//! Plans as SQL. Each root field is one statement whose single value is the field's JSON.
//!
//! The text of a statement depends only on the document's shape: blocks are named by position and
//! every value a client sends is a parameter. So the same document compiles to the same text every
//! time, and its plan is prepared once per connection and reused (`pg::run`).
use crate::catalog::{Column, ForeignKey, Table};
use crate::codec::NodeId;
use crate::error::{Error, Result};
use crate::pg::Arg;
use crate::plan::*;
use serde_json::Value as Json;

pub struct Sql {
	pub params: Vec<Arg>,
	blocks: usize,
}

pub fn ident(s: &str) -> String {
	format!("\"{}\"", s.replace('"', "\"\""))
}

/// A string literal, as `quote_literal` writes one.
pub fn lit(s: &str) -> String {
	let body = s.replace('\'', "''");
	if s.contains('\\') {
		format!("E'{}'", body.replace('\\', "\\\\"))
	} else {
		format!("'{body}'")
	}
}

fn qualified(schema: &str, name: &str) -> String {
	format!("{}.{}", ident(schema), ident(name))
}

/// The schema PostGIS is in, when the column is a `geometry` or `geography` read as GeoJSON.
fn geo_schema(enums: &Enums, column: &Column) -> Option<String> {
	let ps = enums.0.postgis_schema.as_ref()?;
	enums
		.0
		.types
		.get(&column.type_oid)
		.is_some_and(|t| matches!(t.name.as_str(), "geometry" | "geography"))
		.then(|| ps.clone())
}

/// The cast that makes a value come back as its GraphQL scalar says: big numbers and JSON as
/// strings.
fn output_cast(type_oid: u32) -> &'static str {
	match type_oid {
		20 | 1700 => "::text",
		114 | 3802 => " #>> '{}'",
		1016 | 199 | 3807 | 1231 => "::text[]",
		_ => "",
	}
}

fn json_text(v: &Json) -> Result<Option<String>> {
	Ok(match v {
		Json::Null => None,
		Json::Bool(b) => Some(b.to_string()),
		Json::String(s) => Some(s.clone()),
		Json::Number(n) => Some(n.to_string()),
		Json::Array(_) | Json::Object(_) => {
			return Err(Error::new("Unexpected object in input value"));
		}
	})
}

impl Default for Sql {
	fn default() -> Self {
		Self::new()
	}
}

impl Sql {
	pub fn new() -> Sql {
		Sql {
			params: vec![],
			blocks: 0,
		}
	}

	fn block(&mut self) -> String {
		self.blocks += 1;
		format!("b{}", self.blocks)
	}

	/// A value as a parameter, cast to the column's type.
	///
	/// The parameter's own type (text, or text[] for a list) is written into the statement, so a
	/// statement's text fixes its parameters' types. That matters because plans are cached by
	/// text: a plan prepared for a `text[]` parameter must never be handed a `text` one.
	pub fn param(&mut self, value: &Json, type_name: &str) -> Result<String> {
		let n = self.params.len() + 1;
		let (arg, expr) = match value {
			Json::Array(items) if type_name.ends_with("[]") => {
				let mut out = vec![];
				for item in items {
					out.push(match item {
						Json::Array(_) => {
							return Err(Error::new("Unexpected array in input value array"));
						}
						Json::Object(_) => {
							return Err(Error::new("Unexpected object in input value array"));
						}
						other => json_text(other)?,
					});
				}
				(Arg::TextArray(out), format!("(${n}::text[])::{type_name}"))
			}
			Json::Array(_) => return Err(Error::new("Unexpected array in input value")),
			// A single value, or an array literal as text (`{a,b}`) that the cast reads.
			other => (
				Arg::Text(json_text(other)?),
				format!("(${n}::text)::{type_name}"),
			),
		};
		self.params.push(arg);
		Ok(format!("({expr})"))
	}

	// ----- pieces -----------------------------------------------------------------------------

	/// A column's value for the response: enum labels mapped to their GraphQL names.
	fn column_expr(&self, block: &str, column: &Column, enums: &Enums) -> String {
		let col = format!("{block}.{}", ident(&column.name));
		match enums.mappings(column.type_oid) {
			Some(mappings) if !mappings.is_empty() => {
				let mut cases: Vec<(String, String)> = mappings.to_vec();
				cases.sort();
				let whens: Vec<String> = cases
					.iter()
					.map(|(db, gql)| format!("when {col} = {} then {}", lit(db), lit(gql)))
					.collect();
				format!("case {} else {col}::text end", whens.join(" "))
			}
			_ => col,
		}
	}

	/// The value an order key reads for the row in `block`. A related key is a subquery, the same
	/// text every time it is asked for, so a document's statement stays one text.
	fn order_expr(&self, block: &str, key: &OrderKey) -> String {
		match key {
			OrderKey::Column(c) => format!("{block}.{}", ident(&c.name)),
			OrderKey::Related {
				key,
				reverse,
				table,
				column,
			} => format!(
				"(select __related.{} from {} as __related where {})",
				ident(&column.name),
				qualified(&table.schema, &table.name),
				self.join(key, *reverse, "__related", block)
			),
			OrderKey::Count { key, table } => format!(
				"(select count(*) from {} as __related where {})",
				qualified(&table.schema, &table.name),
				self.join(key, true, "__related", block)
			),
		}
	}

	fn cursor_expr(&self, block: &str, order: &[Order]) -> String {
		let parts: Vec<String> = order
			.iter()
			.map(|o| format!("to_jsonb({})", self.order_expr(block, &o.key)))
			.collect();
		format!(
			"translate(encode(convert_to(jsonb_build_array({})::text, 'utf-8'), 'base64'), E'\\n', '')",
			parts.join(", ")
		)
	}

	fn order_clause(&self, block: &str, order: &[Order]) -> String {
		order
			.iter()
			.map(|o| format!("{} {}", self.order_expr(block, &o.key), o.direction.sql()))
			.collect::<Vec<_>>()
			.join(", ")
	}

	fn primary_key_tuple(&self, block: &str, table: &Table) -> String {
		let parts: Vec<String> = table
			.primary_key_columns()
			.iter()
			.map(|c| format!("{block}.{}", ident(&c.name)))
			.collect();
		format!("({})", parts.join(","))
	}

	fn selectable_columns(&self, table: &Table) -> String {
		table
			.columns
			.iter()
			.filter(|c| c.selectable)
			.map(|c| ident(&c.name))
			.collect::<Vec<_>>()
			.join(", ")
	}

	fn join(&self, key: &ForeignKey, reverse: bool, block: &str, parent: &str) -> String {
		let (mine, theirs) = if reverse {
			(&key.local, &key.referenced)
		} else {
			(&key.referenced, &key.local)
		};
		let mut clauses = vec!["true".to_string()];
		for (a, b) in mine.columns.iter().zip(theirs.columns.iter()) {
			clauses.push(format!("{block}.{} = {parent}.{}", ident(a), ident(b)));
		}
		clauses.join(" and ")
	}

	fn pagination(&mut self, block: &str, order: &[Order], cursor: &[Json]) -> Result<String> {
		let Some((value, rest_cursor)) = cursor.split_first() else {
			return Ok("false".into());
		};
		let Some((elem, rest_order)) = order.split_first() else {
			return Err(Error::new(
				"orderBy clause incompatible with pagination cursor",
			));
		};
		let col = self.order_expr(block, &elem.key);
		let v = self.param(value, elem.key.type_name())?;
		let rest = self.pagination(block, rest_order, rest_cursor)?;
		let op = if elem.direction.asc() { ">" } else { "<" };
		let nulls_first = elem.direction.nulls_first();
		Ok(format!(
			"(({col} {op} {v} or ({col} is not null and {v} is null and {nulls_first})) or (({col} = {v} or ({col} is null and {v} is null)) and {rest}))"
		))
	}

	fn node_id_expr(&self, block: &str, table: &Table) -> String {
		let cols: Vec<String> = table
			.primary_key_columns()
			.iter()
			.map(|c| format!("{block}.{}", ident(&c.name)))
			.collect();
		format!(
			"translate(encode(convert_to(jsonb_build_array({}, {}, {})::text, 'utf-8'), 'base64'), E'\\n', '')",
			lit(&table.schema),
			lit(&table.name),
			cols.join(", ")
		)
	}

	fn node_id_match(&mut self, id: &NodeId, block: &str, table: &Table) -> Result<String> {
		if (id.schema.as_str(), id.table.as_str()) != (table.schema.as_str(), table.name.as_str()) {
			return Err(Error::new("nodeId belongs to a different collection"));
		}
		let pk = table
			.primary_key()
			.ok_or_else(|| Error::new("Found table with no primary key"))?;
		if pk.len() != id.values.len() {
			return Err(Error::new(format!(
				"Primary key column count mismatch. Expected {}, provided {}",
				pk.len(),
				id.values.len()
			)));
		}
		let mut conditions = vec![];
		for (name, value) in pk.iter().zip(&id.values) {
			let column = table
				.column(name)
				.ok_or_else(|| Error::new(format!("Primary key column {name} not found")))?;
			let v = self.param(value, &column.type_name)?;
			conditions.push(format!("{block}.{} = {v}", ident(name)));
		}
		Ok(conditions.join(" AND "))
	}

	/// `expr op value`, the value cast to `type_name` (an array of it for `in` and the array
	/// operators).
	fn comparison(&mut self, col: &str, type_name: &str, op: &str, value: &Json) -> Result<String> {
		Ok(if op == "is" {
			let check = match value {
				Json::String(s) if s == "NULL" => "is null",
				Json::String(s) if s == "NOT_NULL" => "is not null",
				Json::String(_) => {
					return Err(Error::new("Error transpiling Is filter value"));
				}
				_ => return Err(Error::new("Error transpiling Is filter value type")),
			};
			format!("{col} {check}")
		} else {
			let array_op = matches!(op, "in" | "contains" | "containedBy" | "overlaps");
			let cast = if array_op {
				format!("{type_name}[]")
			} else {
				type_name.to_string()
			};
			let v = self.param(value, &cast)?;
			let sql_op = match op {
				"eq" => "=",
				"neq" => "<>",
				"lt" => "<",
				"lte" => "<=",
				"gt" => ">",
				"gte" => ">=",
				"in" => "= any",
				"startsWith" => "^@",
				"like" => "like",
				"ilike" => "ilike",
				"regex" => "~",
				"iregex" => "~*",
				"contains" => "@>",
				"containedBy" => "<@",
				"overlaps" => "&&",
				other => {
					return Err(Error::new(format!("Invalid filter operation: {other}")));
				}
			};
			format!("{col} {sql_op} {v}")
		})
	}

	fn filter(&mut self, filter: &Filter, block: &str, table: &Table) -> Result<String> {
		Ok(match filter {
			Filter::Column { column, op, value } => {
				let col = format!("{block}.{}", ident(&column.name));
				self.comparison(&col, &column.type_name, op, value)?
			}
			Filter::NodeId(id) => self.node_id_match(id, block, table)?,
			Filter::Geo {
				column,
				op,
				value,
				postgis,
				geography,
			} => {
				let col = format!("{block}.{}", ident(&column.name));
				let ps = ident(postgis);
				let (geojson, distance) = match value {
					Json::Object(m) => (
						m.get("geometry").cloned().unwrap_or(Json::Null),
						m.get("distance").cloned(),
					),
					other => (other.clone(), None),
				};
				let g = self.param(&geojson, "text")?;
				let g = if *geography {
					format!("{ps}.st_geomfromgeojson({g})::{ps}.geography")
				} else {
					format!("{ps}.st_geomfromgeojson({g})")
				};
				match (op.as_str(), geography) {
					("intersects", _) => format!("{ps}.st_intersects({col}, {g})"),
					("contains", false) => format!("{ps}.st_contains({col}, {g})"),
					("contains", true) => format!("{ps}.st_covers({col}, {g})"),
					("within", false) => format!("{ps}.st_within({col}, {g})"),
					("within", true) => format!("{ps}.st_coveredby({col}, {g})"),
					("dWithin", _) => {
						let d = self.param(&distance.unwrap_or(Json::Null), "float8")?;
						format!("{ps}.st_dwithin({col}, {g}, {d})")
					}
					(other, _) => {
						return Err(Error::new(format!("Invalid filter operation: {other}")));
					}
				}
			}
			Filter::Attribute {
				column,
				attr,
				op,
				value,
			} => {
				let expr = format!("({block}.{}).{}", ident(&column.name), ident(&attr.name));
				self.comparison(&expr, &attr.type_name, op, value)?
			}
			Filter::Computed {
				function,
				table: of,
				op,
				value,
			} => {
				let call = format!(
					"{}({block}::{})",
					qualified(&function.schema_name, &function.name),
					qualified(&of.schema, &of.name)
				);
				self.comparison(&call, &function.return_type_name, op, value)?
			}
			// An empty group constrains nothing.
			Filter::And(items) if items.is_empty() => "true".into(),
			Filter::Or(items) if items.is_empty() => "true".into(),
			Filter::And(items) => {
				let parts = items
					.iter()
					.map(|f| self.filter(f, block, table))
					.collect::<Result<Vec<_>>>()?;
				format!("({})", parts.join(" and "))
			}
			Filter::Or(items) => {
				let parts = items
					.iter()
					.map(|f| self.filter(f, block, table))
					.collect::<Result<Vec<_>>>()?;
				format!("({})", parts.join(" or "))
			}
			Filter::Not(inner) => format!("not({})", self.filter(inner, block, table)?),
			Filter::Related {
				key,
				reverse,
				table: other,
				quantifier,
				inner,
			} => {
				let related = self.block();
				let join = self.join(key, *reverse, &related, block);
				let matches = self.where_clause(inner, &related, other)?;
				let from = qualified(&other.schema, &other.name);
				match quantifier {
					Quantifier::Some => {
						format!(
							"exists (select 1 from {from} as {related} where {join} and {matches})"
						)
					}
					Quantifier::None => {
						format!(
							"not exists (select 1 from {from} as {related} where {join} and {matches})"
						)
					}
					// A row whose condition is null does not match, as `where` has it.
					Quantifier::Every => format!(
						"not exists (select 1 from {from} as {related} where {join} and not coalesce({matches}, false))"
					),
				}
			}
		})
	}

	fn where_clause(&mut self, filters: &[Filter], block: &str, table: &Table) -> Result<String> {
		let mut parts = vec!["true".to_string()];
		for f in filters {
			parts.push(self.filter(f, block, table)?);
		}
		Ok(parts.join(" and "))
	}

	// ----- nodes ------------------------------------------------------------------------------

	/// The pairs of a node's JSON object, `'key', value`.
	fn node_pairs(&mut self, node: &[NodeSel], block: &str, enums: &Enums) -> Result<Vec<String>> {
		let mut out = vec![];
		for sel in node {
			out.push(match sel {
				NodeSel::Column { alias, column } if geo_schema(enums, column).is_some() => {
					let ps = geo_schema(enums, column).unwrap_or_default();
					format!(
						"{}, {}.st_asgeojson({block}.{})::jsonb",
						lit(alias),
						ident(&ps),
						ident(&column.name)
					)
				}
				NodeSel::Column { alias, column } => {
					format!(
						"{}, {}{}",
						lit(alias),
						self.column_expr(block, column, enums),
						output_cast(column.type_oid)
					)
				}
				NodeSel::CompositeColumn {
					alias,
					column,
					fields,
				} => {
					let expr = format!("{block}.{}", ident(&column.name));
					format!("{}, {}", lit(alias), self.composite(&expr, fields, enums))
				}
				NodeSel::NodeId { alias, table } => {
					format!("{}, {}", lit(alias), self.node_id_expr(block, table))
				}
				NodeSel::Typename { alias, name } => format!("{}, {}", lit(alias), lit(name)),
				NodeSel::Related(node) => format!(
					"{}, {}",
					lit(&node.alias),
					self.related(node, block, enums)?
				),
				NodeSel::RelatedMany(conn) => {
					format!(
						"{}, {}",
						lit(&conn.alias),
						self.connection(conn, Some(block), enums)?
					)
				}
				NodeSel::Computed {
					alias,
					function,
					table,
					returns,
					args,
				} => {
					let row = format!("{block}::{}", qualified(&table.schema, &table.name));
					let call = format!(
						"{}({})",
						qualified(&function.schema_name, &function.name),
						self.call_args(args, Some(row))?
					);
					let expr = match returns {
						Computed::Scalar if function.shapes => {
							self.mapped(call, function.return_type, enums)
						}
						Computed::Scalar | Computed::Array => call,
						Computed::Composite(fields) => self.composite(&call, fields, enums),
						Computed::Node(node) => {
							let inner = self.block();
							let obj = self.object(&node.selections, &inner, enums)?;
							format!(
								"(select {obj} from {call} as {inner} where not ({inner} is null))"
							)
						}
						Computed::Connection(conn) => self.connection(conn, Some(block), enums)?,
					};
					format!(
						"{}, {expr}{}",
						lit(alias),
						output_cast(function.return_type)
					)
				}
			});
		}
		Ok(out)
	}

	/// A node's JSON object. `jsonb_build_object` takes at most 100 arguments, so a wide selection
	/// is built in pieces of 50 pairs and concatenated.
	fn object(&mut self, node: &[NodeSel], block: &str, enums: &Enums) -> Result<String> {
		let pairs = self.node_pairs(node, block, enums)?;
		if pairs.is_empty() {
			return Ok("jsonb_build_object()".into());
		}
		Ok(pairs
			.chunks(50)
			.map(|c| format!("jsonb_build_object({})", c.join(", ")))
			.collect::<Vec<_>>()
			.join(" || "))
	}

	fn related(&mut self, node: &Node, parent: &str, enums: &Enums) -> Result<String> {
		let block = self.block();
		let obj = self.object(&node.selections, &block, enums)?;
		let (key, reverse) = node
			.key
			.as_ref()
			.ok_or_else(|| Error::new("Internal Error: relation key"))?;
		let join = self.join(key, *reverse, &block, parent);
		Ok(format!(
			"(select {obj} from {} as {block} where {join})",
			qualified(&node.table.schema, &node.table.name)
		))
	}

	pub fn node_entry(&mut self, node: &Node, enums: &Enums) -> Result<String> {
		let block = self.block();
		let obj = self.object(&node.selections, &block, enums)?;
		let condition = match &node.node_id {
			Some(id) => self.node_id_match(id, &block, &node.table)?,
			None => "true".into(),
		};
		Ok(format!(
			"select (select {obj} from {} as {block} where {condition})::text",
			qualified(&node.table.schema, &node.table.name)
		))
	}

	pub fn by_pk(&mut self, plan: &ByPk, enums: &Enums) -> Result<String> {
		let block = self.block();
		let obj = self.object(&plan.selections, &block, enums)?;
		let mut conditions = vec![];
		for (column, value) in &plan.keys {
			let v = self.param(value, &column.type_name)?;
			conditions.push(format!("{block}.{} = {v}", ident(&column.name)));
		}
		Ok(format!(
			"select (select {obj} from {} as {block} where {})::text",
			qualified(&plan.table.schema, &plan.table.name),
			conditions.join(" AND ")
		))
	}

	// ----- connections ------------------------------------------------------------------------

	pub fn root_connection(&mut self, conn: &Connection, enums: &Enums) -> Result<String> {
		Ok(format!(
			"select {}::text",
			self.connection(conn, None, enums)?
		))
	}

	fn connection(
		&mut self,
		conn: &Connection,
		parent: Option<&str>,
		enums: &Enums,
	) -> Result<String> {
		let block = self.block();
		let table = &conn.table;
		let from = match &conn.rows {
			Rows::Table | Rows::Related { .. } => {
				format!("{} {block}", qualified(&table.schema, &table.name))
			}
			Rows::RowFunction {
				function,
				input,
				args,
			} => {
				let row = format!(
					"{}::{}",
					parent.unwrap_or("null"),
					qualified(&input.schema, &input.name)
				);
				format!(
					"{}({}) {block}",
					qualified(&function.schema_name, &function.name),
					self.call_args(args, Some(row))?
				)
			}
			Rows::Call(call) => format!("{} {block}", self.call(call)?),
		};
		let join = match &conn.rows {
			Rows::Related { key, reverse } => {
				let parent = parent.ok_or_else(|| {
					Error::new("Internal Error: Parent block name is required when fkey_ix is set")
				})?;
				self.join(key, *reverse, &block, parent)
			}
			_ => "true".into(),
		};
		let filter = self.where_clause(&conn.filter, &block, table)?;
		// `distinctOn`: one row per distinct value, the first in the collection's own order,
		// chosen among the rows the filter keeps; the page is then taken from those.
		let from = if conn.distinct.is_empty() {
			from
		} else {
			let keys: Vec<String> = conn
				.distinct
				.iter()
				.map(|c| format!("{block}.{}", ident(&c.name)))
				.collect();
			let keys = keys.join(", ");
			format!(
				"(select distinct on ({keys}) {block}.* from {from} where {join} and {filter} order by {keys}, {}) {block}",
				self.order_clause(&block, &conn.order)
			)
		};
		let forward = self.order_clause(&block, &conn.order);
		let reverse_order = reversed(&conn.order);
		let backward = self.order_clause(&block, &reverse_order);
		let records_order = if conn.reverse() { &backward } else { &forward };
		let cursor = conn.before.as_ref().or(conn.after.as_ref());
		let paging_order = if conn.reverse() {
			reverse_order.clone()
		} else {
			conn.order.clone()
		};
		let paging = match cursor {
			Some(c) => self.pagination(&block, &paging_order, c)?,
			None => "true".into(),
		};
		let limit = conn.limit();
		let offset = conn.offset.unwrap_or(0);

		let object = self.connection_object(conn, &block, enums)?;
		let aggregate = self.aggregates(conn, &block, enums)?;

		let columns = self.selectable_columns(table);
		let pk_block = self.primary_key_tuple(&block, table);
		let pk_records = self.primary_key_tuple("__records", table);

		let wants_next = conn
			.page_selections()
			.any(|p| matches!(p, PageSel::HasNextPage { .. }));
		let wants_previous = conn
			.page_selections()
			.any(|p| matches!(p, PageSel::HasPreviousPage { .. }));
		let wants_total = conn
			.selections
			.iter()
			.any(|s| matches!(s, ConnSel::TotalCount { .. }));

		let mut next = format!(
			"with page_plus_1 as (select 1 from {from} where {join} and {filter} and {paging} order by {forward} limit ({limit} + 1) offset ({offset})) select count(*) > {limit} from page_plus_1"
		);
		let mut previous = format!(
			"with page_minus_1 as (select not ({pk_block} = any(__records.seen)) is_pkey_in_records from {from} left join (select array_agg({pk_records}) from __records) __records(seen) on true where {join} and {filter} order by {records_order} limit 1) select coalesce(bool_and(is_pkey_in_records), false) from page_minus_1"
		);
		if conn.reverse() {
			std::mem::swap(&mut next, &mut previous);
		}
		if !wants_next {
			next = "select null::bool".into();
		}
		if !wants_previous {
			previous = "select null::bool".into();
		}
		let total = if wants_total {
			format!("select count(*) from {from} where {join} and {filter}")
		} else {
			"select null::int8".into()
		};
		let aggregate_cte = match &aggregate {
			Some((_, list)) => format!(
				"__aggregates(agg_result) as (select jsonb_build_object({list}) from {from} where {join} and {filter})"
			),
			None => "__aggregates(agg_result) as (select null::jsonb)".into(),
		};
		let merge = match &aggregate {
			Some((alias, _)) => format!(
				" || jsonb_build_object({}, coalesce(__aggregates.agg_result, '{{}}'::jsonb))",
				lit(alias)
			),
			None => String::new(),
		};
		Ok(format!(
			"(with __records as (select {columns} from {from} where true and {join} and {filter} and {paging} order by {records_order} limit {limit} offset {offset}), \
			__total_count(___total_count) as ({total}), \
			__has_next_page(___has_next_page) as ({next}), \
			__has_previous_page(___has_previous_page) as ({previous}), \
			__has_records(has_records) as (select exists(select 1 from __records)), \
			{aggregate_cte}, \
			__base_object as (select {object} as obj from __total_count cross join __has_next_page cross join __has_previous_page cross join __has_records left join __records {block} on true group by __total_count.___total_count, __has_next_page.___has_next_page, __has_previous_page.___has_previous_page, __has_records.has_records) \
			select coalesce(__base_object.obj, '{{}}'::jsonb){merge} from (select 1) as __dummy_for_left_join left join __base_object on true cross join __aggregates)"
		))
	}

	fn connection_object(
		&mut self,
		conn: &Connection,
		block: &str,
		enums: &Enums,
	) -> Result<String> {
		let mut pairs = vec![];
		for sel in &conn.selections {
			match sel {
				ConnSel::Edges { alias, selections } => {
					let mut edge_pairs = vec![];
					for e in selections {
						edge_pairs.push(match e {
							EdgeSel::Cursor { alias } => {
								format!("{}, {}", lit(alias), self.cursor_expr(block, &conn.order))
							}
							EdgeSel::Node(node) => {
								format!(
									"{}, {}",
									lit(&node.alias),
									self.object(&node.selections, block, enums)?
								)
							}
							EdgeSel::Typename { alias, name } => {
								format!("{}, {}", lit(alias), lit(name))
							}
						});
					}
					let order = self.order_clause(block, &conn.order);
					let not_null = match conn.table.primary_key_columns().first() {
						Some(pk) => {
							format!(" filter (where {block}.{} is not null)", ident(&pk.name))
						}
						None => String::new(),
					};
					pairs.push(format!(
						"{}, coalesce(jsonb_agg(jsonb_build_object({}) order by {order}){not_null}, jsonb_build_array())",
						lit(alias),
						edge_pairs.join(", ")
					));
				}
				ConnSel::PageInfo { alias, selections } => {
					let forward = self.order_clause(block, &conn.order);
					let backward = self.order_clause(block, &reversed(&conn.order));
					let cursor = self.cursor_expr(block, &conn.order);
					let mut page = vec![];
					for p in selections {
						page.push(match p {
							PageSel::StartCursor { alias } => format!(
								"{}, case when __has_records.has_records then (array_agg({cursor} order by {forward}))[1] else null end",
								lit(alias)
							),
							PageSel::EndCursor { alias } => format!(
								"{}, case when __has_records.has_records then (array_agg({cursor} order by {backward}))[1] else null end",
								lit(alias)
							),
							PageSel::HasNextPage { alias } => {
								format!("{}, coalesce(bool_and(__has_next_page.___has_next_page), false)", lit(alias))
							}
							PageSel::HasPreviousPage { alias } => {
								format!("{}, coalesce(bool_and(__has_previous_page.___has_previous_page), false)", lit(alias))
							}
							PageSel::Typename { alias, name } => format!("{}, {}", lit(alias), lit(name)),
						});
					}
					pairs.push(format!(
						"{}, jsonb_build_object({})",
						lit(alias),
						page.join(", ")
					));
				}
				ConnSel::TotalCount { alias } => pairs.push(format!(
					"{}, coalesce(__total_count.___total_count, 0)",
					lit(alias)
				)),
				ConnSel::Typename { alias, name } => {
					pairs.push(format!("{}, {}", lit(alias), lit(name)))
				}
				ConnSel::Aggregate { .. } => {}
			}
		}
		if pairs.is_empty() {
			return Ok("jsonb_build_object()".into());
		}
		Ok(pairs
			.chunks(50)
			.map(|c| format!("jsonb_build_object({})", c.join(", ")))
			.collect::<Vec<_>>()
			.join(" || "))
	}

	/// The aggregate selection's alias and `jsonb_build_object` arguments, if one was asked for.
	fn aggregates(
		&mut self,
		conn: &Connection,
		block: &str,
		enums: &Enums,
	) -> Result<Option<(String, String)>> {
		let Some((alias, selections)) = conn.selections.iter().find_map(|s| match s {
			ConnSel::Aggregate { alias, selections } => Some((alias, selections)),
			_ => None,
		}) else {
			return Ok(None);
		};
		let mut parts = vec![];
		for sel in selections {
			parts.push(match sel {
				AggSel::Count { alias } => format!("{}, count(*)", lit(alias)),
				AggSel::Op { alias, op, columns } => {
					let fields: Vec<String> = columns
						.iter()
						.map(|(col_alias, column)| {
							let expr = self.column_expr(block, column, enums);
							let expr = if op.sql() == "avg" {
								format!("{expr}::numeric")
							} else {
								expr
							};
							format!("{}, {}({expr})", lit(col_alias), op.sql())
						})
						.collect();
					format!("{}, jsonb_build_object({})", lit(alias), fields.join(", "))
				}
				AggSel::Typename { alias, name } => format!("{}, {}", lit(alias), lit(name)),
			});
		}
		if parts.is_empty() {
			return Ok(None);
		}
		Ok(Some((alias.clone(), parts.join(", "))))
	}

	fn call(&mut self, call: &Call) -> Result<String> {
		let args = self.call_args(&call.args, None)?;
		Ok(format!(
			"{}({args})",
			qualified(&call.function.schema_name, &call.function.name),
		))
	}

	/// A call's argument list: the row first where a computed field is called, then each argument
	/// by name, or all by position when one has no name (so none may be left out before the last
	/// one given).
	fn call_args(&mut self, args: &[CallArg], row: Option<String>) -> Result<String> {
		let mut out: Vec<String> = row.into_iter().collect();
		let first = out.len();
		let mut sorted: Vec<&CallArg> = args.iter().collect();
		sorted.sort_by_key(|a| a.position);
		let positional = sorted.iter().any(|a| a.name.is_none());
		for (i, arg) in sorted.iter().enumerate() {
			let v = self.param(&arg.value, &arg.type_name)?;
			if positional {
				if arg.position != first + i {
					return Err(Error::new(format!(
						"arg{} must be given, since a later argument is",
						first + i + 1
					)));
				}
				out.push(v);
			} else {
				out.push(format!(
					"{} => {v}",
					ident(arg.name.as_deref().unwrap_or_default())
				));
			}
		}
		Ok(out.join(", "))
	}

	/// A value written to a column: cast to its type, or read as GeoJSON for a PostGIS column.
	fn write_value(&mut self, column: &Column, value: &Json, enums: &Enums) -> Result<String> {
		match geo_schema(enums, column) {
			Some(ps) if !value.is_null() => {
				let v = self.param(value, "text")?;
				Ok(format!(
					"{}.st_geomfromgeojson({v})::{}",
					ident(&ps),
					column.type_name
				))
			}
			_ => self.param(value, &column.type_name),
		}
	}

	/// A composite value as the object its selection asks for, or null when it is null.
	fn composite(&self, expr: &str, fields: &[AttrSel], enums: &Enums) -> String {
		let mut pairs = vec![];
		for f in fields {
			pairs.push(match f {
				AttrSel::Typename { alias, name } => format!("{}, {}", lit(alias), lit(name)),
				AttrSel::Attr {
					alias,
					attr,
					fields: None,
				} => {
					let value = self.mapped(
						format!("({expr}).{}", ident(&attr.name)),
						attr.type_oid,
						enums,
					);
					format!("{}, {value}{}", lit(alias), output_cast(attr.type_oid))
				}
				AttrSel::Attr {
					alias,
					attr,
					fields: Some(inner),
				} => {
					let nested =
						self.composite(&format!("({expr}).{}", ident(&attr.name)), inner, enums);
					format!("{}, {nested}", lit(alias))
				}
			});
		}
		format!(
			"case when to_jsonb({expr}) is null then null else jsonb_build_object({}) end",
			pairs.join(", ")
		)
	}

	/// A value for the response: an enum's labels mapped to their GraphQL names.
	fn mapped(&self, expr: String, type_oid: u32, enums: &Enums) -> String {
		match enums.mappings(type_oid) {
			Some(mappings) if !mappings.is_empty() => {
				let mut cases: Vec<(String, String)> = mappings.to_vec();
				cases.sort();
				let whens: Vec<String> = cases
					.iter()
					.map(|(db, gql)| format!("when {expr} = {} then {}", lit(db), lit(gql)))
					.collect();
				format!("case {} else {expr}::text end", whens.join(" "))
			}
			_ => expr,
		}
	}

	pub fn function_call(&mut self, plan: &FunctionCall, enums: &Enums) -> Result<String> {
		match &plan.returns {
			CallReturns::Scalar => {
				let call = self.call(&plan.call)?;
				let call = if plan.call.function.shapes {
					self.mapped(call, plan.call.function.return_type, enums)
				} else {
					call
				};
				Ok(format!(
					"select to_jsonb({call}{})::text",
					output_cast(plan.call.function.return_type)
				))
			}
			CallReturns::Node(node) => {
				let call = self.call(&plan.call)?;
				let block = self.block();
				let obj = self.object(&node.selections, &block, enums)?;
				Ok(format!(
					"select coalesce((select {obj} from {call} {block} where not ({block} is null)), null::jsonb)::text"
				))
			}
			CallReturns::Connection(conn) => self.root_connection(conn, enums),
			CallReturns::Composite(fields) => {
				let call = self.call(&plan.call)?;
				let obj = self.composite("__composite.v", fields, enums);
				Ok(format!(
					"select ({obj})::text from (select {call} as v) as __composite"
				))
			}
		}
	}

	// ----- mutations --------------------------------------------------------------------------

	fn mutation_object(
		&mut self,
		selections: &[MutSel],
		block: &str,
		enums: &Enums,
	) -> Result<String> {
		let mut pairs = vec![];
		for sel in selections {
			pairs.push(match sel {
				MutSel::AffectedCount { alias } => format!("{}, count(*)", lit(alias)),
				MutSel::Records(node) => format!(
					"{}, coalesce(jsonb_agg({}), jsonb_build_array())",
					lit(&node.alias),
					self.object(&node.selections, block, enums)?
				),
				MutSel::Typename { alias, name } => format!("{}, {}", lit(alias), lit(name)),
			});
		}
		Ok(format!("jsonb_build_object({})", pairs.join(", ")))
	}

	pub fn insert(&mut self, plan: &Insert, enums: &Enums) -> Result<String> {
		let block = self.block();
		let object = self.mutation_object(&plan.selections, &block, enums)?;
		let table = &plan.table;
		let referenced: Vec<&std::rc::Rc<Column>> = table
			.columns
			.iter()
			.filter(|c| {
				plan.rows
					.iter()
					.any(|r| r.iter().any(|(n, _)| n == &c.name))
			})
			.collect();
		let mut rows = vec![];
		for row in &plan.rows {
			let mut values = vec![];
			for column in &referenced {
				values.push(match row.iter().find(|(n, _)| n == &column.name) {
					Some((_, Some(value))) => self.write_value(column, value, enums)?,
					_ => "default".to_string(),
				});
			}
			rows.push(format!("({})", values.join(", ")));
		}
		let names: Vec<String> = referenced.iter().map(|c| ident(&c.name)).collect();
		let (target, conflict) = match &plan.on_conflict {
			None => (String::new(), String::new()),
			Some(c) => {
				// The row already there is named, so its filter reads it as a filter reads a row.
				let existing = self.block();
				let keys: Vec<String> = c.columns.iter().map(|n| ident(n)).collect();
				let action = if c.update.is_empty() {
					"do nothing".to_string()
				} else {
					let sets: Vec<String> = c
						.update
						.iter()
						.map(|col| format!("{0} = excluded.{0}", ident(&col.name)))
						.collect();
					format!(
						"do update set {} where {}",
						sets.join(", "),
						self.where_clause(&c.filter, &existing, table)?
					)
				};
				(
					format!(" as {existing}"),
					format!(" on conflict ({}) {action}", keys.join(", ")),
				)
			}
		};
		Ok(format!(
			"with affected as (insert into {}{target}({}) values {}{conflict} returning {}) select {object}::text from affected as {block}",
			qualified(&table.schema, &table.name),
			names.join(", "),
			rows.join(", "),
			self.selectable_columns(table)
		))
	}

	pub fn update(&mut self, plan: &Update, enums: &Enums) -> Result<String> {
		let block = self.block();
		let object = self.mutation_object(&plan.selections, &block, enums)?;
		let mut set = vec![];
		for (column, value) in &plan.set {
			set.push(format!(
				"{} = {}",
				ident(&column.name),
				self.write_value(column, value, enums)?
			));
		}
		let filter = self.where_clause(&plan.filter, &block, &plan.table)?;
		self.bounded(
			&format!(
				"update {} as {block} set {} where {filter} returning {}",
				qualified(&plan.table.schema, &plan.table.name),
				set.join(", "),
				self.selectable_columns(&plan.table)
			),
			&object,
			&block,
			plan.at_most,
			"update impacts too many records",
		)
	}

	pub fn delete(&mut self, plan: &Delete, enums: &Enums) -> Result<String> {
		let block = self.block();
		let object = self.mutation_object(&plan.selections, &block, enums)?;
		let filter = self.where_clause(&plan.filter, &block, &plan.table)?;
		self.bounded(
			&format!(
				"delete from {} as {block} where {filter} returning {}",
				qualified(&plan.table.schema, &plan.table.name),
				self.selectable_columns(&plan.table)
			),
			&object,
			&block,
			plan.at_most,
			"delete impacts too many records",
		)
	}

	/// A write that is refused, whole, when it touches more rows than `atMost`.
	fn bounded(
		&mut self,
		write: &str,
		object: &str,
		block: &str,
		at_most: i64,
		refusal: &str,
	) -> Result<String> {
		Ok(format!(
			"with impacted as ({write}), \
			total(total_count) as (select count(*) from impacted), \
			req(res) as (select {object} from impacted {block} limit 1), \
			wrapper(res) as (select case when total.total_count > {at_most} then graphql.exception({})::jsonb else req.res end from total left join req on true limit 1) \
			select res::text from wrapper",
			lit(refusal)
		))
	}
}

/// Enum label mappings by type, for writing a column's value out under its GraphQL name.
pub struct Enums<'a>(pub &'a crate::catalog::Catalog);

impl Enums<'_> {
	fn mappings(&self, type_oid: u32) -> Option<&[(String, String)]> {
		self.0
			.enums
			.get(&type_oid)
			.and_then(|e| e.mappings.as_deref())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn literals() {
		assert_eq!(lit("a"), "'a'");
		assert_eq!(lit("it's"), "'it''s'");
		assert_eq!(lit("a\\b"), "E'a\\\\b'");
		assert_eq!(ident("x\"y"), "\"x\"\"y\"");
	}
}
