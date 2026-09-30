//! A document's fields as plans: what to read, from where, filtered, ordered and paged how. Each
//! plan becomes one SQL statement (`sql.rs`). Everything a client can get wrong is caught here,
//! before any SQL exists.
use crate::catalog::{Attr, Column, ForeignKey, Function, Table};
use crate::codec::{NodeId, decode_cursor, parse_node_id};
use crate::coerce::coerce;
use crate::error::{Error, Result};
use crate::schema::{AggOp, FieldDef, FieldKind, InputKind, Returns, Schema, Source, TypeId};
use crate::select::{self, Fragments, response_key};
use crate::value::{In, from_literal};
use graphql_parser::query::{Field, VariableDefinition};
use serde_json::Value as Json;
use std::rc::Rc;

pub type QField<'a> = Field<'a, String>;

pub struct Ctx<'a, 'd> {
	pub schema: &'a Schema,
	pub variables: &'a serde_json::Map<String, Json>,
	pub definitions: &'a [VariableDefinition<'d, String>],
	pub fragments: &'a Fragments<'d>,
}

#[derive(Clone, Debug)]
pub enum Filter {
	Column {
		column: Rc<Column>,
		op: String,
		value: Json,
	},
	NodeId(NodeId),
	And(Vec<Filter>),
	Or(Vec<Filter>),
	Not(Box<Filter>),
	/// A spatial comparison on a PostGIS column (`Extras::postgis`): `intersects`, `contains`,
	/// `within` or `dWithin`, the value GeoJSON text (or `{geometry, distance}`).
	Geo {
		column: Rc<Column>,
		op: String,
		value: Json,
		postgis: String,
		geography: bool,
	},
	/// A composite column's attribute compared (`InputKind::Attribute`).
	Attribute {
		column: Rc<Column>,
		attr: Rc<Attr>,
		op: String,
		value: Json,
	},
	/// A computed field's value compared (`InputKind::Computed`).
	Computed {
		function: Rc<Function>,
		table: Rc<Table>,
		op: String,
		value: Json,
	},
	/// Rows related by a foreign key that match (`InputKind::Relation`).
	Related {
		key: Rc<ForeignKey>,
		reverse: bool,
		table: Rc<Table>,
		quantifier: Quantifier,
		inner: Vec<Filter>,
	},
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quantifier {
	/// At least one related row matches (a to-one relation, and `some`).
	Some,
	Every,
	None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
	AscNullsFirst,
	AscNullsLast,
	DescNullsFirst,
	DescNullsLast,
}

impl Direction {
	pub fn parse(s: &str) -> Result<Direction> {
		Ok(match s {
			"AscNullsFirst" => Direction::AscNullsFirst,
			"AscNullsLast" => Direction::AscNullsLast,
			"DescNullsFirst" => Direction::DescNullsFirst,
			"DescNullsLast" => Direction::DescNullsLast,
			_ => return Err(Error::new(format!("Invalid order operation {s}"))),
		})
	}

	pub fn reverse(self) -> Direction {
		match self {
			Direction::AscNullsFirst => Direction::DescNullsLast,
			Direction::AscNullsLast => Direction::DescNullsFirst,
			Direction::DescNullsFirst => Direction::AscNullsLast,
			Direction::DescNullsLast => Direction::AscNullsFirst,
		}
	}

	pub fn asc(self) -> bool {
		matches!(self, Direction::AscNullsFirst | Direction::AscNullsLast)
	}

	pub fn nulls_first(self) -> bool {
		matches!(self, Direction::AscNullsFirst | Direction::DescNullsFirst)
	}

	pub fn sql(self) -> &'static str {
		match self {
			Direction::AscNullsFirst => "asc nulls first",
			Direction::AscNullsLast => "asc nulls last",
			Direction::DescNullsFirst => "desc nulls first",
			Direction::DescNullsLast => "desc nulls last",
		}
	}
}

#[derive(Clone, Debug)]
pub struct Order {
	pub key: OrderKey,
	pub direction: Direction,
}

/// What a collection is ordered by.
#[derive(Clone, Debug)]
pub enum OrderKey {
	Column(Rc<Column>),
	/// A column of the one row a relation leads to (`Extras::order_by_related`).
	Related {
		key: Rc<ForeignKey>,
		reverse: bool,
		table: Rc<Table>,
		column: Rc<Column>,
	},
	/// How many rows a to-many relation has.
	Count {
		key: Rc<ForeignKey>,
		table: Rc<Table>,
	},
}

impl OrderKey {
	/// The SQL type of the value, which a cursor's value is read back as.
	pub fn type_name(&self) -> &str {
		match self {
			OrderKey::Column(c) | OrderKey::Related { column: c, .. } => &c.type_name,
			OrderKey::Count { .. } => "int8",
		}
	}
}

pub fn reversed(order: &[Order]) -> Vec<Order> {
	order
		.iter()
		.map(|o| Order {
			key: o.key.clone(),
			direction: o.direction.reverse(),
		})
		.collect()
}

/// Where a connection's rows come from.
#[derive(Clone, Debug)]
pub enum Rows {
	/// The table itself.
	Table,
	/// The table's rows related to the enclosing row by a foreign key.
	Related { key: Rc<ForeignKey>, reverse: bool },
	/// A set-returning function of the enclosing row.
	RowFunction {
		function: Rc<Function>,
		input: Rc<Table>,
		args: Vec<CallArg>,
	},
	/// A set-returning function called with arguments, at the root.
	Call(Call),
}

#[derive(Clone, Debug)]
pub struct Call {
	pub function: Rc<Function>,
	pub args: Vec<CallArg>,
}

/// An argument a function is called with: by name, or by position where it has no name.
#[derive(Clone, Debug)]
pub struct CallArg {
	pub name: Option<String>,
	pub position: usize,
	pub type_name: String,
	pub value: Json,
}

