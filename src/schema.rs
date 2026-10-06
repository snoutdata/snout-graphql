//! The GraphQL schema a role sees, derived from the catalogue.
//!
//! Every named type is registered when the schema is built, which only costs a name per type, and
//! each type's fields are worked out the first time something asks for them and kept. A request
//! that touches three tables of a two-thousand-table database builds the fields of those three.
use crate::catalog::{
	Attr, Catalog, Category, Column, EnumInfo, ForeignKey, Function, Index, Table, Volatility,
};
use std::cell::OnceCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

pub type TypeId = usize;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypeRef {
	/// A named type. `max_len` is a character column's limit, which input values are held to; it is
	/// not part of the type's name.
	Named {
		id: TypeId,
		max_len: Option<i32>,
	},
	List(Box<TypeRef>),
	NonNull(Box<TypeRef>),
}

impl TypeRef {
	pub fn named(id: TypeId) -> Self {
		TypeRef::Named { id, max_len: None }
	}

	pub fn non_null(self) -> Self {
		match self {
			TypeRef::NonNull(_) => self,
			other => TypeRef::NonNull(Box::new(other)),
		}
	}

	pub fn list(self) -> Self {
		TypeRef::List(Box::new(self))
	}

	pub fn nullable(&self) -> &TypeRef {
		match self {
			TypeRef::NonNull(inner) => inner,
			other => other,
		}
	}

	/// The named type under any list and non-null wrappers.
	pub fn base(&self) -> TypeId {
		match self {
			TypeRef::Named { id, .. } => *id,
			TypeRef::List(inner) | TypeRef::NonNull(inner) => inner.base(),
		}
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scalar {
	ID,
	Int,
	Float,
	String,
	Boolean,
	Date,
	Time,
	Datetime,
	BigInt,
	Uuid,
	Json,
	Cursor,
	BigFloat,
	Opaque,
	/// A PostGIS `geometry` or `geography` as GeoJSON (`Extras::postgis`). Not in `ALL`: it is a
	/// type of the schema only where a directive asks for it.
	GeoJson,
}

impl Scalar {
	pub const ALL: [Scalar; 14] = [
		Scalar::ID,
		Scalar::Int,
		Scalar::Float,
		Scalar::String,
		Scalar::Boolean,
		Scalar::Date,
		Scalar::Time,
		Scalar::Datetime,
		Scalar::BigInt,
		Scalar::Uuid,
		Scalar::Json,
		Scalar::Cursor,
		Scalar::BigFloat,
		Scalar::Opaque,
	];

	pub fn name(self) -> &'static str {
		match self {
			Scalar::ID => "ID",
			Scalar::Int => "Int",
			Scalar::Float => "Float",
			Scalar::String => "String",
			Scalar::Boolean => "Boolean",
			Scalar::Date => "Date",
			Scalar::Time => "Time",
			Scalar::Datetime => "Datetime",
			Scalar::BigInt => "BigInt",
			Scalar::Uuid => "UUID",
			Scalar::Json => "JSON",
			Scalar::Cursor => "Cursor",
			Scalar::BigFloat => "BigFloat",
			Scalar::Opaque => "Opaque",
			Scalar::GeoJson => "GeoJSON",
		}
	}

	fn description(self) -> &'static str {
		match self {
			Scalar::ID => "A globally unique identifier for a given record",
			Scalar::Int => "A scalar integer up to 32 bits",
			Scalar::Float => "A scalar floating point value up to 32 bits",
			Scalar::String => "A string",
			Scalar::Boolean => "A value that is true or false",
			Scalar::BigInt => "An arbitrary size integer represented as a string",
			Scalar::Date => "A date without time information",
			Scalar::Time => "A time without date information",
			Scalar::Datetime => "A date and time",
			Scalar::Uuid => "A universally unique identifier",
			Scalar::Json => "A Javascript Object Notation value serialized as a string",
			Scalar::Cursor => {
				"An opaque string using for tracking a position in results during pagination"
			}
			Scalar::BigFloat => "A high precision floating point value represented as a string",
			Scalar::Opaque => "Any type not handled by the type system",
			Scalar::GeoJson => "A GeoJSON geometry, as an object or as a string",
		}
	}

	/// The comparison operators a filter on this scalar offers, in the order they are listed.
	pub fn filter_ops(self) -> &'static [&'static str] {
		const ORDERED: &[&str] = &["eq", "neq", "lt", "lte", "gt", "gte", "in", "is"];
		match self {
			Scalar::ID => &["eq"],
			Scalar::Uuid => &["eq", "neq", "in", "is"],
			Scalar::Boolean => &["eq", "is"],
			Scalar::Opaque => &["eq", "is"],
			Scalar::GeoJson => &["intersects", "contains", "within", "dWithin", "is"],
			Scalar::String => &[
				"eq",
				"neq",
				"lt",
				"lte",
				"gt",
				"gte",
				"in",
				"is",
				"startsWith",
				"like",
				"ilike",
				"regex",
				"iregex",
			],
			Scalar::Json | Scalar::Cursor => &[],
			_ => ORDERED,
		}
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
	Scalar,
	Object,
	Interface,
	Enum,
	InputObject,
}

impl Kind {
	pub fn as_str(self) -> &'static str {
		match self {
			Kind::Scalar => "SCALAR",
			Kind::Object => "OBJECT",
			Kind::Interface => "INTERFACE",
			Kind::Enum => "ENUM",
			Kind::InputObject => "INPUT_OBJECT",
		}
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggOp {
	Sum,
	Avg,
	Min,
	Max,
}

impl AggOp {
	pub fn sql(self) -> &'static str {
		match self {
			AggOp::Sum => "sum",
			AggOp::Avg => "avg",
			AggOp::Min => "min",
			AggOp::Max => "max",
		}
	}

	fn type_part(self) -> &'static str {
		match self {
			AggOp::Sum => "Sum",
			AggOp::Avg => "Avg",
			AggOp::Min => "Min",
			AggOp::Max => "Max",
		}
	}

	fn term(self) -> &'static str {
		match self {
			AggOp::Sum => "summation",
			AggOp::Avg => "average",
			AggOp::Min => "minimum",
			AggOp::Max => "maximum",
		}
	}

	fn capital_term(self) -> &'static str {
		match self {
			AggOp::Sum => "Sum",
			AggOp::Avg => "Average",
			AggOp::Min => "Minimum",
			AggOp::Max => "Maximum",
		}
	}
}

/// The introspection types, which describe the schema itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Meta {
	TypeKind,
	Schema,
	Type,
	Field,
	InputValue,
	EnumValue,
	DirectiveLocation,
	Directive,
}