#[derive(Clone, Debug)]
pub struct Connection {
	pub alias: String,
	pub table: Rc<Table>,
	pub rows: Rows,
	pub first: Option<u64>,
	pub last: Option<u64>,
	pub before: Option<Vec<Json>>,
	pub after: Option<Vec<Json>>,
	pub offset: Option<u64>,
	pub filter: Vec<Filter>,
	pub order: Vec<Order>,
	pub max_rows: u64,
	pub selections: Vec<ConnSel>,
	/// `distinctOn`'s columns (`Extras::distinct_on`).
	pub distinct: Vec<Rc<Column>>,
}

impl Connection {
	pub fn reverse(&self) -> bool {
		self.last.is_some() || self.before.is_some()
	}

	pub fn limit(&self) -> u64 {
		self.first
			.unwrap_or_else(|| self.last.unwrap_or(self.max_rows))
			.min(self.max_rows)
	}

	pub fn page_selections(&self) -> impl Iterator<Item = &PageSel> {
		self.selections.iter().flat_map(|s| match s {
			ConnSel::PageInfo { selections, .. } => selections.iter().collect::<Vec<_>>(),
			_ => vec![],
		})
	}
}

#[derive(Clone, Debug)]
pub enum ConnSel {
	TotalCount {
		alias: String,
	},
	Edges {
		alias: String,
		selections: Vec<EdgeSel>,
	},
	PageInfo {
		alias: String,
		selections: Vec<PageSel>,
	},
	Typename {
		alias: String,
		name: String,
	},
	Aggregate {
		alias: String,
		selections: Vec<AggSel>,
	},
}

#[derive(Clone, Debug)]
pub enum EdgeSel {
	Cursor { alias: String },
	Node(Node),
	Typename { alias: String, name: String },
}

#[derive(Clone, Debug)]
pub enum PageSel {
	StartCursor { alias: String },
	EndCursor { alias: String },
	HasNextPage { alias: String },
	HasPreviousPage { alias: String },
	Typename { alias: String, name: String },
}

#[derive(Clone, Debug)]
pub enum AggSel {
	Count {
		alias: String,
	},
	Op {
		alias: String,
		op: AggOp,
		columns: Vec<(String, Rc<Column>)>,
	},
	Typename {
		alias: String,
		name: String,
	},
}

#[derive(Clone, Debug)]
pub struct Node {
	pub alias: String,
	pub table: Rc<Table>,
	/// For a related row, the key it is reached by and in which direction.
	pub key: Option<(Rc<ForeignKey>, bool)>,
	pub node_id: Option<NodeId>,
	pub selections: Vec<NodeSel>,
}

#[derive(Clone, Debug)]
pub enum NodeSel {
	Column {
		alias: String,
		column: Rc<Column>,
	},
	/// A composite column, and what was selected of it (`Extras::composites`).
	CompositeColumn {
		alias: String,
		column: Rc<Column>,
		fields: Vec<AttrSel>,
	},
	NodeId {
		alias: String,
		table: Rc<Table>,
	},
	Computed {
		alias: String,
		function: Rc<Function>,
		table: Rc<Table>,
		returns: Computed,
		args: Vec<CallArg>,
	},
	Related(Node),
	RelatedMany(Connection),
	Typename {
		alias: String,
		name: String,
	},
}

#[derive(Clone, Debug)]
pub enum Computed {
	Scalar,
	Array,
	Composite(Vec<AttrSel>),
	Node(Box<Node>),
	Connection(Box<Connection>),
}

#[derive(Clone, Debug)]
pub struct ByPk {
	pub table: Rc<Table>,
	pub keys: Vec<(Rc<Column>, Json)>,
	pub selections: Vec<NodeSel>,
}

#[derive(Clone, Debug)]
pub enum MutSel {
	AffectedCount { alias: String },
	Records(Node),
	Typename { alias: String, name: String },
}

#[derive(Clone, Debug)]
pub struct Insert {
	pub table: Rc<Table>,
	/// Each row's values by column; `None` is the column's default.
	pub rows: Vec<Vec<(String, Option<Json>)>>,
	pub selections: Vec<MutSel>,
	pub on_conflict: Option<OnConflict>,
}

/// An upsert's conflict handling (`Extras::upsert`).
#[derive(Clone, Debug)]
pub struct OnConflict {
	/// The unique index's columns, which Postgres matches to the index.
	pub columns: Vec<String>,
	/// Set from the row that conflicted; empty leaves the row there as it was.
	pub update: Vec<Rc<Column>>,
	/// Only a row already there that matches is updated.
	pub filter: Vec<Filter>,
}

#[derive(Clone, Debug)]
pub struct Update {
	pub table: Rc<Table>,
	pub set: Vec<(Rc<Column>, Json)>,
	pub filter: Vec<Filter>,
	pub at_most: i64,
	pub selections: Vec<MutSel>,
}

#[derive(Clone, Debug)]
pub struct Delete {
	pub table: Rc<Table>,
	pub filter: Vec<Filter>,
	pub at_most: i64,
	pub selections: Vec<MutSel>,
}

#[derive(Clone, Debug)]
pub enum CallReturns {
	Scalar,
	Node(Node),
	Connection(Connection),
	Composite(Vec<AttrSel>),
}

/// What is selected of a composite value.
#[derive(Clone, Debug)]
pub enum AttrSel {
	Attr {
		alias: String,
		attr: Rc<Attr>,
		/// An attribute that is itself composite.
		fields: Option<Vec<AttrSel>>,
	},
	Typename {
		alias: String,
		name: String,
	},
}

#[derive(Clone, Debug)]
pub struct FunctionCall {
	pub call: Call,
	pub returns: CallReturns,
}

impl<'a, 'd> Ctx<'a, 'd> {
	fn type_name(&self, id: TypeId) -> &str {
		&self.schema.ty(id).name
	}

	fn fields_of(
		&self,
		set: &graphql_parser::query::SelectionSet<'d, String>,
		type_id: TypeId,
	) -> Result<Vec<QField<'d>>> {
		select::fields(set, self.fragments, self.type_name(type_id), self.variables)
	}

	/// The value given for an argument, checked against the argument's type.
	fn arg(&self, field: &FieldDef, qf: &QField<'d>, name: &str) -> Result<In> {
		let Some(def) = field.args.iter().find(|a| a.name == name) else {
			return Err(Error::new(format!("Internal error 1: {name}")));
		};
		let given = match qf.arguments.iter().find(|(n, _)| n == name) {
			None => In::Absent,
			Some((_, literal)) => from_literal(literal, self.variables, self.definitions)?,
		};
		coerce(self.schema, &def.ty, &given)
	}

	fn restrict(&self, allowed: &[&str], qf: &QField<'d>) -> Result<()> {
		let extra: Vec<&str> = qf
			.arguments
			.iter()
			.map(|(n, _)| n.as_str())
			.filter(|n| !allowed.contains(n))
			.collect();
		if extra.is_empty() {
			Ok(())
		} else {
			Err(Error::new(format!("Input contains extra keys {extra:?}")))
		}
	}

	// ----- connections ------------------------------------------------------------------------

	fn unsigned(&self, field: &FieldDef, qf: &QField<'d>, name: &str) -> Result<Option<u64>> {
		match self.arg(field, qf, name)? {
			In::Absent | In::Null => Ok(None),
			In::Int(n) if n < 0 => Err(Error::new(format!("`{name}` must be an unsigned integer"))),
			In::Int(n) => Ok(Some(n as u64)),
			_ => Err(Error::new(format!(
				"Internal Error: failed to parse validated {name}"
			))),
		}
	}

	fn cursor(&self, field: &FieldDef, qf: &QField<'d>, name: &str) -> Result<Option<Vec<Json>>> {
		match self.arg(field, qf, name)? {
			// A null cursor means no cursor, which is what clients that pass one mean.
			In::Absent | In::Null => Ok(None),
			In::Str(text) => decode_cursor(&text).map(Some),
			_ => Err(Error::new("Cursor re-validation errror")),
		}
	}

	pub fn connection(
		&self,
		field: &FieldDef,
		qf: &QField<'d>,
		table: &Rc<Table>,
		rows: Rows,
		extra_args: &[&str],
	) -> Result<Connection> {
		let connection_type = self.schema.connection_type(table);
		let mut allowed = vec![
			"first", "last", "before", "after", "offset", "filter", "orderBy",
		];
		allowed.extend_from_slice(extra_args);
		let distinct_on = field.args.iter().any(|a| a.name == "distinctOn");
		if distinct_on {
			allowed.push("distinctOn");
		}
		self.restrict(&allowed, qf)?;
		let mut distinct: Vec<Rc<Column>> = vec![];
		if distinct_on && let In::List(items) = self.arg(field, qf, "distinctOn")? {
			let fields = self.schema.distinct_fields(table);
			for item in items {
				let In::Str(name) = item else { continue };
				let column = fields
					.iter()
					.find(|c| self.schema.column_name(c) == name)
					.ok_or_else(|| Error::new("distinctOn re-validation error"))?;
				if !distinct.iter().any(|c| c.name == column.name) {
					distinct.push(Rc::clone(column));
				}
			}
		}

		let first = self.unsigned(field, qf, "first")?;
		let last = self.unsigned(field, qf, "last")?;
		let offset = self.unsigned(field, qf, "offset")?;
		let max_rows = self
			.schema
			.catalog
			.schema(table.schema_oid)
			.map(|s| table.max_rows.unwrap_or(s.max_rows))
			.unwrap_or(30);
		let before = self.cursor(field, qf, "before")?;
		let after = self.cursor(field, qf, "after")?;

		if first.is_some() && last.is_some() {
			return Err(Error::new(
				"only one of \"first\" and \"last\" may be provided",
			));
		} else if before.is_some() && after.is_some() {
			return Err(Error::new(
				"only one of \"before\" and \"after\" may be provided",
			));
		} else if first.is_some() && before.is_some() {
			return Err(Error::new("\"first\" may only be used with \"after\""));
		} else if last.is_some() && after.is_some() {
			return Err(Error::new("\"last\" may only be used with \"before\""));
		} else if offset.is_some() && (last.is_some() || before.is_some()) {
			return Err(Error::new(
				"\"offset\" may only be used with \"first\" and \"after\"",
			));
		}

		let filter = self.filter(field, qf, "filter")?;
		let order = self.order_by(field, qf, table)?;

		let mut selections = vec![];
		for sel in self.fields_of(&qf.selection_set, connection_type)? {
			let alias = response_key(&sel);
			if sel.name == "__typename" {
				selections.push(ConnSel::Typename {
					alias,
					name: self.type_name(connection_type).to_string(),
				});
				continue;
			}
			let Some(f) = self.schema.field(connection_type, &sel.name) else {
				return Err(Error::new(if sel.name == "aggregate" {
					"enable the aggregate directive to use aggregates"
				} else {
					"unknown field in connection"
				}));
			};
			selections.push(match &f.kind {
				FieldKind::Edges => ConnSel::Edges {
					alias,
					selections: self.edges(f, &sel)?,
				},
				FieldKind::PageInfo => ConnSel::PageInfo {
					alias,
					selections: self.page_info(&sel)?,
				},
				FieldKind::Aggregate => ConnSel::Aggregate {
					alias,
					selections: self.aggregate(f, &sel)?,
				},
				FieldKind::TotalCount => ConnSel::TotalCount { alias },
				_ => {
					return Err(Error::new(format!(
						"unknown field type on connection: {}",
						sel.name
					)));
				}
			});
		}

		Ok(Connection {
			alias: response_key(qf),
			table: Rc::clone(table),
			rows,
			first,
			last,
			before,
			after,
			offset,
			filter,
			order,
			max_rows,
			selections,
			distinct,
		})
	}

	fn edges(&self, field: &FieldDef, qf: &QField<'d>) -> Result<Vec<EdgeSel>> {
		let edge_type = field.ty.base();
		let mut out = vec![];
		for sel in self.fields_of(&qf.selection_set, edge_type)? {
			let alias = response_key(&sel);
			if sel.name == "__typename" {
				out.push(EdgeSel::Typename {
					alias,
					name: self.type_name(edge_type).to_string(),
				});
				continue;
			}
			let Some(f) = self.schema.field(edge_type, &sel.name) else {
				return Err(Error::new("unknown field in edge"));
			};
			out.push(match &f.kind {
				FieldKind::EdgeNode => {
					let Source::Node(table) = &self.schema.ty(f.ty.base()).source else {
						return Err(Error::new("unexpected field type on edge"));
					};
					EdgeSel::Node(self.node(f, &sel, table, None, &[])?)
				}
				FieldKind::Cursor => EdgeSel::Cursor { alias },
				_ => return Err(Error::new("unexpected field type on edge")),
			});
		}
		Ok(out)
	}

	fn page_info(&self, qf: &QField<'d>) -> Result<Vec<PageSel>> {
		let page_info = self.schema.page_info;
		let mut out = vec![];
		for sel in self.fields_of(&qf.selection_set, page_info)? {
			let alias = response_key(&sel);
			if sel.name == "__typename" {
				out.push(PageSel::Typename {
					alias,
					name: "PageInfo".into(),
				});
				continue;
			}
			let Some(f) = self.schema.field(page_info, &sel.name) else {
				return Err(Error::new("unknown field in pageInfo"));
			};
			out.push(match f.kind {
				FieldKind::StartCursor => PageSel::StartCursor { alias },
				FieldKind::EndCursor => PageSel::EndCursor { alias },
				FieldKind::HasNextPage => PageSel::HasNextPage { alias },
				FieldKind::HasPreviousPage => PageSel::HasPreviousPage { alias },
				_ => return Err(Error::new("unexpected field type on pageInfo")),
			});
		}
		Ok(out)
	}

	fn aggregate(&self, field: &FieldDef, qf: &QField<'d>) -> Result<Vec<AggSel>> {
		let agg_type = field.ty.base();
		let agg_name = self.type_name(agg_type).to_string();
		let mut out = vec![];
		for sel in self.fields_of(&qf.selection_set, agg_type)? {
			let alias = response_key(&sel);
			if sel.name == "__typename" {
				out.push(AggSel::Typename {
					alias,
					name: agg_name.clone(),
				});
				continue;
			}
			let Some(f) = self.schema.field(agg_type, &sel.name) else {
				return Err(Error::new(format!(
					"Unknown field \"{}\" selected on type \"{}\"",
					sel.name, agg_name
				)));
			};
			out.push(match &f.kind {
				FieldKind::AggCount => AggSel::Count { alias },
				FieldKind::AggOp(op) => AggSel::Op {
					alias,
					op: *op,
					columns: self.aggregate_columns(f, &sel)?,
				},
				_ => return Err(Error::new(format!("Unknown aggregate field: {}", sel.name))),
			});
		}
		Ok(out)
	}

	fn aggregate_columns(
		&self,
		field: &FieldDef,
		qf: &QField<'d>,
	) -> Result<Vec<(String, Rc<Column>)>> {
		let result_type = field.ty.base();
		let type_name = self.type_name(result_type).to_string();
		let mut out = vec![];
		for sel in self.fields_of(&qf.selection_set, result_type)? {
			if sel.name == "__typename" {
				return Err(Error::new(
					"Internal error: Missing column info for aggregate field '__typename'",
				));
			}
			let Some(f) = self.schema.field(result_type, &sel.name) else {
				return Err(Error::new(format!(
					"Unknown or invalid field \"{}\" selected on type \"{}\"",
					sel.name, type_name
				)));
			};
			let FieldKind::AggColumn(column) = &f.kind else {
				return Err(Error::new(format!(
					"Internal error: Missing column info for aggregate field '{}'",
					sel.name
				)));
			};
			out.push((response_key(&sel), Rc::clone(column)));
		}
		Ok(out)
	}

	// ----- filters and ordering ---------------------------------------------------------------

	fn filter(&self, field: &FieldDef, qf: &QField<'d>, name: &str) -> Result<Vec<Filter>> {
		let def = field
			.args
			.iter()
			.find(|a| a.name == name)
			.ok_or_else(|| Error::new(format!("Internal error 1: {name}")))?;
		let entity = def.ty.base();
		if !matches!(self.schema.ty(entity).source, Source::FilterEntity(_)) {
			return Err(Error::new("Could not locate Filter Entity type"));
		}
		let value = self.arg(field, qf, name)?;
		self.filters(&value, entity)
	}

	fn filters(&self, value: &In, entity: TypeId) -> Result<Vec<Filter>> {
		let map = match value {
			In::Absent | In::Null => return Ok(vec![]),
			In::Object(m) => m,
			_ => return Err(Error::new("Filter re-validation error")),
		};
		let mut out = vec![];
		for (key, ops) in map {
			let Some(input) = self.schema.input(entity, key) else {
				return Err(Error::new("Filter re-validation error in filter_iv"));
			};
			if let InputKind::Relation {
				key: fk,
				reverse,
				table,
				many,
			} = &input.kind
			{
				let In::Object(given) = ops else {
					continue;
				};
				let entity = self.schema.node_filter(table);
				let quantified: Vec<(Quantifier, &In)> = if *many {
					given
						.iter()
						.map(|(q, v)| {
							let q = match q.as_str() {
								"some" => Quantifier::Some,
								"every" => Quantifier::Every,
								_ => Quantifier::None,
							};
							(q, v)
						})
						.collect()
				} else {
					vec![(Quantifier::Some, ops)]
				};
				for (quantifier, v) in quantified {
					if v.is_missing() {
						continue;
					}
					out.push(Filter::Related {
						key: Rc::clone(fk),
						reverse: *reverse,
						table: Rc::clone(table),
						quantifier,
						inner: self.filters(v, entity)?,
					});
				}
				continue;
			}
			if let (InputKind::Column(column), In::Object(attrs)) = (&input.kind, ops)
				&& matches!(
					self.schema.ty(input.ty.base()).source,
					Source::CompositeFilter(_)
				) {
				let composite = input.ty.base();
				for (name, attr_ops) in attrs {
					let Some(InputKind::Attribute(attr)) =
						self.schema.input(composite, name).map(|i| &i.kind)
					else {
						return Err(Error::new("Filter re-validation error in filter_iv"));
					};
					let In::Object(attr_ops) = attr_ops else {
						continue;
					};
					for (op, v) in attr_ops {
						if !is_filter_op(op) {
							return Err(Error::new(format!("Invalid filter operation: {op}")));
						}
						if v.is_absent() {
							continue;
						}
						out.push(Filter::Attribute {
							column: Rc::clone(column),
							attr: Rc::clone(attr),
							op: op.clone(),
							value: v.to_json()?,
						});
					}
				}
				continue;
			}
			match ops {
				In::Absent | In::Null => continue,
				In::Object(op_values) => {
					let is_not = key == "not" && matches!(input.kind, InputKind::Plain);
					if is_not {
						let inner = self.filters(ops, entity)?;
						if !inner.is_empty() {
							out.push(Filter::Not(Box::new(Filter::And(inner))));
						}
						continue;
					}
					for (op, v) in op_values {
						if !is_filter_op(op) {
							return Err(Error::new(format!("Invalid filter operation: {op}")));
						}
						if v.is_absent() {
							continue;
						}
						out.push(match &input.kind {
							InputKind::Column(column)
								if matches!(
									op.as_str(),
									"intersects" | "contains" | "within" | "dWithin"
								) && self.schema.is_geo(column.type_oid) =>
							{
								Filter::Geo {
									column: Rc::clone(column),
									op: op.clone(),
									value: v.to_json()?,
									postgis: self
										.schema
										.catalog
										.postgis_schema
										.clone()
										.unwrap_or_default(),
									geography: self
										.schema
										.catalog
										.types
										.get(&column.type_oid)
										.is_some_and(|t| t.name == "geography"),
								}
							}
							InputKind::Column(column) => Filter::Column {
								column: Rc::clone(column),
								op: op.clone(),
								value: v.to_json()?,
							},
							InputKind::NodeId => Filter::NodeId(parse_node_id(v)?),
							InputKind::Attribute(_) => {
								return Err(Error::new("Filter re-validation error"));
							}
							InputKind::Computed(function) => {
								let Source::FilterEntity(table) = &self.schema.ty(entity).source
								else {
									return Err(Error::new("Filter re-validation error"));
								};
								Filter::Computed {
									function: Rc::clone(function),
									table: Rc::clone(table),
									op: op.clone(),
									value: v.to_json()?,
								}
							}
							_ => {
								return Err(Error::new(
									"Filter type error, attempted filter on non-column",
								));
							}
						});
					}
				}
				In::List(items) if key == "and" || key == "or" => {
					if items.is_empty() {
						continue;
					}
					let mut groups = vec![];
					for item in items {
						let inner = self.filters(item, entity)?;
						if !inner.is_empty() {
							groups.push(Filter::And(inner));
						}
					}
					out.push(if key == "and" {
						Filter::And(groups)
					} else {
						Filter::Or(groups)
					});
				}
				_ => return Err(Error::new("Filter re-validation errror op_to_value map")),
			}
		}
		Ok(out)
	}

	fn order_by(&self, field: &FieldDef, qf: &QField<'d>, table: &Rc<Table>) -> Result<Vec<Order>> {
		let def = field
			.args
			.iter()
			.find(|a| a.name == "orderBy")
			.ok_or_else(|| Error::new("Internal error 1: orderBy"))?;
		let entity = def.ty.base();
		let value = self.arg(field, qf, "orderBy")?;
		let mut out = vec![];
		match &value {
			In::Absent | In::Null => {}
			In::List(items) => {
				for item in items {
					match item {
						In::Absent | In::Null => continue,
						In::Object(m) => {
							for (column, direction) in m {
								if let Some(InputKind::Relation {
									key,
									reverse,
									table: other,
									many,
								}) = self.schema.input(entity, column).map(|i| &i.kind)
								{
									out.extend(
										self.related_order(key, *reverse, other, *many, direction)?,
									);
									continue;
								}
								let direction = match direction {
									In::Absent | In::Null => continue,
									In::Str(s) => Direction::parse(s)?,
									_ => return Err(Error::new("Order re-validation error 6")),
								};
								let Some(input) = self.schema.input(entity, column) else {
									return Err(Error::new("Order re-validation error 3"));
								};
								let InputKind::Column(column) = &input.kind else {
									return Err(Error::new("Order re-validation error 4"));
								};
								out.push(Order {
									key: OrderKey::Column(Rc::clone(column)),
									direction,
								});
							}
						}
						_ => return Err(Error::new("OrderBy re-validation errror 1")),
					}
				}
			}
			_ => return Err(Error::new("OrderBy re-validation errror")),
		}
		// Every order ends with the primary key, so a page boundary is always well defined.
		if table.primary_key().is_none() {
			return Err(Error::new("Found table with no primary key"));
		}
		for column in table.primary_key_columns() {
			out.push(Order {
				key: OrderKey::Column(column),
				direction: Direction::AscNullsLast,
			});
		}
		Ok(out)
	}

	/// `{author: {name: AscNullsLast}}` or `{reviewCollection: {count: DescNullsLast}}`: one hop,
	/// so each key is one subquery on an index the foreign key usually has.
	fn related_order(
		&self,
		key: &Rc<ForeignKey>,
		reverse: bool,
		other: &Rc<Table>,
		many: bool,
		value: &In,
	) -> Result<Vec<Order>> {
		let In::Object(m) = value else {
			return Ok(vec![]);
		};
		let entity = self.schema.order_by_entity(other);
		let mut out = vec![];
		for (name, direction) in m {
			let direction = match direction {
				In::Absent | In::Null => continue,
				In::Str(s) => Direction::parse(s)?,
				_ => {
					return Err(Error::new(
						"Ordering by a related row follows one relation, to that row's own fields",
					));
				}
			};
			let order_key = if many {
				OrderKey::Count {
					key: Rc::clone(key),
					table: Rc::clone(other),
				}
			} else {
				let Some(InputKind::Column(column)) =
					self.schema.input(entity, name).map(|i| &i.kind)
				else {
					return Err(Error::new("Order re-validation error 3"));
				};
				OrderKey::Related {
					key: Rc::clone(key),
					reverse,
					table: Rc::clone(other),
					column: Rc::clone(column),
				}
			};
			out.push(Order {
				key: order_key,
				direction,
			});
		}
		Ok(out)
	}

	// ----- nodes ------------------------------------------------------------------------------

	pub fn node(
		&self,
		field: &FieldDef,
		qf: &QField<'d>,
		table: &Rc<Table>,
		key: Option<(Rc<ForeignKey>, bool)>,
		extra_args: &[&str],
	) -> Result<Node> {
		self.restrict(extra_args, qf)?;
		let node_type = self.schema.node_type(table);
		let mut allowed = vec!["nodeId"];
		allowed.extend_from_slice(extra_args);
		self.restrict(&allowed, qf)?;
		let node_id = if field.args.iter().any(|a| a.name == "nodeId") {
			Some(parse_node_id(&self.arg(field, qf, "nodeId")?)?)
		} else {
			None
		};
		let selections = self.node_selections(node_type, table, qf, false)?;
		Ok(Node {
			alias: response_key(qf),
			table: Rc::clone(table),
			key,
			node_id,
			selections,
		})
	}

	/// `node(nodeId:)`: the table the id names.
	pub fn node_entry(&self, field: &FieldDef, qf: &QField<'d>) -> Result<Node> {
		self.restrict(&["nodeId"], qf)?;
		let id = parse_node_id(&self.arg(field, qf, "nodeId")?)?;
		let table = self
			.schema
			.node_types()
			.into_iter()
			.find_map(|t| match &self.schema.ty(t).source {
				Source::Node(table) if table.schema == id.schema && table.name == id.table => {
					Some(Rc::clone(table))
				}
				_ => None,
			})
			.ok_or_else(|| {
				Error::new("Collection referenced by nodeId did not match any known collection")
			})?;
		let node_type = self.schema.node_type(&table);
		let selections = self.node_selections(node_type, &table, qf, false)?;
		Ok(Node {
			alias: response_key(qf),
			table,
			key: None,
			node_id: Some(id),
			selections,
		})
	}

	pub fn by_pk(&self, field: &FieldDef, qf: &QField<'d>, table: &Rc<Table>) -> Result<ByPk> {
		let node_type = self.schema.node_type(table);
		let pk = table
			.primary_key()
			.ok_or_else(|| Error::new("Table has no primary key"))?;
		let mut keys: Vec<(Rc<Column>, Json)> = vec![];
		for (name, literal) in &qf.arguments {
			let Some(def) = field.args.iter().find(|a| &a.name == name) else {
				continue;
			};
			let InputKind::Column(column) = &def.kind else {
				continue;
			};
			let value = from_literal(literal, self.variables, self.definitions)?.to_json()?;
			keys.retain(|(c, _)| c.name != column.name);
			keys.push((Rc::clone(column), value));
		}
		if keys.len() != pk.len() {
			let missing: Vec<&str> = pk
				.iter()
				.filter(|n| !keys.iter().any(|(c, _)| &c.name == *n))
				.map(String::as_str)
				.collect();
			return Err(Error::new(format!(
				"Missing primary key column(s): {}",
				missing.join(", ")
			)));
		}
		let selections = self.node_selections(node_type, table, qf, true)?;
		Ok(ByPk {
			table: Rc::clone(table),
			keys,
			selections,
		})
	}

	fn node_selections(
		&self,
		node_type: TypeId,
		table: &Rc<Table>,
		qf: &QField<'d>,
		by_pk: bool,
	) -> Result<Vec<NodeSel>> {
		let type_name = self.type_name(node_type).to_string();
		let mut out = vec![];
		for sel in self.fields_of(&qf.selection_set, node_type)? {
			let alias = response_key(&sel);
			if sel.name == "__typename" {
				out.push(NodeSel::Typename {
					alias,
					name: type_name.clone(),
				});
				continue;
			}
			let Some(f) = self.schema.field(node_type, &sel.name) else {
				return Err(Error::new(if by_pk {
					format!("Unknown field \"{}\" on type {}", sel.name, type_name)
				} else {
					format!("Unknown field '{}' on type '{}'", sel.name, type_name)
				}));
			};
			out.push(match &f.kind {
				FieldKind::Column(column)
					if matches!(self.schema.ty(f.ty.base()).source, Source::Composite(_)) =>
				{
					NodeSel::CompositeColumn {
						alias,
						column: Rc::clone(column),
						fields: self.attr_selections(f.ty.base(), &sel)?,
					}
				}
				FieldKind::Column(column) => NodeSel::Column {
					alias,
					column: Rc::clone(column),
				},
				FieldKind::NodeId(t) => NodeSel::NodeId {
					alias,
					table: Rc::clone(t),
				},
				FieldKind::Computed(function, returns) => {
					let args = self.call_args(f, &sel)?;
					let names: Vec<&str> = f
						.args
						.iter()
						.filter(|a| matches!(a.kind, InputKind::FunctionArg { .. }))
						.map(|a| a.name.as_str())
						.collect();
					let computed = match returns {
						Returns::Scalar => Computed::Scalar,
						Returns::Enum if function.shapes => Computed::Scalar,
						Returns::List => Computed::Array,
						Returns::Node(t) => {
							Computed::Node(Box::new(self.node(f, &sel, t, None, &names)?))
						}
						Returns::Connection(t) => Computed::Connection(Box::new(self.connection(
							f,
							&sel,
							t,
							Rows::RowFunction {
								function: Rc::clone(function),
								input: Rc::clone(table),
								args: args.clone(),
							},
							&names,
						)?)),
						Returns::Composite => {
							Computed::Composite(self.attr_selections(f.ty.base(), &sel)?)
						}
						Returns::Enum => {
							return Err(Error::new("invalid return type from function"));
						}
					};
					NodeSel::Computed {
						alias,
						function: Rc::clone(function),
						table: Rc::clone(table),
						returns: computed,
						args,
					}
				}
				FieldKind::RelationOne {
					key,
					reverse,
					table: foreign,
				} => NodeSel::Related(self.node(
					f,
					&sel,
					foreign,
					Some((Rc::clone(key), *reverse)),
					&[],
				)?),
				FieldKind::RelationMany {
					key,
					table: foreign,
				} => NodeSel::RelatedMany(self.connection(
					f,
					&sel,
					foreign,
					Rows::Related {
						key: Rc::clone(key),
						reverse: true,
					},
					&[],
				)?),
				_ => {
					return Err(Error::new(format!(
						"unexpected field type on node {}",
						f.name
					)));
				}
			});
		}
		Ok(out)
	}

	// ----- mutations --------------------------------------------------------------------------

	fn mutation_selections(
		&self,
		field: &FieldDef,
		qf: &QField<'d>,
		what: &str,
		table: &Rc<Table>,
	) -> Result<Vec<MutSel>> {
		let response_type = field.ty.base();
		let response_name = self.type_name(response_type).to_string();
		let mut out = vec![];
		for sel in self.fields_of(&qf.selection_set, response_type)? {
			let alias = response_key(&sel);
			if sel.name == "__typename" {
				out.push(MutSel::Typename {
					alias,
					name: response_name.clone(),
				});
				continue;
			}
			let Some(f) = self.schema.field(response_type, &sel.name) else {
				return Err(Error::new(format!("unknown field in {what}")));
			};
			out.push(match f.kind {
				FieldKind::AffectedCount => MutSel::AffectedCount { alias },
				FieldKind::Records => MutSel::Records(self.node(f, &sel, table, None, &[])?),
				_ => {
					return Err(Error::new(format!(
						"unexpected field type on {what} response"
					)));
				}
			});
		}
		Ok(out)
	}

	fn at_most(&self, field: &FieldDef, qf: &QField<'d>) -> Result<i64> {
		// Any value that does not check out means the default, 1.
		match self.arg(field, qf, "atMost").unwrap_or(In::Int(1)) {
			In::Int(n) => Ok(n),
			_ => Err(Error::new(
				"Internal Error: failed to parse validated atFirst",
			)),
		}
	}

	pub fn insert(&self, field: &FieldDef, qf: &QField<'d>, table: &Rc<Table>) -> Result<Insert> {
		let upsert = field.args.iter().any(|a| a.name == "onConflict");
		self.restrict(
			if upsert {
				&["objects", "onConflict"]
			} else {
				&["objects"]
			},
			qf,
		)?;
		let objects = self.arg(field, qf, "objects")?;
		let input_type = field.args[0].ty.base();
		let mut rows = vec![];
		match &objects {
			In::Absent | In::Null => {}
			In::List(items) => {
				for item in items {
					let mut row = vec![];
					match item {
						In::Absent | In::Null => continue,
						In::Object(values) => {
							for (name, value) in values {
								let Some(input) = self.schema.input(input_type, name) else {
									return Err(Error::new("Insert re-validation error 3"));
								};
								let InputKind::Column(column) = &input.kind else {
									return Err(Error::new("Insert re-validation error 4"));
								};
								let value = if value.is_absent() {
									None
								} else {
									Some(value.to_json()?)
								};
								row.push((column.name.clone(), value));
							}
						}
						_ => return Err(Error::new("Insert re-validation errror 1")),
					}
					rows.push(row);
				}
			}
			_ => return Err(Error::new("Insert re-validation errror")),
		}
		if rows.is_empty() {
			return Err(Error::new(
				"At least one record must be provided to objects",
			));
		}
		let on_conflict = if upsert {
			self.on_conflict(&self.arg(field, qf, "onConflict")?, table)?
		} else {
			None
		};
		let selections = self.mutation_selections(field, qf, "insert", table)?;
		Ok(Insert {
			table: Rc::clone(table),
			rows,
			selections,
			on_conflict,
		})
	}

	fn on_conflict(&self, value: &In, table: &Rc<Table>) -> Result<Option<OnConflict>> {
		let In::Object(given) = value else {
			return Ok(None);
		};
		let Some(In::Str(name)) = given.get("constraint") else {
			return Err(Error::new("Invalid input for NonNull type"));
		};
		let index = self
			.schema
			.upsert_indexes(table)
			.into_iter()
			.find(|i| &i.name == name)
			.ok_or_else(|| Error::new("onConflict re-validation error"))?;
		let fields = self.schema.upsert_fields(table);
		let mut update: Vec<Rc<Column>> = vec![];
		if let Some(In::List(items)) = given.get("updateFields") {
			for item in items {
				let In::Str(field) = item else { continue };
				let column = fields
					.iter()
					.find(|c| &self.schema.column_name(c) == field)
					.ok_or_else(|| Error::new("onConflict re-validation error"))?;
				if !update.iter().any(|c| c.name == column.name) {
					update.push(Rc::clone(column));
				}
			}
		}
		let filter = match given.get("filter") {
			Some(v) => self.filters(v, self.schema.node_filter(table))?,
			None => vec![],
		};
		Ok(Some(OnConflict {
			columns: index.columns,
			update,
			filter,
		}))
	}

	pub fn update(&self, field: &FieldDef, qf: &QField<'d>, table: &Rc<Table>) -> Result<Update> {
		self.restrict(&["set", "filter", "atMost"], qf)?;
		let set_value = self.arg(field, qf, "set")?;
		let input_type = field
			.args
			.iter()
			.find(|a| a.name == "set")
			.map(|a| a.ty.base())
			.unwrap_or(0);
		let mut set = vec![];
		match &set_value {
			In::Absent | In::Null => {}
			In::Object(values) => {
				for (name, value) in values {
					if value.is_absent() {
						continue;
					}
					let Some(input) = self.schema.input(input_type, name) else {
						return Err(Error::new("Update re-validation error 3"));
					};
					let InputKind::Column(column) = &input.kind else {
						return Err(Error::new("Update re-validation error 4"));
					};
					set.push((Rc::clone(column), value.to_json()?));
				}
			}
			_ => return Err(Error::new("Update re-validation errror")),
		}
		if set.is_empty() {
			return Err(Error::new(
				"At least one mapping must be provided to set argument",
			));
		}
		let filter = self.filter(field, qf, "filter")?;
		let at_most = self.at_most(field, qf)?;
		let selections = self.mutation_selections(field, qf, "update", table)?;
		Ok(Update {
			table: Rc::clone(table),
			set,
			filter,
			at_most,
			selections,
		})
	}

	pub fn delete(&self, field: &FieldDef, qf: &QField<'d>, table: &Rc<Table>) -> Result<Delete> {
		self.restrict(&["filter", "atMost"], qf)?;
		let filter = self.filter(field, qf, "filter")?;
		let at_most = self.at_most(field, qf)?;
		let selections = self.mutation_selections(field, qf, "delete", table)?;
		Ok(Delete {
			table: Rc::clone(table),
			filter,
			at_most,
			selections,
		})
	}

	/// What a document selects of a composite value.
	fn attr_selections(&self, type_id: TypeId, qf: &QField<'d>) -> Result<Vec<AttrSel>> {
		let type_name = self.type_name(type_id).to_string();
		if qf.selection_set.items.is_empty() {
			return Err(Error::new(format!(
				"Field of type {type_name} must have a selection of subfields"
			)));
		}
		let mut out = vec![];
		for sel in self.fields_of(&qf.selection_set, type_id)? {
			let alias = response_key(&sel);
			if sel.name == "__typename" {
				out.push(AttrSel::Typename {
					alias,
					name: type_name.clone(),
				});
				continue;
			}
			let Some(f) = self.schema.field(type_id, &sel.name) else {
				return Err(Error::new(format!(
					"Unknown field \"{}\" on type {type_name}",
					sel.name
				)));
			};
			let FieldKind::Attribute(attr) = &f.kind else {
				return Err(Error::new(format!("unexpected field type on {type_name}")));
			};
			let inner = f.ty.base();
			let fields = match self.schema.ty(inner).source {
				Source::Composite(_) => Some(self.attr_selections(inner, &sel)?),
				_ => None,
			};
			out.push(AttrSel::Attr {
				alias,
				attr: Rc::clone(attr),
				fields,
			});
		}
		Ok(out)
	}

	/// The function arguments a field was given. A connection argument (`filter`, `first`, ...)
	/// is read by the connection, not passed to the function; it may hold an omitted variable,
	/// which is not an error.
	fn call_args(&self, field: &FieldDef, qf: &QField<'d>) -> Result<Vec<CallArg>> {
		let mut args = vec![];
		for def in &field.args {
			let InputKind::FunctionArg {
				name,
				position,
				type_name,
			} = &def.kind
			else {
				continue;
			};
			let value = self.arg(field, qf, &def.name)?;
			if value.is_absent() {
				continue;
			}
			args.push(CallArg {
				name: name.clone(),
				position: *position,
				type_name: type_name.clone(),
				value: value.to_json()?,
			});
		}
		Ok(args)
	}

	pub fn function_call(
		&self,
		field: &FieldDef,
		qf: &QField<'d>,
		function: &Rc<Function>,
		returns: &Returns,
	) -> Result<FunctionCall> {
		let allowed: Vec<&str> = field.args.iter().map(|a| a.name.as_str()).collect();
		self.restrict(&allowed, qf)?;
		// Every argument of a function field is read against its type first, a collection's
		// included, as upstream reads a function call's arguments.
		for a in &field.args {
			self.arg(field, qf, &a.name)?;
		}
		let args = self.call_args(field, qf)?;
		let call = Call {
			function: Rc::clone(function),
			args,
		};
		let returns = match returns {
			Returns::Scalar | Returns::List => CallReturns::Scalar,
			Returns::Enum if function.shapes => CallReturns::Scalar,
			Returns::Node(t) => CallReturns::Node(self.node(field, qf, t, None, &allowed)?),
			Returns::Connection(t) => CallReturns::Connection(self.connection(
				field,
				qf,
				t,
				Rows::Call(call.clone()),
				&allowed,
			)?),
			Returns::Composite => {
				CallReturns::Composite(self.attr_selections(field.ty.base(), qf)?)
			}
			Returns::Enum => {
				let name = self.type_name(field.ty.base());
				return Err(Error::new(format!("unsupported return type: {name}")));
			}
		};
		Ok(FunctionCall { call, returns })
	}
}

fn is_filter_op(op: &str) -> bool {
	matches!(
		op,
		"eq" | "neq"
			| "lt" | "lte"
			| "gt" | "gte"
			| "in" | "is"
			| "startsWith"
			| "like" | "ilike"
			| "regex" | "iregex"
			| "contains"
			| "containedBy"
			| "overlaps"
			| "intersects"
			| "within"
			| "dWithin"
	)
}