/// What a named type stands for.
#[derive(Clone, Debug)]
pub enum Source {
	Scalar(Scalar),
	Query,
	Mutation,
	NodeInterface,
	PageInfo,
	FilterIs,
	OrderByDirection,
	Meta(Meta),
	Node(Rc<Table>),
	Edge(Rc<Table>),
	Connection(Rc<Table>),
	FilterEntity(Rc<Table>),
	/// `some`, `every` and `none` over a table's rows, for a relation filter (`Extras`).
	CollectionFilter(Rc<Table>),
	/// An insert's `onConflict` (`Extras::upsert`).
	OnConflict(Rc<Table>),
	/// `count` over a table's rows, for ordering by a to-many relation (`Extras`).
	CollectionOrderBy(Rc<Table>),
	/// An enum of our additions, its values read from the table.
	ExtraEnum(Rc<Table>, ExtraEnum),
	OrderByEntity(Rc<Table>),
	InsertInput(Rc<Table>),
	InsertResponse(Rc<Table>),
	UpdateInput(Rc<Table>),
	UpdateResponse(Rc<Table>),
	DeleteResponse(Rc<Table>),
	Aggregate(Rc<Table>),
	AggregateNumeric(Rc<Table>, AggOp),
	Enum(Rc<EnumInfo>),
	FilterScalar(Scalar),
	FilterList(Scalar),
	FilterEnum(TypeId),
	/// `dWithin`'s argument: a geometry and a distance (`Extras::postgis`).
	GeoDistance,
	/// `<Enum>ListFilter`, for a column holding an array of an enum (`Extras::enum_arrays`).
	FilterEnumList(TypeId),
	/// A composite type as an object type, and the filter on its attributes (`Extras::composites`).
	Composite(u32),
	CompositeFilter(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtraEnum {
	/// The unique indexes an upsert may name: `<Table>UniqueConstraint`.
	UniqueConstraint,
	/// The columns an upsert may update: `<Table>UpdateField`.
	UpdateField,
	/// The columns `distinctOn` may name: `<Table>Field`.
	Field,
}

/// What a function returns, as far as resolving it is concerned.
#[derive(Clone, Debug)]
pub enum Returns {
	Scalar,
	/// An enum, which a computed field cannot return (resolving one is an error, as upstream).
	Enum,
	List,
	Node(Rc<Table>),
	Connection(Rc<Table>),
	/// A composite type's value (`Extras::composites`).
	Composite,
}

#[derive(Clone, Debug)]
pub enum FieldKind {
	/// `node(nodeId:)` on Query.
	NodeEntry,
	Collection(Rc<Table>),
	ByPk(Rc<Table>),
	QueryFunction(Rc<Function>, Returns),
	MutationFunction(Rc<Function>, Returns),
	IntroType,
	IntroSchema,
	Insert(Rc<Table>),
	Update(Rc<Table>),
	Delete(Rc<Table>),
	Column(Rc<Column>),
	NodeId(Rc<Table>),
	Computed(Rc<Function>, Returns),
	RelationOne {
		key: Rc<ForeignKey>,
		reverse: bool,
		table: Rc<Table>,
	},
	RelationMany {
		key: Rc<ForeignKey>,
		table: Rc<Table>,
	},
	Edges,
	PageInfo,
	TotalCount,
	Aggregate,
	Cursor,
	EdgeNode,
	StartCursor,
	EndCursor,
	HasNextPage,
	HasPreviousPage,
	AffectedCount,
	Records,
	AggCount,
	AggOp(AggOp),
	AggColumn(Rc<Column>),
	/// An attribute of a composite value.
	Attribute(Rc<Attr>),
	/// A field of an introspection type, resolved by name.
	Meta,
}

#[derive(Clone, Debug)]
pub struct FieldDef {
	pub name: String,
	pub description: Option<String>,
	pub ty: TypeRef,
	pub args: Vec<InputDef>,
	pub kind: FieldKind,
	/// The schema a function field belongs to, which decides whether introspection shows it.
	pub function_schema: Option<u32>,
}

#[derive(Clone, Debug)]
pub enum InputKind {
	Plain,
	Column(Rc<Column>),
	/// The `nodeId` filter.
	NodeId,
	/// A filter on the rows a foreign key relates (`Extras::relation_filters`).
	Relation {
		key: Rc<ForeignKey>,
		reverse: bool,
		table: Rc<Table>,
		many: bool,
	},
	/// A function argument: its SQL name (none for an argument that has none, passed by
	/// position), its position, and the type its value is cast to.
	FunctionArg {
		name: Option<String>,
		position: usize,
		type_name: String,
	},
	/// A computed field in a filter (`Extras::function_shapes`).
	Computed(Rc<Function>),
	/// An attribute in a composite's filter.
	Attribute(Rc<Attr>),
}

#[derive(Clone, Debug)]
pub struct InputDef {
	pub name: String,
	pub description: Option<String>,
	pub ty: TypeRef,
	pub default_value: Option<String>,
	pub kind: InputKind,
}

impl InputDef {
	fn plain(name: &str, ty: TypeRef, description: Option<&str>) -> Self {
		InputDef {
			name: name.to_string(),
			description: description.map(str::to_string),
			ty,
			default_value: None,
			kind: InputKind::Plain,
		}
	}
}

pub struct TypeDef {
	pub name: String,
	pub kind: Kind,
	pub source: Source,
	/// Whether it is in the schema's list of types. A type can be reachable without being listed
	/// (an enum whose type the role may not use), as upstream behaviour has it.
	pub listed: bool,
	fields: OnceCell<Option<Vec<FieldDef>>>,
	inputs: OnceCell<Option<Vec<InputDef>>>,
}

pub struct Schema {
	/// Unique among the schemas this connection has built, so what was computed from one schema
	/// is never taken for another's (`answers.rs`).
	pub id: u64,
	pub catalog: Catalog,
	types: Vec<TypeDef>,
	by_name: HashMap<String, TypeId>,
	scalars: HashMap<Scalar, TypeId>,
	nodes: HashMap<u32, TypeId>,
	table_types: HashMap<(u32, String), TypeId>,
	tables_listed: HashSet<u32>,
	enums: HashMap<u32, TypeId>,
	filter_scalars: HashMap<Scalar, TypeId>,
	filter_lists: HashMap<Scalar, TypeId>,
	filter_enums: HashMap<u32, TypeId>,
	filter_enum_lists: HashMap<u32, TypeId>,
	composite_types: HashMap<u32, TypeId>,
	composite_filters: HashMap<u32, TypeId>,
	pub query: TypeId,
	pub mutation: OnceCell<Option<TypeId>>,
	mutation_id: TypeId,
	pub node_interface: TypeId,
	pub page_info: TypeId,
	pub filter_is: TypeId,
	pub order_by_direction: TypeId,
	meta: HashMap<&'static str, TypeId>,
}

pub fn is_valid_name(name: &str) -> bool {
	let mut chars = name.chars();
	match chars.next() {
		Some(c) if c == '_' || c.is_ascii_alphabetic() => {
			chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
		}
		_ => false,
	}
}

/// The type name for a table or enum: the override if there is one, else the name itself, else
/// (with `inflect_names`) the name with each word capitalised and the underscores removed.
pub fn base_name(name: &str, name_override: Option<&str>, inflect: bool) -> String {
	if let Some(o) = name_override {
		return o.to_string();
	}
	if !inflect {
		return name.to_string();
	}
	let mut out = String::with_capacity(name.len());
	let mut prev_alnum = false;
	for c in name.chars() {
		if prev_alnum {
			out.push(c);
		} else {
			out.extend(c.to_uppercase());
		}
		prev_alnum = c.is_alphanumeric();
	}
	out.replace('_', "")
}

pub fn lower_first(s: &str) -> String {
	let mut chars = s.chars();
	match chars.next() {
		Some(c) => c.to_lowercase().chain(chars).collect(),
		None => String::new(),
	}
}

const CONNECTION_ARG_NAMES: [&str; 7] = [
	"first", "last", "before", "after", "offset", "filter", "orderBy",
];

fn is_numeric_type(n: &str) -> bool {
	matches!(
		n,
		"int2" | "int4" | "int8" | "float4" | "float8" | "numeric" | "decimal" | "money"
	)
}
fn is_string_type(n: &str) -> bool {
	matches!(
		n,
		"text" | "varchar" | "char" | "bpchar" | "name" | "citext"
	)
}
fn is_datetime_type(n: &str) -> bool {
	matches!(n, "date" | "time" | "timetz" | "timestamp" | "timestamptz")
}

impl Schema {
	pub fn build(catalog: Catalog) -> Schema {
		thread_local! {
			static BUILT: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
		}
		let id = BUILT.with(|n| {
			n.set(n.get() + 1);
			n.get()
		});
		let mut s = Schema {
			id,
			catalog,
			types: vec![],
			by_name: HashMap::new(),
			scalars: HashMap::new(),
			nodes: HashMap::new(),
			table_types: HashMap::new(),
			tables_listed: HashSet::new(),
			enums: HashMap::new(),
			filter_scalars: HashMap::new(),
			filter_lists: HashMap::new(),
			filter_enums: HashMap::new(),
			filter_enum_lists: HashMap::new(),
			composite_types: HashMap::new(),
			composite_filters: HashMap::new(),
			query: 0,
			mutation: OnceCell::new(),
			mutation_id: 0,
			node_interface: 0,
			page_info: 0,
			filter_is: 0,
			order_by_direction: 0,
			meta: HashMap::new(),
		};
		for (name, meta) in [
			("__TypeKind", Meta::TypeKind),
			("__Schema", Meta::Schema),
			("__Type", Meta::Type),
			("__Field", Meta::Field),
			("__InputValue", Meta::InputValue),
			("__EnumValue", Meta::EnumValue),
			("__DirectiveLocation", Meta::DirectiveLocation),
			("__Directive", Meta::Directive),
		] {
			let kind = match meta {
				Meta::TypeKind | Meta::DirectiveLocation => Kind::Enum,
				_ => Kind::Object,
			};
			let id = s.add(name.to_string(), kind, Source::Meta(meta), true);
			s.meta.insert(name, id);
		}
		s.page_info = s.add("PageInfo".into(), Kind::Object, Source::PageInfo, true);
		for scalar in Scalar::ALL {
			let id = s.add(
				scalar.name().into(),
				Kind::Scalar,
				Source::Scalar(scalar),
				true,
			);
			s.scalars.insert(scalar, id);
		}
		s.filter_is = s.add("FilterIs".into(), Kind::Enum, Source::FilterIs, true);
		s.order_by_direction = s.add(
			"OrderByDirection".into(),
			Kind::Enum,
			Source::OrderByDirection,
			true,
		);
		for scalar in [
			Scalar::ID,
			Scalar::Int,
			Scalar::Float,
			Scalar::String,
			Scalar::Boolean,
			Scalar::Date,
			Scalar::Time,
			Scalar::Datetime,
			Scalar::BigInt,
			Scalar::Uuid,
			Scalar::BigFloat,
			Scalar::Opaque,
		] {
			let id = s.add(
				format!("{}Filter", scalar.name()),
				Kind::InputObject,
				Source::FilterScalar(scalar),
				true,
			);
			s.filter_scalars.insert(scalar, id);
		}
		for scalar in [
			Scalar::Int,
			Scalar::Float,
			Scalar::String,
			Scalar::Boolean,
			Scalar::Date,
			Scalar::Time,
			Scalar::Datetime,
			Scalar::BigInt,
			Scalar::Uuid,
			Scalar::BigFloat,
		] {
			let id = s.add(
				format!("{}ListFilter", scalar.name()),
				Kind::InputObject,
				Source::FilterList(scalar),
				true,
			);
			s.filter_lists.insert(scalar, id);
		}
		s.query = s.add("Query".into(), Kind::Object, Source::Query, true);
		s.node_interface = s.add("Node".into(), Kind::Interface, Source::NodeInterface, true);
		// Listed only if it turns out to have fields; decided when first asked.
		s.mutation_id = s.add("Mutation".into(), Kind::Object, Source::Mutation, false);

		let tables = s.catalog.tables.clone();
		for table in &tables {
			let listed = s.table_is_selectable(table);
			if listed {
				s.tables_listed.insert(table.oid);
			}
			let base = s.table_name(table);
			let node = s.add(
				base.clone(),
				Kind::Object,
				Source::Node(Rc::clone(table)),
				listed,
			);
			s.nodes.insert(table.oid, node);
			s.add(
				format!("{base}Edge"),
				Kind::Object,
				Source::Edge(Rc::clone(table)),
				listed,
			);
			s.add(
				format!("{base}Connection"),
				Kind::Object,
				Source::Connection(Rc::clone(table)),
				listed,
			);
			s.add(
				format!("{base}Filter"),
				Kind::InputObject,
				Source::FilterEntity(Rc::clone(table)),
				listed,
			);
			s.add(
				format!("{base}OrderBy"),
				Kind::InputObject,
				Source::OrderByEntity(Rc::clone(table)),
				listed,
			);
			let insert = listed && table.columns.iter().any(|c| c.insertable);
			let update = listed && table.columns.iter().any(|c| c.updatable);
			let delete = listed && table.deletable;
			s.add(
				format!("{base}InsertInput"),
				Kind::InputObject,
				Source::InsertInput(Rc::clone(table)),
				insert,
			);
			s.add(
				format!("{base}InsertResponse"),
				Kind::Object,
				Source::InsertResponse(Rc::clone(table)),
				insert,
			);
			s.add(
				format!("{base}UpdateInput"),
				Kind::InputObject,
				Source::UpdateInput(Rc::clone(table)),
				update,
			);
			s.add(
				format!("{base}UpdateResponse"),
				Kind::Object,
				Source::UpdateResponse(Rc::clone(table)),
				update,
			);
			s.add(
				format!("{base}DeleteResponse"),
				Kind::Object,
				Source::DeleteResponse(Rc::clone(table)),
				delete,
			);
			let aggregate = listed && table.aggregate;
			s.add(
				format!("{base}Aggregate"),
				Kind::Object,
				Source::Aggregate(Rc::clone(table)),
				aggregate,
			);
			let sums = aggregate && table.columns.iter().any(|c| s.aggregatable(c, AggOp::Sum));
			let mins = aggregate && table.columns.iter().any(|c| s.aggregatable(c, AggOp::Min));
			for (op, listed) in [
				(AggOp::Sum, sums),
				(AggOp::Avg, sums),
				(AggOp::Min, mins),
				(AggOp::Max, mins),
			] {
				s.add(
					format!("{base}{}AggregateResult", op.type_part()),
					Kind::Object,
					Source::AggregateNumeric(Rc::clone(table), op),
					listed,
				);
			}
		}

		// Our additions' types, only where a directive asks for them, so a schema that asks for
		// none has exactly upstream's types.
		let mut collection_filters: HashSet<u32> = HashSet::new();
		let mut collection_orders: HashSet<u32> = HashSet::new();
		for table in tables.iter().filter(|t| s.table_listed(t)) {
			for key in s
				.catalog
				.foreign_keys
				.iter()
				.filter(|k| k.referenced.oid == table.oid)
			{
				if !s.catalog.key_is_locally_unique(key) {
					if table.extras.relation_filters {
						collection_filters.insert(key.local.oid);
					}
					if table.extras.order_by_related {
						collection_orders.insert(key.local.oid);
					}
				}
			}
		}
		for table in tables.iter().filter(|t| collection_orders.contains(&t.oid)) {
			let listed = s.table_listed(table);
			let base = s.table_name(table);
			s.add(
				format!("{base}CollectionOrderBy"),
				Kind::InputObject,
				Source::CollectionOrderBy(Rc::clone(table)),
				listed,
			);
		}
		for table in &tables {
			if !table.extras.upsert
				|| !s.table_listed(table)
				|| !table.columns.iter().any(|c| c.insertable)
				|| s.upsert_indexes(table).is_empty()
			{
				continue;
			}
			let base = s.table_name(table);
			s.add(
				format!("{base}OnConflict"),
				Kind::InputObject,
				Source::OnConflict(Rc::clone(table)),
				true,
			);
			s.add(
				format!("{base}UniqueConstraint"),
				Kind::Enum,
				Source::ExtraEnum(Rc::clone(table), ExtraEnum::UniqueConstraint),
				true,
			);
			if !s.upsert_fields(table).is_empty() {
				s.add(
					format!("{base}UpdateField"),
					Kind::Enum,
					Source::ExtraEnum(Rc::clone(table), ExtraEnum::UpdateField),
					true,
				);
			}
		}
		for table in &tables {
			if table.extras.distinct_on
				&& s.table_listed(table)
				&& !s.distinct_fields(table).is_empty()
			{
				let base = s.table_name(table);
				s.add(
					format!("{base}Field"),
					Kind::Enum,
					Source::ExtraEnum(Rc::clone(table), ExtraEnum::Field),
					true,
				);
			}
		}
		for table in tables
			.iter()
			.filter(|t| collection_filters.contains(&t.oid))
		{
			let listed = s.table_listed(table);
			let base = s.table_name(table);
			s.add(
				format!("{base}CollectionFilter"),
				Kind::InputObject,
				Source::CollectionFilter(Rc::clone(table)),
				listed,
			);
		}

		let mut enum_arrays: HashSet<u32> = HashSet::new();
		for table in tables.iter().filter(|t| t.extras.enum_arrays) {
			for c in &table.columns {
				if let Some(e) = s.catalog.types.get(&c.type_oid).and_then(|t| t.element)
					&& s.catalog.enums.contains_key(&e)
				{
					enum_arrays.insert(e);
				}
			}
		}
		let enums: Vec<Rc<EnumInfo>> = s.catalog.enums.values().cloned().collect();
		for e in enums {
			if !s.catalog.schemas.contains_key(&e.schema_oid) {
				continue;
			}
			let name = base_name(
				&e.name,
				e.name_override.as_deref(),
				s.catalog.inflect(e.schema_oid),
			);
			let listed = e.usable;
			let id = s.add(
				name.clone(),
				Kind::Enum,
				Source::Enum(Rc::clone(&e)),
				listed,
			);
			s.enums.insert(e.oid, id);
			let filter = s.add(
				format!("{name}Filter"),
				Kind::InputObject,
				Source::FilterEnum(id),
				listed,
			);
			s.filter_enums.insert(e.oid, filter);
			if enum_arrays.contains(&e.oid) {
				let list = s.add(
					format!("{name}ListFilter"),
					Kind::InputObject,
					Source::FilterEnumList(id),
					listed,
				);
				s.filter_enum_lists.insert(e.oid, list);
			}
		}
		if s.catalog.postgis_schema.is_some() {
			let id = s.add(
				"GeoJSON".into(),
				Kind::Scalar,
				Source::Scalar(Scalar::GeoJson),
				true,
			);
			s.scalars.insert(Scalar::GeoJson, id);
			let filter = s.add(
				"GeoJSONFilter".into(),
				Kind::InputObject,
				Source::FilterScalar(Scalar::GeoJson),
				true,
			);
			s.filter_scalars.insert(Scalar::GeoJson, filter);
			s.add(
				"GeoJSONDistance".into(),
				Kind::InputObject,
				Source::GeoDistance,
				true,
			);
		}
		if s.catalog.schemas.values().any(|x| x.extras.composites) {
			let mut oids: Vec<u32> = s.catalog.composite_attrs.keys().copied().collect();
			oids.sort_unstable();
			for oid in oids {
				let Some(info) = s.catalog.types.get(&oid) else {
					continue;
				};
				let schema_oid = s.catalog.composite_attrs[&oid]
					.first()
					.map(|a| a.schema_oid)
					.unwrap_or(0);
				let name = base_name(&info.name, None, s.catalog.inflect(schema_oid));
				let listed = info.usable;
				let object = s.add(name.clone(), Kind::Object, Source::Composite(oid), listed);
				s.composite_types.insert(oid, object);
				let filter = s.add(
					format!("{name}Filter"),
					Kind::InputObject,
					Source::CompositeFilter(oid),
					listed,
				);
				s.composite_filters.insert(oid, filter);
			}
		}
		s
	}

	fn add(&mut self, name: String, kind: Kind, source: Source, listed: bool) -> TypeId {
		let id = self.types.len();
		if let Some(table) = source_table(&source) {
			let base = self.table_name(table);
			let suffix = name.strip_prefix(base.as_str()).unwrap_or("").to_string();
			self.table_types.insert((table.oid, suffix), id);
		}
		// A listed type wins the name; among equals, the first registered.
		match self.by_name.get(&name) {
			Some(&existing) if self.types[existing].listed || !listed => {}
			_ => {
				self.by_name.insert(name.clone(), id);
			}
		}
		self.types.push(TypeDef {
			name,
			kind,
			source,
			listed,
			fields: OnceCell::new(),
			inputs: OnceCell::new(),
		});
		id
	}

	pub fn ty(&self, id: TypeId) -> &TypeDef {
		&self.types[id]
	}

	pub fn lookup(&self, name: &str) -> Option<TypeId> {
		self.by_name.get(name).copied()
	}

	pub fn meta(&self, name: &str) -> TypeId {
		self.meta[name]
	}

	pub fn scalar(&self, s: Scalar) -> TypeRef {
		TypeRef::named(self.scalars[&s])
	}

	fn string_ref(&self, max_len: Option<i32>) -> TypeRef {
		TypeRef::Named {
			id: self.scalars[&Scalar::String],
			max_len,
		}
	}

	/// The type listed in `__schema { types }`, in the order it lists them.
	pub fn listed_types(&self) -> Vec<TypeId> {
		let mutation = self.mutation_type();
		let mut ids: Vec<TypeId> = (0..self.types.len())
			.filter(|&id| self.types[id].listed || Some(id) == mutation)
			.filter(|&id| self.by_name.get(&self.types[id].name) == Some(&id))
			.collect();
		ids.sort_by(|a, b| self.types[*a].name.cmp(&self.types[*b].name));
		ids
	}

	pub fn mutation_type(&self) -> Option<TypeId> {
		*self.mutation.get_or_init(|| {
			let has_fields = self
				.fields(self.mutation_id)
				.map(|f| !f.is_empty())
				.unwrap_or(false);
			has_fields.then_some(self.mutation_id)
		})
	}

	/// The schema a type belongs to, for deciding whether introspection may show it.
	pub fn type_schema(&self, id: TypeId) -> Option<u32> {
		match &self.types[id].source {
			Source::Node(t)
			| Source::Edge(t)
			| Source::Connection(t)
			| Source::FilterEntity(t)
			| Source::CollectionFilter(t)
			| Source::OnConflict(t)
			| Source::CollectionOrderBy(t)
			| Source::ExtraEnum(t, _)
			| Source::OrderByEntity(t)
			| Source::InsertInput(t)
			| Source::InsertResponse(t)
			| Source::UpdateInput(t)
			| Source::UpdateResponse(t)
			| Source::DeleteResponse(t)
			| Source::Aggregate(t)
			| Source::AggregateNumeric(t, _) => Some(t.schema_oid),
			Source::Enum(e) => Some(e.schema_oid),
			// An enum filter belongs to no schema, so introspection always shows it, as upstream does.
			_ => None,
		}
	}

	pub fn introspectable(&self, id: TypeId) -> bool {
		match self.type_schema(id) {
			Some(oid) => self.catalog.introspection_in(oid),
			None => true,
		}
	}

	pub fn description(&self, id: TypeId) -> Option<String> {
		let t = &self.types[id];
		Some(match &t.source {
			Source::Scalar(s) => s.description().to_string(),
			Source::Query => "The root type for querying data".into(),
			Source::Mutation => "The root type for creating and mutating data".into(),
			Source::OrderByDirection => "Defines a per-field sorting order".into(),
			Source::Node(table) => return table.description.clone(),
			Source::FilterScalar(_)
			| Source::FilterList(_)
			| Source::FilterEnum(_)
			| Source::FilterEnumList(_) => {
				let entity = t.name.strip_suffix("Filter").unwrap_or(&t.name);
				format!("Boolean expression comparing fields on type \"{entity}\"")
			}
			Source::Aggregate(table) => {
				format!("Aggregate results for `{}`", self.table_name(table))
			}
			Source::CollectionOrderBy(table) => format!(
				"Orders by how many related rows of `{}` there are",
				self.table_name(table)
			),
			Source::OnConflict(table) => format!(
				"What to do when a row inserted into `{}` has the same key as one already there",
				self.table_name(table)
			),
			Source::ExtraEnum(table, ExtraEnum::UniqueConstraint) => format!(
				"The unique keys of `{}`, by index name",
				self.table_name(table)
			),
			Source::ExtraEnum(table, ExtraEnum::UpdateField) => format!(
				"The fields of `{}` an upsert can update",
				self.table_name(table)
			),
			Source::ExtraEnum(table, ExtraEnum::Field) => format!(
				"The fields of `{}` a collection can be made distinct on",
				self.table_name(table)
			),
			Source::CollectionFilter(table) => {
				format!(
					"Compares the related rows of `{}`: whether some, every or none of them match",
					self.table_name(table)
				)
			}
			Source::AggregateNumeric(table, op) => {
				format!(
					"Result of {} aggregation for `{}`",
					op.term(),
					self.table_name(table)
				)
			}
			Source::Meta(m) => meta_description(*m).to_string(),
			_ => return None,
		})
	}

	// ----- names ------------------------------------------------------------------------------

	pub fn table_name(&self, table: &Table) -> String {
		base_name(
			&table.name,
			table.name_override.as_deref(),
			self.catalog.inflect(table.schema_oid),
		)
	}

	pub fn column_name(&self, column: &Column) -> String {
		if let Some(o) = &column.name_override {
			return o.clone();
		}
		let inflect = self.catalog.inflect(column.schema_oid);
		let base = base_name(&column.name, None, inflect);
		if inflect { lower_first(&base) } else { base }
	}

	fn function_name(&self, f: &Function) -> String {
		if let Some(o) = &f.name_override {
			return o.clone();
		}
		let trimmed = f.name.strip_prefix('_').unwrap_or(&f.name);
		lower_first(&base_name(
			trimmed,
			None,
			self.catalog.inflect(f.schema_oid),
		))
	}

	fn function_arg_name(&self, f: &Function, arg: &str) -> String {
		lower_first(&base_name(arg, None, self.catalog.inflect(f.schema_oid)))
	}

	fn key_field_name(&self, key: &ForeignKey, reverse: bool) -> String {
		let (side, name_override, unique, columns) = if reverse {
			(
				&key.local,
				&key.local_name,
				self.catalog.key_is_locally_unique(key),
				&key.referenced.columns,
			)
		} else {
			(&key.referenced, &key.foreign_name, true, &key.local.columns)
		};
		if let Some(o) = name_override {
			return o.clone();
		}
		let Some(table) = self.catalog.table(side.oid) else {
			return String::new();
		};
		let inflect = self.catalog.inflect(table.schema_oid);
		let base = base_name(&table.name, table.name_override.as_deref(), inflect);
		let base_field = lower_first(&base);
		let singular = match columns.as_slice() {
			[column] => {
				let suffix = if inflect { "_id" } else { "Id" };
				match column.strip_suffix(suffix) {
					Some(stripped) => lower_first(&base_name(stripped, None, inflect)),
					None => base_field.clone(),
				}
			}
			_ => base_field.clone(),
		};
		if unique {
			singular
		} else {
			format!("{base_field}Collection")
		}
	}

	// ----- what a table offers ----------------------------------------------------------------

	pub fn table_is_selectable(&self, table: &Table) -> bool {
		is_valid_name(&self.table_name(table))
			&& table.primary_key().is_some()
			&& table.columns.iter().any(|c| c.selectable)
	}

	pub fn table_listed(&self, table: &Table) -> bool {
		self.tables_listed.contains(&table.oid)
	}

	fn type_of(&self, table: &Table, suffix: &str) -> TypeId {
		// Registered in `build` for every table, keyed by the table, so a type of the same name
		// from another table can never be mixed up with it.
		self.table_types
			.get(&(table.oid, suffix.to_string()))
			.copied()
			.unwrap_or(self.query)
	}

	pub fn node_type(&self, table: &Table) -> TypeId {
		self.nodes
			.get(&table.oid)
			.copied()
			.unwrap_or_else(|| self.type_of(table, ""))
	}

	/// The unique indexes an upsert may name: whole-table, immediate, columns only, and named
	/// with a GraphQL name.
	pub fn upsert_indexes(&self, table: &Table) -> Vec<Index> {
		table
			.indexes
			.iter()
			.filter(|i| i.unique && i.plain && !i.columns.is_empty() && is_valid_name(&i.name))
			.cloned()
			.collect()
	}

	/// The columns an upsert may set from the row that conflicted: those an update may.
	pub fn upsert_fields(&self, table: &Table) -> Vec<Rc<Column>> {
		self.write_inputs(table, |c| c.updatable)
			.into_iter()
			.filter_map(|i| match i.kind {
				InputKind::Column(c) => Some(c),
				_ => None,
			})
			.collect()
	}

	/// A table's `<Table>OrderBy`.
	pub fn order_by_entity(&self, table: &Table) -> TypeId {
		self.type_of(table, "OrderBy")
	}

	/// A table's `<Table>Filter`.
	pub fn node_filter(&self, table: &Table) -> TypeId {
		self.type_of(table, "Filter")
	}

	pub fn connection_type(&self, table: &Table) -> TypeId {
		self.type_of(table, "Connection")
	}

	fn by_pk_supported(&self, table: &Table) -> bool {
		let cols = table.primary_key_columns();
		!cols.is_empty()
			&& cols.iter().all(|c| {
				matches!(
					c.type_name.as_str(),
					"int"
						| "int4" | "integer"
						| "bigint" | "int8"
						| "smallint" | "int2"
						| "text" | "varchar"
						| "char" | "bpchar"
						| "citext" | "uuid"
				)
			})
	}

	fn aggregatable(&self, column: &Column, op: AggOp) -> bool {
		let Some(t) = self.catalog.types.get(&column.type_oid) else {
			return false;
		};
		if t.category != Category::Other {
			return false;
		}
		match op {
			AggOp::Sum | AggOp::Avg => is_numeric_type(&t.name),
			AggOp::Min | AggOp::Max => {
				is_numeric_type(&t.name)
					|| is_string_type(&t.name)
					|| is_datetime_type(&t.name)
					|| t.name == "bool"
			}
		}
	}

	fn aggregate_result(&self, column: &Column, op: AggOp) -> Option<TypeRef> {
		let t = self.catalog.types.get(&column.type_oid)?;
		let n = t.name.as_str();
		let scalar = match op {
			AggOp::Sum if matches!(n, "int2" | "int4" | "int8") => Scalar::BigInt,
			AggOp::Sum | AggOp::Avg if is_numeric_type(n) => Scalar::BigFloat,
			AggOp::Sum | AggOp::Avg => return None,
			AggOp::Min | AggOp::Max if is_numeric_type(n) || is_datetime_type(n) => match n {
				"int2" | "int4" => Scalar::Int,
				"int8" => Scalar::BigInt,
				"float4" | "float8" | "numeric" | "decimal" => Scalar::BigFloat,
				"date" => Scalar::Date,
				"time" | "timetz" => Scalar::Time,
				"timestamp" | "timestamptz" => Scalar::Datetime,
				_ => Scalar::Opaque,
			},
			AggOp::Min | AggOp::Max if is_string_type(n) => {
				return Some(self.string_ref(column.max_characters));
			}
			AggOp::Min | AggOp::Max if n == "bool" => Scalar::Boolean,
			_ => return None,
		};
		Some(self.scalar(scalar))
	}

	// ----- SQL types to GraphQL types ---------------------------------------------------------

	/// The GraphQL type of a value of SQL type `oid`, if there is one.
	pub fn sql_type(&self, oid: u32, max_len: Option<i32>, set_of: bool) -> Option<TypeRef> {
		let t = self.catalog.types.get(&oid)?;
		if set_of && t.category != Category::Table {
			return None;
		}
		match t.category {
			Category::Other => Some(match t.oid {
				20 => self.scalar(Scalar::BigInt),
				16 => self.scalar(Scalar::Boolean),
				1082 => self.scalar(Scalar::Date),
				1184 | 1114 => self.scalar(Scalar::Datetime),
				701 | 700 => self.scalar(Scalar::Float),
				23 | 21 => self.scalar(Scalar::Int),
				3802 | 114 => self.scalar(Scalar::Json),
				1083 => self.scalar(Scalar::Time),
				2950 => self.scalar(Scalar::Uuid),
				1700 => self.scalar(Scalar::BigFloat),
				25 => self.scalar(Scalar::String),
				18 | 1042 | 1043 => self.string_ref(max_len),
				_ if t.name == "citext" => self.scalar(Scalar::String),
				_ if self.is_geo(t.oid) => self.scalar(Scalar::GeoJson),
				_ => self.scalar(Scalar::Opaque),
			}),
			Category::Array => {
				let element = t.element?;
				let inner = match self.catalog.types.get(&element) {
					// An array of composites stays out, as upstream has it.
					Some(e) if e.category == Category::Composite => return None,
					Some(e) if e.usable => self.sql_type(element, None, false)?,
					Some(_) => return None,
					None => self.scalar(Scalar::Opaque),
				};
				Some(inner.list())
			}
			Category::Enum => Some(match self.enums.get(&oid) {
				Some(&id) => TypeRef::named(id),
				None => self.scalar(Scalar::Opaque),
			}),
			Category::Table => {
				let table = self.catalog.table(t.table?)?;
				Some(TypeRef::named(if set_of {
					self.connection_type(table)
				} else {
					self.node_type(table)
				}))
			}
			Category::Composite => self.composite_types.get(&oid).map(|&id| TypeRef::named(id)),
		}
	}

	/// Whether a type is PostGIS's `geometry` or `geography`, read as GeoJSON here.
	pub fn is_geo(&self, oid: u32) -> bool {
		self.catalog.postgis_schema.is_some()
			&& self
				.catalog
				.types
				.get(&oid)
				.is_some_and(|t| matches!(t.name.as_str(), "geometry" | "geography"))
	}

	/// An attribute's field name, inflected as a column's is.
	pub fn attr_name(&self, attr: &Attr) -> String {
		let inflect = self.catalog.inflect(attr.schema_oid);
		let base = base_name(&attr.name, None, inflect);
		if inflect { lower_first(&base) } else { base }
	}

	/// Whether a column's composite type is reflected (`Extras::composites`).
	fn composite_reflected(&self, type_oid: u32) -> bool {
		self.composite_types.contains_key(&type_oid)
	}

	pub fn column_type(&self, column: &Column) -> Option<TypeRef> {
		let t = self.sql_type(column.type_oid, column.max_characters, false)?;
		Some(if column.not_null { t.non_null() } else { t })
	}

	fn returns(&self, f: &Function) -> Option<(TypeRef, Returns)> {
		let ty = self.sql_type(f.return_type, None, f.set_of)?;
		let returns = match &ty {
			TypeRef::List(_) => Returns::List,
			TypeRef::Named { id, .. } => match &self.types[*id].source {
				Source::Node(t) => Returns::Node(Rc::clone(t)),
				Source::Connection(t) => Returns::Connection(Rc::clone(t)),
				Source::Enum(_) => Returns::Enum,
				Source::Composite(_) => Returns::Composite,
				_ => Returns::Scalar,
			},
			TypeRef::NonNull(_) => Returns::Scalar,
		};
		Some((ty, returns))
	}

	// ----- fields -----------------------------------------------------------------------------

	pub fn fields(&self, id: TypeId) -> Option<&[FieldDef]> {
		self.types[id]
			.fields
			.get_or_init(|| self.build_fields(id))
			.as_deref()
	}

	pub fn inputs(&self, id: TypeId) -> Option<&[InputDef]> {
		self.types[id]
			.inputs
			.get_or_init(|| self.build_inputs(id))
			.as_deref()
	}

	/// A field by name. When two fields share a name (a column `count` and a computed field `_count`,
	/// or `_id` and `id` under inflection), the one defined later is the one a request reaches.
	pub fn field(&self, id: TypeId, name: &str) -> Option<&FieldDef> {
		self.fields(id)?.iter().rev().find(|f| f.name == name)
	}

	pub fn input(&self, id: TypeId, name: &str) -> Option<&InputDef> {
		self.inputs(id)?.iter().rev().find(|f| f.name == name)
	}

	fn mk(&self, name: &str, ty: TypeRef, kind: FieldKind) -> FieldDef {
		FieldDef {
			name: name.to_string(),
			description: None,
			ty,
			args: vec![],
			kind,
			function_schema: None,
		}
	}

	fn build_fields(&self, id: TypeId) -> Option<Vec<FieldDef>> {
		let t = &self.types[id];
		Some(match &t.source {
			Source::Query => self.query_fields(),
			Source::Mutation => self.mutation_fields(),
			Source::Node(table) => self.node_fields(table),
			Source::Composite(oid) => self
				.catalog
				.composite_attrs
				.get(oid)
				.map(|attrs| {
					attrs
						.iter()
						.filter_map(|a| {
							let ty = self.sql_type(a.type_oid, None, false)?;
							let name = self.attr_name(a);
							is_valid_name(&name)
								.then(|| self.mk(&name, ty, FieldKind::Attribute(Rc::clone(a))))
						})
						.collect()
				})
				.unwrap_or_default(),
			Source::Edge(table) => vec![
				self.mk(
					"cursor",
					self.scalar(Scalar::String).non_null(),
					FieldKind::Cursor,
				),
				self.mk(
					"node",
					TypeRef::named(self.node_type(table)).non_null(),
					FieldKind::EdgeNode,
				),
			],
			Source::Connection(table) => {
				let edge = self.type_of(table, "Edge");
				let mut f = vec![
					self.mk(
						"edges",
						TypeRef::named(edge).non_null().list().non_null(),
						FieldKind::Edges,
					),
					self.mk(
						"pageInfo",
						TypeRef::named(self.page_info).non_null(),
						FieldKind::PageInfo,
					),
				];
				if table.total_count {
					let mut c = self.mk(
						"totalCount",
						self.scalar(Scalar::Int).non_null(),
						FieldKind::TotalCount,
					);
					c.description =
						Some("The total number of records matching the `filter` criteria".into());
					f.push(c);
				}
				if table.aggregate {
					let mut a = self.mk(
						"aggregate",
						TypeRef::named(self.type_of(table, "Aggregate")),
						FieldKind::Aggregate,
					);
					a.description = Some(format!(
						"Aggregate functions calculated on the collection of `{}`",
						self.table_name(table)
					));
					f.push(a);
				}
				f
			}
			Source::NodeInterface => {
				let mut f = self.mk(
					"nodeId",
					self.scalar(Scalar::ID).non_null(),
					FieldKind::Meta,
				);
				f.description = Some("Retrieves a record by `ID`".into());
				vec![f]
			}
			Source::PageInfo => vec![
				self.mk(
					"endCursor",
					self.scalar(Scalar::String),
					FieldKind::EndCursor,
				),
				self.mk(
					"hasNextPage",
					self.scalar(Scalar::Boolean).non_null(),
					FieldKind::HasNextPage,
				),
				self.mk(
					"hasPreviousPage",
					self.scalar(Scalar::Boolean).non_null(),
					FieldKind::HasPreviousPage,
				),
				self.mk(
					"startCursor",
					self.scalar(Scalar::String),
					FieldKind::StartCursor,
				),
			],
			Source::InsertResponse(table)
			| Source::UpdateResponse(table)
			| Source::DeleteResponse(table) => {
				let mut count = self.mk(
					"affectedCount",
					self.scalar(Scalar::Int).non_null(),
					FieldKind::AffectedCount,
				);
				count.description = Some("Count of the records impacted by the mutation".into());
				let mut records = self.mk(
					"records",
					TypeRef::named(self.node_type(table))
						.non_null()
						.list()
						.non_null(),
					FieldKind::Records,
				);
				records.description = Some("Array of records impacted by the mutation".into());
				vec![count, records]
			}
			Source::Aggregate(table) => {
				let mut count = self.mk(
					"count",
					self.scalar(Scalar::Int).non_null(),
					FieldKind::AggCount,
				);
				count.description = Some("The number of records matching the query".into());
				let mut f = vec![count];
				let sums = table
					.columns
					.iter()
					.any(|c| self.aggregatable(c, AggOp::Sum));
				let mins = table
					.columns
					.iter()
					.any(|c| self.aggregatable(c, AggOp::Min));
				let ops: &[(AggOp, &str)] = &[
					(AggOp::Sum, "Summation aggregates for numeric fields"),
					(AggOp::Avg, "Average aggregates for numeric fields"),
					(AggOp::Min, "Minimum aggregates for comparable fields"),
					(AggOp::Max, "Maximum aggregates for comparable fields"),
				];
				for (op, text) in ops {
					let wanted = if matches!(op, AggOp::Sum | AggOp::Avg) {
						sums
					} else {
						mins
					};
					if wanted {
						let result =
							self.type_of(table, &format!("{}AggregateResult", op.type_part()));
						let mut field =
							self.mk(op.sql(), TypeRef::named(result), FieldKind::AggOp(*op));
						field.description = Some((*text).to_string());
						f.push(field);
					}
				}
				f
			}
			Source::AggregateNumeric(table, op) => {
				let mut f = vec![];
				for column in &table.columns {
					if !self.aggregatable(column, *op) {
						continue;
					}
					let Some(ty) = self.aggregate_result(column, *op) else {
						continue;
					};
					let name = self.column_name(column);
					let mut field = self.mk(&name, ty, FieldKind::AggColumn(Rc::clone(column)));
					field.description = Some(format!(
						"{} of {} across all matching records",
						op.capital_term(),
						name
					));
					f.push(field);
				}
				if f.is_empty() {
					return None;
				}
				f
			}
			Source::Meta(Meta::TypeKind | Meta::DirectiveLocation) => return None,
			Source::Meta(m) => self.meta_fields(*m),
			_ => return None,
		})
	}

	fn connection_args(&self, table: &Table) -> Vec<InputDef> {
		let mut args = self.upstream_connection_args(table);
		if table.extras.distinct_on && !self.distinct_fields(table).is_empty() {
			args.push(InputDef::plain(
				"distinctOn",
				TypeRef::named(self.type_of(table, "Field"))
					.non_null()
					.list(),
				Some(
					"Keep one row for each distinct value of these fields: the first in the collection's order",
				),
			));
		}
		args
	}

	/// The columns `distinctOn` may name: those a collection can be ordered by.
	pub fn distinct_fields(&self, table: &Table) -> Vec<Rc<Column>> {
		self.inputs(self.type_of(table, "OrderBy"))
			.unwrap_or(&[])
			.iter()
			.filter_map(|i| match &i.kind {
				InputKind::Column(c) => Some(Rc::clone(c)),
				_ => None,
			})
			.collect()
	}

	fn upstream_connection_args(&self, table: &Table) -> Vec<InputDef> {
		vec![
			InputDef::plain(
				"first",
				self.scalar(Scalar::Int),
				Some("Query the first `n` records in the collection"),
			),
			InputDef::plain(
				"last",
				self.scalar(Scalar::Int),
				Some("Query the last `n` records in the collection"),
			),
			InputDef::plain(
				"before",
				self.scalar(Scalar::Cursor),
				Some("Query values in the collection before the provided cursor"),
			),
			InputDef::plain(
				"after",
				self.scalar(Scalar::Cursor),
				Some("Query values in the collection after the provided cursor"),
			),
			InputDef::plain(
				"offset",
				self.scalar(Scalar::Int),
				Some(
					"Skip n values from the after cursor. Alternative to cursor pagination. Backward pagination not supported.",
				),
			),
			InputDef::plain(
				"filter",
				TypeRef::named(self.type_of(table, "Filter")),
				Some("Filters to apply to the results set when querying from the collection"),
			),
			InputDef::plain(
				"orderBy",
				TypeRef::named(self.type_of(table, "OrderBy"))
					.non_null()
					.list(),
				Some("Sort order to apply to the collection"),
			),
		]
	}

	fn query_fields(&self) -> Vec<FieldDef> {
		let mut f = vec![];
		let mut node = self.mk(
			"node",
			TypeRef::named(self.node_interface),
			FieldKind::NodeEntry,
		);
		node.description = Some("Retrieve a record by its `ID`".into());
		node.args = vec![InputDef::plain(
			"nodeId",
			self.scalar(Scalar::ID).non_null(),
			Some("The record's `ID`"),
		)];
		f.push(node);

		for table in &self.catalog.tables {
			if !self.table_listed(table) || !table.extras.root {
				continue;
			}
			let base = self.table_name(table);
			let mut collection = self.mk(
				&format!("{}Collection", lower_first(&base)),
				TypeRef::named(self.connection_type(table)).non_null(),
				FieldKind::Collection(Rc::clone(table)),
			);
			collection.args = self.connection_args(table);
			collection.description = Some(format!("A pagable collection of type `{base}`"));
			f.push(collection);

			if self.by_pk_supported(table) {
				let mut args = vec![];
				for column in table.primary_key_columns() {
					let ty = self
						.column_type(&column)
						.unwrap_or_else(|| self.scalar(Scalar::String));
					args.push(InputDef {
						name: self.column_name(&column),
						description: Some(format!("The record's `{}` value", column.name)),
						ty: ty.non_null(),
						default_value: None,
						kind: InputKind::Column(Rc::clone(&column)),
					});
				}
				let mut by_pk = self.mk(
					&format!("{}ByPk", lower_first(&base)),
					TypeRef::named(self.node_type(table)),
					FieldKind::ByPk(Rc::clone(table)),
				);
				by_pk.args = args;
				by_pk.description = Some(format!(
					"Retrieve a record of type `{base}` by its primary key"
				));
				f.push(by_pk);
			}
		}

		let existing: HashSet<String> = f.iter().map(|x| x.name.clone()).collect();
		for field in self.function_fields(&[Volatility::Immutable, Volatility::Stable], false) {
			if !existing.contains(&field.name) {
				f.push(field);
			}
		}

		if self.catalog.introspection_anywhere() {
			let mut t = self.mk(
				"__type",
				TypeRef::named(self.meta("__Type")),
				FieldKind::IntroType,
			);
			t.args = vec![InputDef::plain("name", self.scalar(Scalar::String), None)];
			f.push(t);
			f.push(self.mk(
				"__schema",
				TypeRef::named(self.meta("__Schema")).non_null(),
				FieldKind::IntroSchema,
			));
		}
		f.sort_by(|a, b| a.name.cmp(&b.name));
		f
	}

	fn mutation_fields(&self) -> Vec<FieldDef> {
		let mut f = vec![];
		for table in &self.catalog.tables {
			if !self.table_listed(table) {
				continue;
			}
			let base = self.table_name(table);
			if table.columns.iter().any(|c| c.insertable) {
				let mut insert = self.mk(
					&format!("insertInto{base}Collection"),
					TypeRef::named(self.type_of(table, "InsertResponse")),
					FieldKind::Insert(Rc::clone(table)),
				);
				insert.args = vec![InputDef::plain(
					"objects",
					TypeRef::named(self.type_of(table, "InsertInput"))
						.non_null()
						.list()
						.non_null(),
					None,
				)];
				if table.extras.upsert && !self.upsert_indexes(table).is_empty() {
					insert.args.push(InputDef::plain(
						"onConflict",
						TypeRef::named(self.type_of(table, "OnConflict")),
						None,
					));
				}
				insert.description = Some(format!(
					"Adds one or more `{base}` records to the collection"
				));
				f.push(insert);
			}
			if table.columns.iter().any(|c| c.updatable) {
				let mut update = self.mk(
					&format!("update{base}Collection"),
					TypeRef::named(self.type_of(table, "UpdateResponse")).non_null(),
					FieldKind::Update(Rc::clone(table)),
				);
				update.args = vec![
					InputDef::plain(
						"set",
						TypeRef::named(self.type_of(table, "UpdateInput")).non_null(),
						Some(
							"Fields that are set will be updated for all records matching the `filter`",
						),
					),
					InputDef::plain(
						"filter",
						TypeRef::named(self.type_of(table, "Filter")),
						Some("Restricts the mutation's impact to records matching the criteria"),
					),
					InputDef {
						default_value: Some("1".into()),
						..InputDef::plain(
							"atMost",
							self.scalar(Scalar::Int).non_null(),
							Some(
								"The maximum number of records in the collection permitted to be affected",
							),
						)
					},
				];
				update.description = Some(format!(
					"Updates zero or more records in the `{base}` collection"
				));
				f.push(update);
			}
			if table.deletable {
				let mut delete = self.mk(
					&format!("deleteFrom{base}Collection"),
					TypeRef::named(self.type_of(table, "DeleteResponse")).non_null(),
					FieldKind::Delete(Rc::clone(table)),
				);
				delete.args = vec![
					InputDef::plain(
						"filter",
						TypeRef::named(self.type_of(table, "Filter")),
						Some("Restricts the mutation's impact to records matching the criteria"),
					),
					InputDef {
						default_value: Some("1".into()),
						..InputDef::plain(
							"atMost",
							self.scalar(Scalar::Int).non_null(),
							Some(
								"The maximum number of records in the collection permitted to be affected",
							),
						)
					},
				];
				delete.description = Some(format!(
					"Deletes zero or more records from the `{base}` collection"
				));
				f.push(delete);
			}
		}
		let existing: HashSet<String> = f.iter().map(|x| x.name.clone()).collect();
		for field in self.function_fields(&[Volatility::Volatile], true) {
			if !existing.contains(&field.name) {
				f.push(field);
			}
		}
		f.sort_by(|a, b| a.name.cmp(&b.name));
		f
	}

	/// Whether a function can be a Query or Mutation field.
	fn function_supported(&self, f: &Function, counts: &HashMap<&str, usize>) -> bool {
		let types = &self.catalog.types;
		let element_ok = |t: &crate::catalog::TypeInfo| {
			if t.category != Category::Array {
				return true;
			}
			t.element
				.and_then(|e| types.get(&e))
				.map(|e| e.category == Category::Other)
				.unwrap_or(false)
		};
		// With `functionShapes`, an enum argument or result is its GraphQL enum, overloads are told
		// apart by their field names, and an argument without a name is taken by position.
		let enum_ok = |t: &crate::catalog::TypeInfo| f.shapes && t.category == Category::Enum;
		let return_ok = types
			.get(&f.return_type)
			.map(|t| {
				(t.category != Category::Enum || enum_ok(t))
					&& !matches!(t.name.as_str(), "record" | "trigger" | "event_trigger")
					&& element_ok(t)
			})
			.unwrap_or(false);
		let args_ok = f.args.iter().all(|a| {
			types
				.get(&a.type_oid)
				.map(|t| {
					t.category == Category::Other
						|| enum_ok(t) || (t.category == Category::Array && element_ok(t))
				})
				.unwrap_or(false)
		});
		let unique = if f.shapes {
			let name = self.function_name(f);
			self.catalog
				.functions
				.iter()
				.filter(|g| self.function_name(g) == name)
				.count() <= 1
		} else {
			counts.get(f.name.as_str()).copied().unwrap_or(0) <= 1
		};
		return_ok
			&& args_ok
			&& unique && (f.shapes || f.args.iter().all(|a| a.name.is_some()))
			&& f.executable
			&& !matches!(
				f.schema_name.as_str(),
				"graphql" | "graphql_public" | "auth" | "extensions"
			)
	}

	/// Why a function is not a root field, in the order `function_supported` and
	/// `function_fields` decide it; `None` when nothing stops it (`report.rs`).
	pub fn function_reason(&self, f: &Function) -> Option<String> {
		let types = &self.catalog.types;
		let type_name = |oid: u32| {
			types
				.get(&oid)
				.map(|t| t.name.clone())
				.unwrap_or_else(|| format!("type {oid}"))
		};
		if !f.executable {
			return Some("the role may not execute it".into());
		}
		if matches!(
			f.schema_name.as_str(),
			"graphql" | "graphql_public" | "auth" | "extensions"
		) {
			return Some(format!(
				"functions in the {} schema are never reflected",
				f.schema_name
			));
		}
		if f.shapes {
			let name = self.function_name(f);
			let same = self
				.catalog
				.functions
				.iter()
				.filter(|g| self.function_name(g) == name)
				.count();
			if same > 1 {
				return Some(format!(
					"{same} functions would be the field {name}; a name directive on each tells them apart"
				));
			}
		} else {
			let overloads = self
				.catalog
				.functions
				.iter()
				.filter(|g| g.name == f.name)
				.count();
			if overloads > 1 {
				return Some(format!(
					"{overloads} functions are named {}, and a field needs one",
					f.name
				));
			}
		}
		if !f.shapes && f.args.iter().any(|a| a.name.is_none()) {
			return Some("an argument has no name, and a GraphQL argument needs one".into());
		}
		let mut counts: HashMap<&str, usize> = HashMap::new();
		counts.insert(f.name.as_str(), 1);
		if !self.function_supported(f, &counts) {
			if let Some(a) = f.args.iter().find(|a| {
				!types
					.get(&a.type_oid)
					.is_some_and(|t| t.category == Category::Other || t.category == Category::Array)
			}) {
				return Some(format!(
					"its argument {} is of type {}, which GraphQL cannot take",
					a.name.as_deref().unwrap_or("?"),
					type_name(a.type_oid)
				));
			}
			return Some(format!(
				"it returns {}{}, which GraphQL cannot return",
				if f.set_of { "setof " } else { "" },
				type_name(f.return_type)
			));
		}
		let Some((_, returns)) = self.returns(f) else {
			return Some(format!(
				"it returns {}, which GraphQL cannot return",
				type_name(f.return_type)
			));
		};
		if let Returns::Node(t) | Returns::Connection(t) = &returns {
			if !self.table_listed(t) {
				return Some(format!(
					"it returns rows of {}.{}, which is not in the schema",
					t.schema, t.name
				));
			}
			if matches!(returns, Returns::Connection(_))
				&& let Some(a) = self
					.function_args(f, false)
					.into_iter()
					.find(|a| CONNECTION_ARG_NAMES.contains(&a.name.as_str()))
			{
				return Some(format!(
					"its argument {} has the name of a collection's own argument",
					a.name
				));
			}
		}
		let name = self.function_name(f);
		if !is_valid_name(&name) {
			return Some(format!(
				"its field name, {name:?}, is not a valid GraphQL name"
			));
		}
		None
	}

	fn function_fields(&self, volatilities: &[Volatility], mutation: bool) -> Vec<FieldDef> {
		let mut counts: HashMap<&str, usize> = HashMap::new();
		for f in &self.catalog.functions {
			*counts.entry(f.name.as_str()).or_default() += 1;
		}
		let mut out = vec![];
		for f in &self.catalog.functions {
			if !self.function_supported(f, &counts) || !volatilities.contains(&f.volatility) {
				continue;
			}
			let Some((ty, returns)) = self.returns(f) else {
				continue;
			};
			let mut args = self.function_args(f, false);
			if let Returns::Connection(table) = &returns {
				if args
					.iter()
					.any(|a| CONNECTION_ARG_NAMES.contains(&a.name.as_str()))
				{
					continue;
				}
				args.extend(self.connection_args(table));
			}
			if let Returns::Node(t) | Returns::Connection(t) = &returns
				&& !self.table_listed(t)
			{
				continue;
			}
			let name = self.function_name(f);
			if !is_valid_name(&name) {
				continue;
			}
			let kind = if mutation {
				FieldKind::MutationFunction(Rc::clone(f), returns)
			} else {
				FieldKind::QueryFunction(Rc::clone(f), returns)
			};
			out.push(FieldDef {
				name,
				description: f.description.clone(),
				ty,
				args,
				kind,
				function_schema: Some(f.schema_oid),
			});
		}
		out
	}

	/// A function's arguments as GraphQL arguments; a computed field's first, the row, is not one.
	fn function_args(&self, f: &Function, computed: bool) -> Vec<InputDef> {
		let mut out = vec![];
		for (position, arg) in f.args.iter().enumerate() {
			if computed && position == 0 {
				continue;
			}
			let positional = format!("arg{}", position + 1);
			let name = match &arg.name {
				Some(n) => n,
				None if f.shapes => &positional,
				None => continue,
			};
			let Some(ty) = self.sql_type(arg.type_oid, None, false) else {
				continue;
			};
			let ty = if arg.default.is_none() {
				ty.non_null()
			} else {
				ty
			};
			let default_value = match &arg.default {
				Some(crate::catalog::ArgDefault::Value(v)) => Some(v.clone()),
				_ => None,
			};
			out.push(InputDef {
				name: self.function_arg_name(f, name),
				description: None,
				ty,
				default_value,
				kind: InputKind::FunctionArg {
					name: arg.name.clone(),
					position,
					type_name: arg.type_name.clone(),
				},
			});
		}
		out
	}

	fn node_fields(&self, table: &Rc<Table>) -> Vec<FieldDef> {
		let mut f = vec![];
		if table.primary_key().is_some() {
			let mut id = self.mk(
				"nodeId",
				self.scalar(Scalar::ID).non_null(),
				FieldKind::NodeId(Rc::clone(table)),
			);
			id.description = Some("Globally Unique Record Identifier".into());
			f.push(id);
		}
		for column in &table.columns {
			if !column.selectable
				|| (self.catalog.composites.contains(&column.type_oid)
					&& !self.composite_reflected(column.type_oid))
			{
				continue;
			}
			let Some(ty) = self.column_type(column) else {
				continue;
			};
			let name = self.column_name(column);
			if !is_valid_name(&name) {
				continue;
			}
			let mut field = self.mk(&name, ty, FieldKind::Column(Rc::clone(column)));
			field.description = column.description.clone();
			f.push(field);
		}

		for key in self
			.catalog
			.foreign_keys
			.iter()
			.filter(|k| k.local.oid == table.oid)
		{
			let Some(foreign) = self.catalog.table(key.referenced.oid) else {
				continue;
			};
			if !self.table_listed(foreign) {
				continue;
			}
			let mut ty = TypeRef::named(self.node_type(foreign));
			let not_null = key.local.columns.iter().any(|n| {
				table.columns.iter().any(|c| &c.name == n && c.not_null) && !key.referenced.rls
			});
			if not_null {
				ty = ty.non_null();
			}
			f.push(self.mk(
				&self.key_field_name(key, false),
				ty,
				FieldKind::RelationOne {
					key: Rc::clone(key),
					reverse: false,
					table: Rc::clone(foreign),
				},
			));
		}
		for key in self
			.catalog
			.foreign_keys
			.iter()
			.filter(|k| k.referenced.oid == table.oid)
		{
			let Some(foreign) = self.catalog.table(key.local.oid) else {
				continue;
			};
			if !self.table_listed(foreign) {
				continue;
			}
			let name = self.key_field_name(key, true);
			if self.catalog.key_is_locally_unique(key) {
				f.push(self.mk(
					&name,
					TypeRef::named(self.node_type(foreign)),
					FieldKind::RelationOne {
						key: Rc::clone(key),
						reverse: true,
						table: Rc::clone(foreign),
					},
				));
			} else {
				let mut field = self.mk(
					&name,
					TypeRef::named(self.connection_type(foreign)),
					FieldKind::RelationMany {
						key: Rc::clone(key),
						table: Rc::clone(foreign),
					},
				);
				field.args = self.connection_args(foreign);
				f.push(field);
			}
		}

		if table.selectable {
			for func in &table.functions {
				if !func.executable {
					continue;
				}
				let Some((ty, returns)) = self.returns(func) else {
					continue;
				};
				// A computed field's own arguments (`functionShapes`), each of a type GraphQL takes.
				let mut args = self.function_args(func, true);
				if args.len() + 1 != func.args.len() {
					continue;
				}
				if let Returns::Connection(t) = &returns {
					if args
						.iter()
						.any(|a| CONNECTION_ARG_NAMES.contains(&a.name.as_str()))
					{
						continue;
					}
					args.extend(self.connection_args(t));
				}
				let name = self.function_name(func);
				if !is_valid_name(&name) {
					continue;
				}
				f.push(FieldDef {
					name,
					description: func.description.clone(),
					ty,
					args,
					kind: FieldKind::Computed(Rc::clone(func), returns),
					function_schema: None,
				});
			}
		}
		f
	}

	fn build_inputs(&self, id: TypeId) -> Option<Vec<InputDef>> {
		let t = &self.types[id];
		Some(match &t.source {
			Source::FilterScalar(scalar) => self.scalar_filter_inputs(*scalar, None),
			Source::FilterList(scalar) => {
				let element = self.scalar(*scalar);
				let mut f: Vec<InputDef> = ["contains", "containedBy", "eq", "overlaps"]
					.iter()
					.map(|op| InputDef::plain(op, element.clone().non_null().list(), None))
					.collect();
				f.push(InputDef::plain("is", TypeRef::named(self.filter_is), None));
				f.sort_by(|a, b| a.name.cmp(&b.name));
				f
			}
			Source::FilterEnumList(e) => {
				let element = TypeRef::named(*e);
				let mut f: Vec<InputDef> = ["contains", "containedBy", "eq", "overlaps"]
					.iter()
					.map(|op| InputDef::plain(op, element.clone().non_null().list(), None))
					.collect();
				f.push(InputDef::plain("is", TypeRef::named(self.filter_is), None));
				f.sort_by(|a, b| a.name.cmp(&b.name));
				f
			}
			Source::FilterEnum(e) => {
				let e = TypeRef::named(*e);
				let mut f = vec![
					InputDef::plain("eq", e.clone(), None),
					InputDef::plain("neq", e.clone(), None),
					InputDef::plain("in", e.non_null().list(), None),
					InputDef::plain("is", TypeRef::named(self.filter_is), None),
				];
				f.sort_by(|a, b| a.name.cmp(&b.name));
				f
			}
			Source::FilterEntity(table) => self.filter_entity_inputs(table),
			Source::GeoDistance => vec![
				InputDef::plain("geometry", self.scalar(Scalar::GeoJson).non_null(), None),
				InputDef::plain("distance", self.scalar(Scalar::Float).non_null(), None),
			],
			Source::CompositeFilter(oid) => {
				let mut f = vec![];
				for a in self.catalog.composite_attrs.get(oid).into_iter().flatten() {
					let Some(ty) = self.sql_type(a.type_oid, None, false) else {
						continue;
					};
					let filter = match ty.nullable() {
						TypeRef::Named { id, .. } => match &self.types[*id].source {
							Source::Scalar(s) if *s != Scalar::Json => {
								self.filter_scalars.get(s).copied()
							}
							Source::Enum(e) => self.filter_enums.get(&e.oid).copied(),
							_ => None,
						},
						_ => None,
					};
					let name = self.attr_name(a);
					if let Some(filter) = filter
						&& is_valid_name(&name)
					{
						f.push(InputDef {
							name,
							description: None,
							ty: TypeRef::named(filter),
							default_value: None,
							kind: InputKind::Attribute(Rc::clone(a)),
						});
					}
				}
				f
			}
			Source::OnConflict(table) => {
				let mut f = vec![InputDef::plain(
					"constraint",
					TypeRef::named(self.type_of(table, "UniqueConstraint")).non_null(),
					Some("The unique key whose conflict this handles"),
				)];
				if !self.upsert_fields(table).is_empty() {
					f.push(InputDef {
						default_value: Some("[]".into()),
						..InputDef::plain(
							"updateFields",
							TypeRef::named(self.type_of(table, "UpdateField"))
								.non_null()
								.list(),
							Some(
								"The fields set from the row that conflicted; none leaves the row there as it was",
							),
						)
					});
				}
				f.push(InputDef::plain(
					"filter",
					TypeRef::named(self.type_of(table, "Filter")),
					Some("Update only a row already there that matches"),
				));
				f
			}
			Source::CollectionFilter(table) => {
				let entity = TypeRef::named(self.type_of(table, "Filter"));
				vec![
					InputDef::plain(
						"some",
						entity.clone(),
						Some("True if at least one related row matches"),
					),
					InputDef::plain(
						"every",
						entity.clone(),
						Some("True if every related row matches, and if there are none"),
					),
					InputDef::plain("none", entity, Some("True if no related row matches")),
				]
			}
			Source::OrderByEntity(table) => {
				let mut f: Vec<InputDef> = table
					.columns
					.iter()
					.filter(|c| c.selectable)
					.filter(|c| !c.type_name.ends_with("[]"))
					.filter(|c| !self.catalog.composites.contains(&c.type_oid))
					.filter(|c| c.type_name != "json" && c.type_name != "jsonb")
					.filter(|c| !self.is_geo(c.type_oid))
					.map(|c| InputDef {
						name: self.column_name(c),
						description: None,
						ty: TypeRef::named(self.order_by_direction),
						default_value: None,
						kind: InputKind::Column(Rc::clone(c)),
					})
					.filter(|x| is_valid_name(&x.name))
					.collect();
				if table.extras.order_by_related {
					let related = self.relation_inputs(table, &f, "OrderBy", "CollectionOrderBy");
					f.extend(related);
				}
				f
			}
			Source::CollectionOrderBy(_) => vec![InputDef::plain(
				"count",
				TypeRef::named(self.order_by_direction),
				Some("How many related rows there are"),
			)],
			Source::InsertInput(table) => self.write_inputs(table, |c| c.insertable),
			Source::UpdateInput(table) => self.write_inputs(table, |c| c.updatable),
			_ => return None,
		})
	}

	/// The operators of a scalar filter. `max_len` holds string comparisons to a column's limit.
	pub fn scalar_filter_inputs(&self, scalar: Scalar, max_len: Option<i32>) -> Vec<InputDef> {
		let value = if scalar == Scalar::String {
			self.string_ref(max_len)
		} else {
			self.scalar(scalar)
		};
		let mut f: Vec<InputDef> = scalar
			.filter_ops()
			.iter()
			.map(|op| match *op {
				"in" => InputDef::plain(op, value.clone().non_null().list(), None),
				"is" => InputDef::plain(op, TypeRef::named(self.filter_is), None),
				"dWithin" => InputDef::plain(
					op,
					TypeRef::named(self.lookup("GeoJSONDistance").unwrap_or(self.query)),
					Some("Within this distance of the geometry: meters for a geography, the column's units for a geometry"),
				),
				_ => InputDef::plain(op, value.clone(), None),
			})
			.collect();
		f.sort_by(|a, b| a.name.cmp(&b.name));
		f
	}

	fn filter_entity_inputs(&self, table: &Rc<Table>) -> Vec<InputDef> {
		let mut f = vec![];
		let (mut has_and, mut has_or, mut has_not) = (false, false, false);
		for column in &table.columns {
			if !column.selectable
				|| (self.catalog.composites.contains(&column.type_oid)
					&& !self.composite_reflected(column.type_oid))
				|| column.type_name == "json"
				|| column.type_name == "jsonb"
			{
				continue;
			}
			let Some(ty) = self.column_type(column) else {
				continue;
			};
			let name = self.column_name(column);
			match name.as_str() {
				"and" => has_and = true,
				"or" => has_or = true,
				"not" => has_not = true,
				_ => {}
			}
			let mut max_len = None;
			let filter = match ty.nullable() {
				TypeRef::Named { id, max_len: m } => match &self.types[*id].source {
					Source::Scalar(s) => {
						max_len = *m;
						self.filter_scalars.get(s).copied()
					}
					Source::Enum(e) => self.filter_enums.get(&e.oid).copied(),
					Source::Composite(oid) => self.composite_filters.get(oid).copied(),
					_ => None,
				},
				TypeRef::List(inner) => match inner.nullable() {
					TypeRef::Named { id, .. } => match &self.types[*id].source {
						Source::Scalar(
							s @ (Scalar::Int
							| Scalar::Float
							| Scalar::String
							| Scalar::Boolean
							| Scalar::Uuid
							| Scalar::BigInt
							| Scalar::BigFloat
							| Scalar::Time
							| Scalar::Date
							| Scalar::Datetime),
						) => self.filter_lists.get(s).copied(),
						Source::Enum(e) if table.extras.enum_arrays => {
							self.filter_enum_lists.get(&e.oid).copied()
						}
						_ => None,
					},
					_ => None,
				},
				TypeRef::NonNull(_) => None,
			};
			let Some(filter) = filter else { continue };
			if !is_valid_name(&name) {
				continue;
			}
			f.push(InputDef {
				name,
				description: None,
				// A character column's limit rides on its filter, so compared values are held to it.
				ty: TypeRef::Named {
					id: filter,
					max_len,
				},
				default_value: None,
				kind: InputKind::Column(Rc::clone(column)),
			});
		}
		if table.primary_key().is_some() {
			f.push(InputDef {
				name: "nodeId".into(),
				description: None,
				ty: TypeRef::named(self.filter_scalars[&Scalar::ID]),
				default_value: None,
				kind: InputKind::NodeId,
			});
		}
		if table.extras.relation_filters {
			let related = self.relation_inputs(table, &f, "Filter", "CollectionFilter");
			f.extend(related);
		}
		if table.extras.function_shapes {
			let computed = self.computed_filter_inputs(table, &f);
			f.extend(computed);
		}
		let entity = TypeRef::named(self.type_of(table, "Filter"));
		if !has_and {
			f.push(InputDef::plain(
				"and",
				entity.clone().non_null().list(),
				Some(
					"Returns true only if all its inner filters are true, otherwise returns false",
				),
			));
		}
		if !has_or {
			f.push(InputDef::plain(
				"or",
				entity.clone().non_null().list(),
				Some(
					"Returns true if at least one of its inner filters is true, otherwise returns false",
				),
			));
		}
		if !has_not {
			f.push(InputDef::plain("not", entity, Some("Negates a filter")));
		}
		f
	}

	/// An input per relation the table's node offers, under the same name, typed `<Other><one>`
	/// for a relation to one row and `<Other><many>` for one to many. A filter's compile to
	/// `EXISTS` subqueries and an order's to a subquery per key, both run as the caller, so the
	/// related table's own policies decide what they see.
	fn relation_inputs(
		&self,
		table: &Rc<Table>,
		taken: &[InputDef],
		one: &str,
		many_suffix: &str,
	) -> Vec<InputDef> {
		let Some(fields) = self.fields(self.node_type(table)) else {
			return vec![];
		};
		let mut out: Vec<InputDef> = vec![];
		for field in fields {
			let (key, reverse, other, many) = match &field.kind {
				FieldKind::RelationOne {
					key,
					reverse,
					table,
				} => (key, *reverse, table, false),
				FieldKind::RelationMany { key, table } => (key, true, table, true),
				_ => continue,
			};
			let name = &field.name;
			if ["and", "or", "not"].contains(&name.as_str())
				|| taken.iter().chain(out.iter()).any(|i| &i.name == name)
			{
				continue;
			}
			let suffix = if many { many_suffix } else { one };
			out.push(InputDef {
				name: name.clone(),
				description: None,
				ty: TypeRef::named(self.type_of(other, suffix)),
				default_value: None,
				kind: InputKind::Relation {
					key: Rc::clone(key),
					reverse,
					table: Rc::clone(other),
					many,
				},
			});
		}
		out
	}

	/// A filter field per computed field that takes only the row and returns one scalar or enum
	/// value, filtered as a column of that type is.
	fn computed_filter_inputs(&self, table: &Rc<Table>, taken: &[InputDef]) -> Vec<InputDef> {
		let Some(fields) = self.fields(self.node_type(table)) else {
			return vec![];
		};
		let mut out: Vec<InputDef> = vec![];
		for field in fields {
			let FieldKind::Computed(function, Returns::Scalar | Returns::Enum) = &field.kind else {
				continue;
			};
			if function.args.len() != 1
				|| ["and", "or", "not"].contains(&field.name.as_str())
				|| taken.iter().chain(out.iter()).any(|i| i.name == field.name)
			{
				continue;
			}
			let filter = match &self.types[field.ty.base()].source {
				Source::Scalar(s) if *s != Scalar::Json => self.filter_scalars.get(s).copied(),
				Source::Enum(e) => self.filter_enums.get(&e.oid).copied(),
				_ => None,
			};
			let Some(filter) = filter else { continue };
			out.push(InputDef {
				name: field.name.clone(),
				description: None,
				ty: TypeRef::named(filter),
				default_value: None,
				kind: InputKind::Computed(Rc::clone(function)),
			});
		}
		out
	}

	fn write_inputs(&self, table: &Table, allowed: impl Fn(&Column) -> bool) -> Vec<InputDef> {
		table
			.columns
			.iter()
			.filter(|c| allowed(c) && !c.generated && !c.serial)
			.filter(|c| !self.catalog.composites.contains(&c.type_oid))
			.filter_map(|c| {
				let ty = self.column_type(c)?;
				Some(InputDef {
					name: self.column_name(c),
					description: None,
					ty: ty.nullable().clone(),
					default_value: None,
					kind: InputKind::Column(Rc::clone(c)),
				})
			})
			.collect()
	}

	/// The Node types, for the Node interface's `possibleTypes`.
	pub fn node_types(&self) -> Vec<TypeId> {
		self.listed_types()
			.into_iter()
			.filter(
				|&id| matches!(&self.types[id].source, Source::Node(t) if t.primary_key().is_some()),
			)
			.collect()
	}

	pub fn enum_values(&self, id: TypeId) -> Option<Vec<(String, Option<&'static str>)>> {
		match &self.types[id].source {
			Source::Enum(e) => Some(e.values.iter().map(|v| (e.to_graphql(v), None)).collect()),
			Source::FilterIs => Some(vec![("NULL".into(), None), ("NOT_NULL".into(), None)]),
			Source::OrderByDirection => Some(vec![
				("AscNullsFirst".into(), Some("Ascending order, nulls first")),
				("AscNullsLast".into(), Some("Ascending order, nulls last")),
				(
					"DescNullsFirst".into(),
					Some("Descending order, nulls first"),
				),
				("DescNullsLast".into(), Some("Descending order, nulls last")),
			]),
			Source::Meta(Meta::TypeKind) => Some(
				[
					"SCALAR",
					"OBJECT",
					"INTERFACE",
					"UNION",
					"ENUM",
					"INPUT_OBJECT",
					"LIST",
					"NON_NULL",
				]
				.iter()
				.map(|v| (v.to_string(), None))
				.collect(),
			),
			Source::ExtraEnum(table, ExtraEnum::UniqueConstraint) => Some(
				self.upsert_indexes(table)
					.into_iter()
					.map(|i| (i.name.clone(), None))
					.collect(),
			),
			Source::ExtraEnum(table, ExtraEnum::UpdateField) => Some(
				self.upsert_fields(table)
					.into_iter()
					.map(|c| (self.column_name(&c), None))
					.collect(),
			),
			Source::ExtraEnum(table, ExtraEnum::Field) => Some(
				self.distinct_fields(table)
					.into_iter()
					.map(|c| (self.column_name(&c), None))
					.collect(),
			),
			Source::Meta(Meta::DirectiveLocation) => Some(
				DIRECTIVE_LOCATIONS
					.iter()
					.map(|(v, d)| (v.to_string(), Some(*d)))
					.collect(),
			),
			_ => None,
		}
	}

	fn meta_fields(&self, m: Meta) -> Vec<FieldDef> {
		let string = || self.scalar(Scalar::String);
		let boolean = || self.scalar(Scalar::Boolean);
		let t = |name: &str| TypeRef::named(self.meta(name));
		let include_deprecated = || {
			vec![InputDef {
				default_value: Some("false".into()),
				..InputDef::plain("includeDeprecated", self.scalar(Scalar::Boolean), None)
			}]
		};
		let mut f: Vec<FieldDef> = match m {
			Meta::Schema => vec![
				meta_field(
					"types",
					t("__Type").non_null().list().non_null(),
					Some("A list of all types supported by this server."),
					vec![],
				),
				meta_field(
					"queryType",
					t("__Type").non_null(),
					Some("The type that query operations will be rooted at."),
					vec![],
				),
				meta_field(
					"mutationType",
					t("__Type"),
					Some(
						"If this server supports mutation, the type that mutation operations will be rooted at.",
					),
					vec![],
				),
				meta_field(
					"subscriptionType",
					t("__Type"),
					Some(
						"If this server support subscription, the type that subscription operations will be rooted at.",
					),
					vec![],
				),
				meta_field(
					"directives",
					t("__Directive").non_null().list().non_null(),
					Some("A list of all directives supported by this server."),
					include_deprecated(),
				),
				meta_field("description", string(), None, vec![]),
			],
			Meta::Type => vec![
				meta_field("name", string(), None, vec![]),
				meta_field("description", string(), None, vec![]),
				meta_field("kind", t("__TypeKind").non_null(), None, vec![]),
				meta_field(
					"inputFields",
					t("__InputValue").non_null().list(),
					None,
					include_deprecated(),
				),
				meta_field("interfaces", t("__Type").non_null().list(), None, vec![]),
				meta_field("possibleTypes", t("__Type").non_null().list(), None, vec![]),
				meta_field(
					"enumValues",
					t("__EnumValue").non_null().list(),
					None,
					include_deprecated(),
				),
				meta_field(
					"fields",
					t("__Field").non_null().list(),
					None,
					include_deprecated(),
				),
				meta_field("ofType", t("__Type"), None, vec![]),
				meta_field("specifiedByURL", string(), None, vec![]),
			],
			Meta::Field => vec![
				meta_field("name", string().non_null(), None, vec![]),
				meta_field("description", string(), None, vec![]),
				meta_field(
					"args",
					t("__InputValue").non_null().list().non_null(),
					None,
					include_deprecated(),
				),
				meta_field("type", t("__Type").non_null(), None, vec![]),
				meta_field("isDeprecated", boolean().non_null(), None, vec![]),
				meta_field("deprecationReason", string(), None, vec![]),
			],
			Meta::InputValue => vec![
				meta_field("name", string().non_null(), None, vec![]),
				meta_field("description", string(), None, vec![]),
				meta_field("type", t("__Type").non_null(), None, vec![]),
				meta_field(
					"defaultValue",
					string(),
					Some(
						"A GraphQL-formatted string representing the default value for this input value.",
					),
					vec![],
				),
				meta_field("isDeprecated", boolean().non_null(), None, vec![]),
				meta_field("deprecationReason", string(), None, vec![]),
			],
			Meta::EnumValue => vec![
				meta_field("name", string().non_null(), None, vec![]),
				meta_field("description", string(), None, vec![]),
				meta_field("isDeprecated", boolean().non_null(), None, vec![]),
				meta_field("deprecationReason", string(), None, vec![]),
			],
			Meta::Directive => vec![
				meta_field("name", string().non_null(), None, vec![]),
				meta_field("description", string(), None, vec![]),
				meta_field("isRepeatable", boolean().non_null(), None, vec![]),
				meta_field(
					"locations",
					t("__DirectiveLocation").non_null().list().non_null(),
					None,
					vec![],
				),
				meta_field(
					"args",
					t("__InputValue").non_null().list().non_null(),
					None,
					include_deprecated(),
				),
			],
			Meta::TypeKind | Meta::DirectiveLocation => return vec![],
		};
		f.sort_by(|a, b| a.name.cmp(&b.name));
		f
	}
}

fn meta_field(name: &str, ty: TypeRef, description: Option<&str>, args: Vec<InputDef>) -> FieldDef {
	FieldDef {
		name: name.to_string(),
		description: description.map(str::to_string),
		ty,
		args,
		kind: FieldKind::Meta,
		function_schema: None,
	}
}

fn source_table(s: &Source) -> Option<&Rc<Table>> {
	match s {
		Source::Node(t)
		| Source::Edge(t)
		| Source::Connection(t)
		| Source::FilterEntity(t)
		| Source::CollectionFilter(t)
		| Source::OnConflict(t)
		| Source::CollectionOrderBy(t)
		| Source::ExtraEnum(t, _)
		| Source::OrderByEntity(t)
		| Source::InsertInput(t)
		| Source::InsertResponse(t)
		| Source::UpdateInput(t)
		| Source::UpdateResponse(t)
		| Source::DeleteResponse(t)
		| Source::Aggregate(t)
		| Source::AggregateNumeric(t, _) => Some(t),
		_ => None,
	}
}

fn meta_description(m: Meta) -> &'static str {
	match m {
		Meta::TypeKind => "An enum describing what kind of type a given `__Type` is.",
		Meta::Schema => {
			"A GraphQL Schema defines the capabilities of a GraphQL server. It exposes all available types and directives on the server, as well as the entry points for query, mutation, and subscription operations."
		}
		Meta::Type => {
			"The fundamental unit of any GraphQL Schema is the type. There are many kinds of types in GraphQL as represented by the `__TypeKind` enum.\\n\\nDepending on the kind of a type, certain fields describe information about that type. Scalar types provide no information beyond a name, description and optional `specifiedByURL`, while Enum types provide their values. Object and Interface types provide the fields they describe. Abstract types, Union and Interface, provide the Object types possible at runtime. List and NonNull types compose other types "
		}
		Meta::Field => {
			"Object and Interface types are described by a list of Fields, each of which has a name, potentially a list of arguments, and a return type."
		}
		Meta::InputValue => {
			"Arguments provided to Fields or Directives and the input fields of an InputObject are represented as Input Values which describe their type and optionally a default value."
		}
		Meta::EnumValue => {
			"One possible value for a given Enum. Enum values are unique values, not a placeholder for a string or numeric value. However an Enum value is returned in a JSON response as a string."
		}
		Meta::DirectiveLocation => {
			"A Directive can be adjacent to many parts of the GraphQL language, a __DirectiveLocation describes one such possible adjacencies."
		}
		Meta::Directive => {
			"A Directive provides a way to describe alternate runtime execution and type validation behavior in a GraphQL document.\\n\\nIn some cases, you need to provide options to alter GraphQL execution behavior in ways field arguments will not suffice, such as conditionally including or skipping a field. Directives provide this by describing additional information to the executor."
		}
	}
}

pub const DIRECTIVE_LOCATIONS: [(&str, &str); 19] = [
	("QUERY", "Location adjacent to a query operation."),
	("MUTATION", "Location adjacent to a mutation operation."),
	(
		"SUBSCRIPTION",
		"Location adjacent to a subscription operation.",
	),
	("FIELD", "Location adjacent to a field."),
	(
		"FRAGMENT_DEFINITION",
		"Location adjacent to a fragment definition.",
	),
	("FRAGMENT_SPREAD", "Location adjacent to a fragment spread."),
	(
		"INLINE_FRAGMENT",
		"Location adjacent to an inline fragment.",
	),
	(
		"VARIABLE_DEFINITION",
		"Location adjacent to a variable definition.",
	),
	("SCHEMA", "Location adjacent to a schema definition."),
	("SCALAR", "Location adjacent to a scalar definition."),
	("OBJECT", "Location adjacent to an object type definition."),
	(
		"FIELD_DEFINITION",
		"Location adjacent to a field definition.",
	),
	(
		"ARGUMENT_DEFINITION",
		"Location adjacent to an argument definition.",
	),
	("INTERFACE", "Location adjacent to an interface definition."),
	("UNION", "Location adjacent to a union definition."),
	("ENUM", "Location adjacent to an enum definition."),
	(
		"ENUM_VALUE",
		"Location adjacent to an enum value definition.",
	),
	(
		"INPUT_OBJECT",
		"Location adjacent to an input object type definition.",
	),
	(
		"INPUT_FIELD_DEFINITION",
		"Location adjacent to an input object field definition.",
	),
];
