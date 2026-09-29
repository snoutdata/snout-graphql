//! What the database says about itself: the schemas on the search path, their tables, views and
//! functions, the types those use, and the comment directives on each. Everything the GraphQL
//! schema is built from is read here, as the calling role, in one pass of a few queries.
//!
//! The structs are plain data. Nothing here knows about GraphQL; `schema.rs` turns this into
//! types and fields.
use crate::pg;
use serde_json::Value as Json;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

/// Who is asking, and against which version of the schema. Two requests with equal keys see the
/// same reflected schema, so this is what a cached schema is looked up by.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Key {
	pub search_path: Vec<u32>,
	pub role: String,
	/// A fingerprint of role memberships and of whether this role is a superuser: both change what
	/// it may see without any DDL.
	pub memberships: Vec<i64>,
	pub schema_version: i32,
}

#[derive(Clone, Debug)]
pub struct Schema {
	pub inflect_names: bool,
	pub max_rows: u64,
	pub introspection: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
	Array,
	Enum,
	Table,
	Composite,
	Other,
}

#[derive(Clone, Debug)]
pub struct TypeInfo {
	pub oid: u32,
	pub name: String,
	pub category: Category,
	pub element: Option<u32>,
	pub table: Option<u32>,
	pub usable: bool,
}

#[derive(Clone, Debug)]
pub struct EnumInfo {
	pub oid: u32,
	pub schema_oid: u32,
	pub name: String,
	pub name_override: Option<String>,
	/// Database label to GraphQL value, from the `mappings` directive.
	pub mappings: Option<Vec<(String, String)>>,
	pub values: Vec<String>,
	pub usable: bool,
}

impl EnumInfo {
	pub fn to_graphql(&self, label: &str) -> String {
		self.mappings
			.as_ref()
			.and_then(|m| {
				m.iter()
					.find(|(db, _)| db == label)
					.map(|(_, gql)| gql.clone())
			})
			.unwrap_or_else(|| label.to_string())
	}

	pub fn db_label(&self, value: &str) -> Option<String> {
		self.mappings.as_ref().and_then(|m| {
			m.iter()
				.find(|(_, gql)| gql == value)
				.map(|(db, _)| db.clone())
		})
	}
}

#[derive(Clone, Debug)]
pub struct Column {
	pub name: String,
	pub type_oid: u32,
	/// `format_type` of the column, with its modifier (`character varying(255)`). Used to cast
	/// every value sent to the database for this column.
	pub type_name: String,
	pub max_characters: Option<i32>,
	pub schema_oid: u32,
	pub not_null: bool,
	pub serial: bool,
	pub generated: bool,
	pub insertable: bool,
	pub selectable: bool,
	pub updatable: bool,
	pub name_override: Option<String>,
	pub description: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Index {
	pub columns: Vec<String>,
	pub unique: bool,
	pub primary: bool,
}

#[derive(Clone, Debug)]
pub struct DirectiveForeignKey {
	pub local_name: Option<String>,
	pub local_columns: Vec<String>,
	pub foreign_name: Option<String>,
	pub foreign_schema: String,
	pub foreign_table: String,
	pub foreign_columns: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Table {
	pub oid: u32,
	pub name: String,
	pub schema: String,
	pub schema_oid: u32,
	pub rls: bool,
	pub columns: Vec<Rc<Column>>,
	pub indexes: Vec<Index>,
	pub selectable: bool,
	pub deletable: bool,
	pub name_override: Option<String>,
	pub description: Option<String>,
	pub total_count: bool,
	pub aggregate: bool,
	pub primary_key_directive: Option<Vec<String>>,
	pub foreign_key_directives: Vec<DirectiveForeignKey>,
	pub max_rows: Option<u64>,
	/// Functions taking this table's row as their only argument: computed fields.
	pub functions: Vec<Rc<Function>>,
}

impl Table {
	/// The real primary key, or the one a directive declares (a view's), if every column it names
	/// exists.
	pub fn primary_key(&self) -> Option<Vec<String>> {
		if let Some(pk) = self.indexes.iter().find(|i| i.primary) {
			return Some(pk.columns.clone());
		}
		let names = self.primary_key_directive.as_ref()?;
		let all_exist = names
			.iter()
			.all(|n| self.columns.iter().any(|c| &c.name == n));
		all_exist.then(|| names.clone())
	}

	pub fn primary_key_columns(&self) -> Vec<Rc<Column>> {
		self.primary_key()
			.unwrap_or_default()
			.iter()
			.filter_map(|n| self.columns.iter().find(|c| &c.name == n).cloned())
			.collect()
	}

	pub fn column(&self, name: &str) -> Option<&Rc<Column>> {
		self.columns.iter().find(|c| c.name == name)
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Volatility {
	Volatile,
	Stable,
	Immutable,
}

#[derive(Clone, Debug)]
pub enum ArgDefault {
	/// A default the schema can state (`5`, `true`, `"text"`), as GraphQL literal text.
	Value(String),
	/// A default exists but cannot be stated (an expression, or NULL).
	Null,
}

#[derive(Clone, Debug)]
pub struct Arg {
	pub type_oid: u32,
	pub type_name: String,
	pub name: Option<String>,
	pub default: Option<ArgDefault>,
}

#[derive(Clone, Debug)]
pub struct Function {
	pub name: String,
	pub schema_oid: u32,
	pub schema_name: String,
	pub args: Vec<Arg>,
	pub return_type: u32,
	pub volatility: Volatility,
	pub set_of: bool,
	pub executable: bool,
	pub name_override: Option<String>,
	pub description: Option<String>,
}

#[derive(Clone, Debug)]
pub struct KeySide {
	pub oid: u32,
	pub rls: bool,
	pub columns: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct ForeignKey {
	pub local: KeySide,
	pub referenced: KeySide,
	pub local_name: Option<String>,
	pub foreign_name: Option<String>,
}

pub struct Catalog {
	pub schemas: HashMap<u32, Schema>,
	pub tables: Vec<Rc<Table>>,
	pub table_by_oid: HashMap<u32, usize>,
	pub types: HashMap<u32, TypeInfo>,
	pub enums: HashMap<u32, Rc<EnumInfo>>,
	pub composites: HashSet<u32>,
	pub functions: Vec<Rc<Function>>,
	/// Every foreign key GraphQL may follow: the real ones, then the ones directives declare.
	pub foreign_keys: Vec<Rc<ForeignKey>>,
}

/// A loading failure a client sees: the same sentence whether the cause is a directive with the
/// wrong shape or a value of the wrong type.
pub struct LoadError(pub String);

fn directive_error(detail: impl std::fmt::Display) -> LoadError {
	LoadError(format!(
		"Error while loading schema, check comment directives. {detail}"
	))
}

/// The cache key for the current role and search path: one short query, run on every request.
pub fn key() -> Key {
	const SQL: &str = "
		select
			coalesce(
				(select array_agg(n.oid::int8 order by p.ord)
				   from unnest(current_schemas(false)) with ordinality p(name, ord)
				   join pg_catalog.pg_namespace n on n.nspname = p.name),
				'{}'),
			current_role::text,
			-- Any change to any role membership, or to whether this role is a superuser, moves
			-- this: one scan of a small catalogue, where testing each role would cost a function
			-- call per role on every request.
			array[
				coalesce((select sum(hashint8(m.roleid::int8 * 4294967296 + m.member::int8))::int8
				            from pg_catalog.pg_auth_members m), 0),
				coalesce((select r.rolsuper::int::int8 from pg_catalog.pg_roles r where r.rolname = current_role::text), 0)
			],
			graphql.get_schema_version()";
	let row = pg::query(SQL, &[]).into_iter().next().unwrap_or_default();
	Key {
		search_path: row.int8_array(0).into_iter().map(|x| x as u32).collect(),
		role: row.text(1).unwrap_or_default(),
		memberships: row.int8_array(2),
		schema_version: row.int8(3).unwrap_or(0) as i32,
	}
}

/// A fingerprint of every catalogue row the reflected schema is read from: the tables, views,
/// columns, defaults, indexes, foreign keys, functions and types of the schemas on the search
/// path, the types their columns use, every schema's privileges, and the comments on all of them.
/// Every row's `xmin` moves when the row does, so any change a GraphQL schema could see changes
/// this, and nothing else does: a temporary table, a materialized view's refresh or a partition
/// created in another schema moves the schema version but not this.
///
/// It is read only when the version has moved, to decide whether the schema must be read again.
pub fn fingerprint(key: &Key) -> i64 {
	let path: Vec<i64> = key.search_path.iter().map(|&o| o as i64).collect();
	let sql = format!(
		"
		with t as materialized (
			select c.oid from pg_catalog.pg_class c where c.relnamespace = any($1::int8[])
		), a as materialized (
			select a.xmin, a.ctid, a.atttypid from pg_catalog.pg_attribute a
			 where a.attrelid in (select oid from t) and a.attnum > 0
		)
		select coalesce(sum(h), 0)::int8 + count(*) from (
			select hashtext(xmin::text || ctid::text) as h from pg_catalog.pg_namespace
			union all select hashtext(c.xmin::text || c.ctid::text) from pg_catalog.pg_class c where c.oid in (select oid from t)
			union all select hashtext(a.xmin::text || a.ctid::text) from a
			union all select hashtext(d.xmin::text || d.ctid::text) from pg_catalog.pg_attrdef d where d.adrelid in (select oid from t)
			union all select hashtext(i.xmin::text || i.ctid::text) from pg_catalog.pg_index i where i.indrelid in (select oid from t)
			union all select hashtext(k.xmin::text || k.ctid::text) from pg_catalog.pg_constraint k
				where k.contype = 'f' and (k.confrelid in (select oid from t) or k.conrelid in (select oid from t))
			union all select hashtext(p.xmin::text || p.ctid::text) from pg_catalog.pg_proc p where p.pronamespace = any($1::int8[]){}
			union all select hashtext(y.xmin::text || y.ctid::text) from pg_catalog.pg_type y
				where y.typnamespace = any($1::int8[])
				   or y.oid in (select a.atttypid from a)
			union all select hashtext(e.xmin::text || e.ctid::text) from pg_catalog.pg_enum e
				join pg_catalog.pg_type y on y.oid = e.enumtypid where y.typnamespace = any($1::int8[])
			-- Comments on user objects, and on every schema (`public` is a built-in object, and its
			-- comment is where the schema-wide directives live).
			union all select hashtext(s.xmin::text || s.ctid::text) from pg_catalog.pg_description s
				where s.objoid >= 16384 or s.classoid = 'pg_catalog.pg_namespace'::regclass
		) rows",
		user_objects_only(&path)
	);
	pg::query(&sql, &[pg::Arg::Int8Array(path)])
		.first()
		.and_then(|r| r.int8(0))
		.unwrap_or(0)
}

/// The filter that lets a read of `pg_proc` for the schemas on the search path use the index on
/// `oid` instead of reading every built-in function: anything created after `initdb` has an oid of
/// at least 16384 (FirstNormalObjectId), and only a system schema named on the path (`pg_catalog`,
/// `information_schema`) holds anything below it. `public` (oid 2200) is made by `initdb` too, but
/// empty.
fn user_objects_only(path: &[i64]) -> &'static str {
	const PUBLIC: i64 = 2200;
	if path.iter().all(|&oid| oid >= 16384 || oid == PUBLIC) {
		" and p.oid >= 16384"
	} else {
		""
	}
}

/// Whether this database's `graphql.increment_schema_version` is the one this extension's install
/// script writes, which leaves temporary objects and materialized view refreshes alone, so that a
/// moved version almost always means a change the schema shows. A database that installed the
/// extension under its old name keeps the trigger it had, which moves the version for every DDL
/// statement anywhere, and only there is the fingerprint worth reading first. Read only when the
/// version has moved.
pub fn version_is_precise() -> bool {
	const SQL: &str = "
		select coalesce(
			(select p.prosrc like '%pg_event_trigger_ddl_commands%'
			   from pg_catalog.pg_proc p
			  where p.oid = pg_catalog.to_regprocedure('graphql.increment_schema_version()')),
			false)";
	pg::query(SQL, &[])
		.first()
		.and_then(|r| r.bool(0))
		.unwrap_or(false)
}

pub fn load(key: Key) -> Result<Catalog, LoadError> {
	let path: Vec<i64> = key.search_path.iter().map(|&o| o as i64).collect();

	// Schemas the role may use, and which of them are on the search path.
	let mut usable_schemas: HashSet<u32> = HashSet::new();
	let mut schemas: HashMap<u32, Schema> = HashMap::new();
	for row in pg::query(
		"select n.oid::int8, n.nspname::text, n.oid = any($1::int8[]),
			(select ds.description from pg_catalog.pg_description ds
			  where ds.objoid = n.oid and ds.classoid = 'pg_catalog.pg_namespace'::regclass and ds.objsubid = 0)
		   from pg_catalog.pg_namespace n
		  where pg_catalog.has_schema_privilege(current_user, n.oid, 'USAGE')",
		&[pg::Arg::Int8Array(path.clone())],
	) {
		let oid = row.int8(0).unwrap_or(0) as u32;
		usable_schemas.insert(oid);
		let d = parse_directive(row.text(3))?;
		if row.bool(2).unwrap_or(false) {
			let max_rows = match d.get("max_rows") {
				None | Some(Json::Null) => 30,
				Some(v) => {
					let n = sql_int(v).ok_or_else(|| {
						LoadError(format!("invalid input syntax for type integer: \"{}\"", json_text(v)))
					})?;
					if n < 0 {
						return Err(directive_error(format!("invalid value: integer `{n}`, expected u64")));
					}
					n as u64
				}
			};
			schemas.insert(
				oid,
				Schema {
					inflect_names: d.get("inflect_names") == Some(&Json::Bool(true)),
					max_rows,
					introspection: d.get("introspection") == Some(&Json::Bool(true)),
				},
			);
		}
	}
	let usable: Vec<i64> = usable_schemas.iter().map(|&o| o as i64).collect();
	let exposed: Vec<i64> = schemas.keys().map(|&o| o as i64).collect();

	// Types in schemas the role may use.
	let mut types: HashMap<u32, TypeInfo> = HashMap::new();
	for row in pg::query(
		"select t.oid::int8, t.typname::text,
			case
				when t.typcategory = 'A' then 'A'
				when t.typcategory = 'E' then 'E'
				when t.typcategory = 'C' and c.relkind in ('r', 't', 'v', 'm', 'f', 'p') then 'T'
				when t.typcategory = 'C' and c.relkind = 'c' then 'C'
				else 'O'
			end,
			nullif(t.typelem, 0)::int8, c.oid::int8,
			pg_catalog.has_type_privilege(current_user, t.oid, 'USAGE')
		   from pg_catalog.pg_type t
		   left join pg_catalog.pg_class c on c.oid = t.typrelid
		  where t.typnamespace = any($1::int8[])",
		&[pg::Arg::Int8Array(usable.clone())],
	) {
		let oid = row.int8(0).unwrap_or(0) as u32;
		let category = match row.text(2).as_deref() {
			Some("A") => Category::Array,
			Some("E") => Category::Enum,
			Some("T") => Category::Table,
			Some("C") => Category::Composite,
			_ => Category::Other,
		};
		types.insert(
			oid,
			TypeInfo {
				oid,
				name: row.text(1).unwrap_or_default(),
				category,
				element: row.int8(3).map(|x| x as u32),
				table: row.int8(4).map(|x| x as u32),
				usable: row.bool(5).unwrap_or(false),
			},
		);
	}
	let composites: HashSet<u32> = types
		.values()
		.filter(|t| t.category == Category::Composite)
		.map(|t| t.oid)
		.collect();

	// Enums, with their labels in sort order.
	let mut enums: HashMap<u32, Rc<EnumInfo>> = HashMap::new();
	for row in pg::query(
		"select t.oid::int8, t.typnamespace::int8, t.typname::text,
			(select ds.description from pg_catalog.pg_description ds
			  where ds.objoid = t.oid and ds.classoid = 'pg_catalog.pg_type'::regclass and ds.objsubid = 0),
			array_agg(e.enumlabel::text order by e.enumsortorder),
			pg_catalog.has_type_privilege(current_user, t.oid, 'USAGE')
		   from pg_catalog.pg_type t
		   join pg_catalog.pg_enum e on e.enumtypid = t.oid
		  where t.typnamespace = any($1::int8[])
		  group by t.oid",
		&[pg::Arg::Int8Array(usable.clone())],
	) {
		let oid = row.int8(0).unwrap_or(0) as u32;
		let d = parse_directive(row.text(3))?;
		let mappings = match d.get("mappings") {
			None | Some(Json::Null) => None,
			Some(Json::Object(m)) => {
				let mut out = vec![];
				for (k, v) in m {
					match v {
						Json::String(s) => out.push((k.clone(), s.clone())),
						other => return Err(directive_error(serde_kind(other, "a string"))),
					}
				}
				Some(out)
			}
			Some(other) => return Err(directive_error(serde_kind(other, "a map"))),
		};
		enums.insert(
			oid,
			Rc::new(EnumInfo {
				oid,
				schema_oid: row.int8(1).unwrap_or(0) as u32,
				name: row.text(2).unwrap_or_default(),
				name_override: json_text_opt(d.get("name")),
				mappings,
				values: row.text_array(4),
				usable: row.bool(5).unwrap_or(false),
			}),
		);
	}

	// Functions in the schemas on the search path.
	let mut functions: Vec<Rc<Function>> = vec![];
	let functions_sql = format!(
		"select p.oid::int8, p.proname::text, p.prorettype::int8, p.pronamespace::int8,
			p.pronamespace::regnamespace::text,
			p.proargtypes::int8[], p.proargnames::text[],
			pg_catalog.pg_get_expr(p.proargdefaults, 0)::text,
			p.pronargs::int4, p.pronargdefaults::int4,
			p.proargtypes::regtype[]::text[],
			p.provolatile::text,
			p.proretset and p.prorows <> 1,
			(select ds.description from pg_catalog.pg_description ds
			  where ds.objoid = p.oid and ds.classoid = 'pg_catalog.pg_proc'::regclass and ds.objsubid = 0),
			pg_catalog.has_function_privilege(current_user, p.oid, 'EXECUTE')
		   from pg_catalog.pg_proc p
		  where p.pronamespace = any($1::int8[]){}
		  order by p.oid",
		user_objects_only(&path)
	);
	for row in pg::query(&functions_sql, &[pg::Arg::Int8Array(path.clone())]) {
		let d = parse_directive(row.text(13))?;
		let arg_types: Vec<u32> = row.int8_array(5).into_iter().map(|x| x as u32).collect();
		let arg_names = row.opt_text_array(6);
		let arg_type_names = row.text_array(10);
		let n = arg_types.len();
		let num_defaults = row.int4(9).unwrap_or(0).max(0) as usize;
		let defaults = arg_defaults(row.text(7), num_defaults, &arg_types);
		let args = (0..n)
			.map(|i| {
				let name = arg_names
					.as_ref()
					.and_then(|names| names.get(i).cloned().flatten())
					.filter(|s| !s.is_empty());
				let mut type_name = arg_type_names.get(i).cloned().unwrap_or_default();
				if type_name == "character" {
					type_name = "text".to_string();
				}
				Arg {
					type_oid: arg_types[i],
					type_name,
					name,
					default: defaults.get(i).cloned().flatten(),
				}
			})
			.collect();
		functions.push(Rc::new(Function {
			name: row.text(1).unwrap_or_default(),
			return_type: row.int8(2).unwrap_or(0) as u32,
			schema_oid: row.int8(3).unwrap_or(0) as u32,
			schema_name: row.text(4).unwrap_or_default(),
			args,
			volatility: match row.text(11).as_deref() {
				Some("i") => Volatility::Immutable,
				Some("s") => Volatility::Stable,
				_ => Volatility::Volatile,
			},
			set_of: row.bool(12).unwrap_or(false),
			executable: row.bool(14).unwrap_or(false),
			name_override: json_text_opt(d.get("name")),
			description: json_text_opt(d.get("description")),
		}));
	}

	// Columns of the tables, views, materialized views and foreign tables on the search path.
	let mut numbered: HashMap<u32, Vec<(i64, Rc<Column>)>> = HashMap::new();
	// Column names by table and number, for the keys of indexes and foreign keys below.
	let mut attnames: HashMap<u32, HashMap<i64, String>> = HashMap::new();
	for row in pg::query(
		"with t as materialized (
			-- A privilege on a table is one on each of its columns, so a column's own ACL is read
			-- only when the table's says no: once per table, not three times per column.
			select c.oid, c.relnamespace,
				pg_catalog.has_table_privilege(current_user, c.oid, 'INSERT') as ins,
				pg_catalog.has_table_privilege(current_user, c.oid, 'SELECT') as sel,
				pg_catalog.has_table_privilege(current_user, c.oid, 'UPDATE') as upd
			  from pg_catalog.pg_class c
			 where c.relnamespace = any($1::int8[])
			   and c.relkind in ('r', 'v', 'm', 'f')
		), serial as materialized (
			-- The columns a sequence belongs to (serial, identity): from the sequences, through
			-- the index on the dependent side, rather than every pg_class dependency there is.
			select dep.refobjid, dep.refobjsubid
			  from pg_catalog.pg_sequence sq
			  join pg_catalog.pg_depend dep
			    on dep.classid = 'pg_catalog.pg_class'::regclass and dep.objid = sq.seqrelid
			 where dep.refclassid = 'pg_catalog.pg_class'::regclass
			   and dep.deptype in ('a', 'i')
		)
		select a.attrelid::int8, a.attname::text, a.atttypid::int8,
			pg_catalog.format_type(a.atttypid, a.atttypmod),
			nullif(a.atttypmod, -1) - 4,
			t.relnamespace::int8,
			a.attnotnull, d.adbin is not null,
			(a.attrelid, a.attnum::int4) in (select refobjid, refobjsubid from serial),
			a.attgenerated <> '',
			case when t.ins then true
				else pg_catalog.has_column_privilege(current_user, a.attrelid, a.attnum, 'INSERT') end,
			case when t.sel then true
				else pg_catalog.has_column_privilege(current_user, a.attrelid, a.attnum, 'SELECT') end,
			case when t.upd then true
				else pg_catalog.has_column_privilege(current_user, a.attrelid, a.attnum, 'UPDATE') end,
			(select ds.description from pg_catalog.pg_description ds
			  where ds.objoid = a.attrelid and ds.classoid = 'pg_catalog.pg_class'::regclass and ds.objsubid = a.attnum),
			a.attnum::int8
		   from t
		   -- Each table's columns through the index on (attrelid, attnum), not a scan of every
		   -- column in the database, most of which are system columns and catalogues'.
		   cross join lateral (
				select a.attrelid, a.attname, a.atttypid, a.atttypmod, a.attnum, a.attnotnull, a.attgenerated
				  from pg_catalog.pg_attribute a
				 where a.attrelid = t.oid and a.attnum > 0 and not a.attisdropped
				offset 0
		   ) a
		   left join pg_catalog.pg_attrdef d on d.adrelid = a.attrelid and d.adnum = a.attnum",
		&[pg::Arg::Int8Array(exposed.clone())],
	) {
		let d = parse_directive(row.text(13))?;
		let table = row.int8(0).unwrap_or(0) as u32;
		let number = row.int8(14).unwrap_or(0);
		attnames
			.entry(table)
			.or_default()
			.insert(number, row.text(1).unwrap_or_default());
		numbered.entry(table).or_default().push((number, Rc::new(Column {
			name: row.text(1).unwrap_or_default(),
			type_oid: row.int8(2).unwrap_or(0) as u32,
			type_name: row.text(3).unwrap_or_default(),
			max_characters: row.int4(4),
			schema_oid: row.int8(5).unwrap_or(0) as u32,
			not_null: row.bool(6).unwrap_or(false),
			serial: row.bool(8).unwrap_or(false),
			generated: row.bool(9).unwrap_or(false),
			insertable: row.bool(10).unwrap_or(false),
			selectable: row.bool(11).unwrap_or(false),
			updatable: row.bool(12).unwrap_or(false),
			name_override: json_text_opt(d.get("name")),
			description: json_str_strict(d.get("description"))?,
		})));
	}
	// In column order, which is the order fields are listed in.
	let mut columns: HashMap<u32, Vec<Rc<Column>>> = numbered
		.into_iter()
		.map(|(table, mut list)| {
			list.sort_by_key(|(n, _)| *n);
			(table, list.into_iter().map(|(_, c)| c).collect())
		})
		.collect();

	// Indexes of those tables: which column sets are unique, and which is the primary key.
	let mut indexes: HashMap<u32, Vec<Index>> = HashMap::new();
	for row in pg::query(
		"select i.indrelid::int8, i.indkey::int2[]::int8[],
			i.indisunique and i.indpred is null,
			i.indisprimary
		   from pg_catalog.pg_index i
		   join pg_catalog.pg_class c on c.oid = i.indrelid
		  where c.relnamespace = any($1::int8[])",
		&[pg::Arg::Int8Array(exposed.clone())],
	) {
		let table = row.int8(0).unwrap_or(0) as u32;
		indexes.entry(table).or_default().push(Index {
			columns: column_names(&attnames, table, &row.int8_array(1)),
			unique: row.bool(2).unwrap_or(false),
			primary: row.bool(3).unwrap_or(false),
		});
	}

	// Computed fields: functions whose only argument is a row type.
	let mut functions_by_arg: HashMap<u32, Vec<Rc<Function>>> = HashMap::new();
	for f in &functions {
		if f.args.len() == 1 {
			functions_by_arg
				.entry(f.args[0].type_oid)
				.or_default()
				.push(Rc::clone(f));
		}
	}

	let mut tables: Vec<Rc<Table>> = vec![];
	let mut table_by_oid: HashMap<u32, usize> = HashMap::new();
	for row in pg::query(
		"select c.oid::int8, c.relname::text, c.reltype::int8, c.relrowsecurity,
			n.nspname::text, c.relnamespace::int8,
			pg_catalog.has_table_privilege(current_user, c.oid, 'INSERT'),
			pg_catalog.has_table_privilege(current_user, c.oid, 'SELECT'),
			pg_catalog.has_table_privilege(current_user, c.oid, 'UPDATE'),
			pg_catalog.has_table_privilege(current_user, c.oid, 'DELETE'),
			(select ds.description from pg_catalog.pg_description ds
			  where ds.objoid = c.oid and ds.classoid = 'pg_catalog.pg_class'::regclass and ds.objsubid = 0)
		   from pg_catalog.pg_class c
		   join pg_catalog.pg_namespace n on n.oid = c.relnamespace
		  where c.relnamespace = any($1::int8[])
		    and c.relkind in ('r', 'v', 'm', 'f')
		  order by c.oid",
		&[pg::Arg::Int8Array(exposed.clone())],
	) {
		let oid = row.int8(0).unwrap_or(0) as u32;
		let reltype = row.int8(2).unwrap_or(0) as u32;
		let d = parse_directive(row.text(10))?;
		let enabled = |key: &str| {
			d.get(key)
				.and_then(|v| v.get("enabled"))
				.map(json_text)
				.as_deref() == Some("true")
		};
		let primary_key_directive = match d.get("primary_key_columns") {
			None | Some(Json::Null) => None,
			Some(v) => Some(string_list(v)?),
		};
		let foreign_key_directives = match d.get("foreign_keys") {
			None | Some(Json::Null) => vec![],
			Some(Json::Array(items)) => items
				.iter()
				.map(directive_foreign_key)
				.collect::<Result<_, _>>()?,
			Some(other) => return Err(directive_error(serde_kind(other, "a sequence"))),
		};
		let max_rows = match d.get("max_rows") {
			None | Some(Json::Null) => None,
			Some(v) => {
				let n = sql_int(v).ok_or_else(|| {
					LoadError(format!(
						"invalid input syntax for type integer: \"{}\"",
						json_text(v)
					))
				})?;
				if n < 0 {
					return Err(directive_error(format!(
						"invalid value: integer `{n}`, expected u64"
					)));
				}
				Some(n as u64)
			}
		};
		table_by_oid.insert(oid, tables.len());
		tables.push(Rc::new(Table {
			oid,
			name: row.text(1).unwrap_or_default(),
			rls: row.bool(3).unwrap_or(false),
			schema: row.text(4).unwrap_or_default(),
			schema_oid: row.int8(5).unwrap_or(0) as u32,
			selectable: row.bool(7).unwrap_or(false),
			deletable: row.bool(9).unwrap_or(false),
			columns: columns.remove(&oid).unwrap_or_default(),
			indexes: indexes.remove(&oid).unwrap_or_default(),
			name_override: json_text_opt(d.get("name")),
			description: json_str_strict(d.get("description"))?,
			total_count: enabled("totalCount"),
			aggregate: enabled("aggregate"),
			primary_key_directive,
			foreign_key_directives,
			max_rows,
			functions: functions_by_arg.get(&reltype).cloned().unwrap_or_default(),
		}));
	}

	// Foreign keys whose referenced table is on the search path. A key is followed only between two
	// tables read above (`key_is_selectable`), so its columns are named from theirs.
	let mut raw_keys: Vec<ForeignKey> = vec![];
	for row in pg::query(
		"select k.conrelid::int8, l.relrowsecurity, k.conkey::int8[],
			k.confrelid::int8, r.relrowsecurity, k.confkey::int8[],
			(select ds.description from pg_catalog.pg_description ds
			  where ds.objoid = k.oid and ds.classoid = 'pg_catalog.pg_constraint'::regclass and ds.objsubid = 0)
		   from pg_catalog.pg_constraint k
		   join pg_catalog.pg_class l on l.oid = k.conrelid
		   join pg_catalog.pg_class r on r.oid = k.confrelid
		  where k.contype = 'f'
		    and r.relnamespace = any($1::int8[])
		  order by k.oid",
		&[pg::Arg::Int8Array(path.clone())],
	) {
		let d = parse_directive(row.text(6))?;
		let local = row.int8(0).unwrap_or(0) as u32;
		let referenced = row.int8(3).unwrap_or(0) as u32;
		raw_keys.push(ForeignKey {
			local: KeySide {
				oid: local,
				rls: row.bool(1).unwrap_or(false),
				columns: column_names(&attnames, local, &row.int8_array(2)),
			},
			referenced: KeySide {
				oid: referenced,
				rls: row.bool(4).unwrap_or(false),
				columns: column_names(&attnames, referenced, &row.int8_array(5)),
			},
			local_name: json_text_opt(d.get("local_name")),
			foreign_name: json_text_opt(d.get("foreign_name")),
		});
	}

	let mut catalog = Catalog {
		schemas,
		tables,
		table_by_oid,
		types,
		enums,
		composites,
		functions,
		foreign_keys: vec![],
	};
	catalog.foreign_keys = catalog.followable_keys(raw_keys);
	Ok(catalog)
}

impl Catalog {
	pub fn table(&self, oid: u32) -> Option<&Rc<Table>> {
		self.table_by_oid.get(&oid).map(|&i| &self.tables[i])
	}

	pub fn table_by_name(&self, schema: &str, name: &str) -> Option<&Rc<Table>> {
		self.tables
			.iter()
			.find(|t| t.schema == schema && t.name == name)
	}

	pub fn schema(&self, oid: u32) -> Option<&Schema> {
		self.schemas.get(&oid)
	}

	pub fn inflect(&self, schema_oid: u32) -> bool {
		self.schemas
			.get(&schema_oid)
			.map(|s| s.inflect_names)
			.unwrap_or(false)
	}

	pub fn introspection_anywhere(&self) -> bool {
		self.schemas.values().any(|s| s.introspection)
	}

	pub fn introspection_in(&self, schema_oid: u32) -> bool {
		self.schemas
			.get(&schema_oid)
			.map(|s| s.introspection)
			.unwrap_or(false)
	}

	/// The real foreign keys and the directive-declared ones, keeping those whose columns are
	/// all selectable on both sides.
	fn followable_keys(&self, raw: Vec<ForeignKey>) -> Vec<Rc<ForeignKey>> {
		let mut keys = raw;
		for table in &self.tables {
			for fk in &table.foreign_key_directives {
				let Some(referenced) = self.table_by_name(&fk.foreign_schema, &fk.foreign_table)
				else {
					continue;
				};
				if !fk
					.foreign_columns
					.iter()
					.all(|c| referenced.column(c).is_some())
				{
					continue;
				}
				keys.push(ForeignKey {
					local: KeySide {
						oid: table.oid,
						rls: table.rls,
						columns: fk.local_columns.clone(),
					},
					referenced: KeySide {
						oid: referenced.oid,
						// A directive's key is treated as protected by RLS when the table
						// declaring it is.
						rls: table.rls,
						columns: fk.foreign_columns.clone(),
					},
					local_name: fk.local_name.clone(),
					foreign_name: fk.foreign_name.clone(),
				});
			}
		}
		keys.into_iter()
			.filter(|k| self.key_is_selectable(k))
			.map(Rc::new)
			.collect()
	}

	fn key_is_selectable(&self, key: &ForeignKey) -> bool {
		let (Some(local), Some(referenced)) =
			(self.table(key.local.oid), self.table(key.referenced.oid))
		else {
			return false;
		};
		let selectable = |t: &Table, cols: &[String]| {
			cols.iter()
				.all(|n| t.columns.iter().any(|c| &c.name == n && c.selectable))
		};
		selectable(local, &key.local.columns) && selectable(referenced, &key.referenced.columns)
	}

	/// Whether the local side of a key is covered by a unique index (so it is one-to-one).
	pub fn key_is_locally_unique(&self, key: &ForeignKey) -> bool {
		let Some(table) = self.table(key.local.oid) else {
			return false;
		};
		let cols: HashSet<&String> = key.local.columns.iter().collect();
		table
			.indexes
			.iter()
			.filter(|i| i.unique)
			.any(|i| i.columns.iter().all(|c| cols.contains(c)))
	}
}

/// The names of a table's columns by number, in the order given, skipping a number that is not a
/// column read above (an index's expression is number 0).
fn column_names(
	attnames: &HashMap<u32, HashMap<i64, String>>,
	table: u32,
	numbers: &[i64],
) -> Vec<String> {
	let Some(names) = attnames.get(&table) else {
		return vec![];
	};
	numbers
		.iter()
		.filter_map(|n| names.get(n).cloned())
		.collect()
}

/// The JSON of an object's comment directive, found the way `graphql.comment_directive` finds it:
/// the regular expression `@graphql\((.+)\)`, so from the first `@graphql(` to the LAST `)` after
/// it, with at least one character between. A later `@graphql(` cannot match where the first did
/// not, since its text is a suffix of the first's.
fn directive_json(comment: &str) -> Option<&str> {
	const OPEN: &str = "@graphql(";
	let rest = &comment[comment.find(OPEN)? + OPEN.len()..];
	match rest.rfind(')') {
		Some(end) if end >= 1 => Some(&rest[..end]),
		_ => None,
	}
}

/// An object's comment directive, as `graphql.comment_directive` reads it (a comment without one
/// is `{}`, and so is a directive that is not a JSON object). Parsed here rather than by calling
/// that function per catalogue row, which Postgres cannot inline (it is IMMUTABLE and calls a
/// STABLE function) and so runs as a statement of its own for every column of every table.
/// Text serde_json refuses, or that `jsonb` treats differently (`\u0000`), goes to Postgres, so an
/// invalid directive fails with the same error it always did.
fn parse_directive(comment: Option<String>) -> Result<serde_json::Map<String, Json>, LoadError> {
	let Some(json) = comment.as_deref().and_then(directive_json) else {
		return Ok(serde_json::Map::new());
	};
	let parsed = match serde_json::from_str::<Json>(json) {
		Ok(v) if !json.contains("\\u0000") => v,
		_ => {
			let text = pg::query(
				"select $1::jsonb::text",
				&[pg::Arg::Text(Some(json.to_string()))],
			)
			.first()
			.and_then(|r| r.text(0))
			.unwrap_or_default();
			serde_json::from_str::<Json>(&text).map_err(|e| LoadError(e.to_string()))?
		}
	};
	Ok(match parsed {
		Json::Object(m) => m,
		_ => serde_json::Map::new(),
	})
}

/// A JSON value as `->>` renders it.
fn json_text(v: &Json) -> String {
	match v {
		Json::String(s) => s.clone(),
		other => other.to_string(),
	}
}

fn json_text_opt(v: Option<&Json>) -> Option<String> {
	match v {
		None | Some(Json::Null) => None,
		Some(v) => Some(json_text(v)),
	}
}

/// A directive value that must be a string when present.
fn json_str_strict(v: Option<&Json>) -> Result<Option<String>, LoadError> {
	match v {
		None | Some(Json::Null) => Ok(None),
		Some(Json::String(s)) => Ok(Some(s.clone())),
		Some(other) => Err(directive_error(serde_kind(other, "a string"))),
	}
}

fn string_list(v: &Json) -> Result<Vec<String>, LoadError> {
	match v {
		Json::Array(items) => items
			.iter()
			.map(|i| match i {
				Json::String(s) => Ok(s.clone()),
				other => Err(directive_error(serde_kind(other, "a string"))),
			})
			.collect(),
		other => Err(directive_error(serde_kind(other, "a sequence"))),
	}
}

fn directive_foreign_key(v: &Json) -> Result<DirectiveForeignKey, LoadError> {
	let Json::Object(m) = v else {
		return Err(directive_error(serde_kind(
			v,
			"struct TableDirectiveForeignKey",
		)));
	};
	let required = |name: &str| -> Result<&Json, LoadError> {
		m.get(name)
			.ok_or_else(|| directive_error(format!("missing field `{name}`")))
	};
	let string = |name: &str| -> Result<String, LoadError> {
		match required(name)? {
			Json::String(s) => Ok(s.clone()),
			other => Err(directive_error(serde_kind(other, "a string"))),
		}
	};
	Ok(DirectiveForeignKey {
		local_name: json_str_strict(m.get("local_name"))?,
		local_columns: string_list(required("local_columns")?)?,
		foreign_name: json_str_strict(m.get("foreign_name"))?,
		foreign_schema: string("foreign_schema")?,
		foreign_table: string("foreign_table")?,
		foreign_columns: string_list(required("foreign_columns")?)?,
	})
}

/// How a JSON value would be described by a deserializer that expected something else.
fn serde_kind(v: &Json, expected: &str) -> String {
	let got = match v {
		Json::Null => "null".to_string(),
		Json::Bool(b) => format!("boolean `{b}`"),
		Json::Number(n) if n.is_i64() || n.is_u64() => format!("integer `{n}`"),
		Json::Number(n) => format!("floating point `{n}`"),
		Json::String(s) => format!("string \"{s}\""),
		Json::Array(_) => "sequence".to_string(),
		Json::Object(_) => "map".to_string(),
	};
	format!("invalid type: {got}, expected {expected}")
}

/// The value as `(x ->> 'k')::int` would read it.
fn sql_int(v: &Json) -> Option<i64> {
	match v {
		Json::Number(n) => n.as_i64().filter(|x| i32::try_from(*x).is_ok()),
		Json::String(s) => s.trim().parse::<i32>().ok().map(i64::from),
		_ => None,
	}
}

/// Function argument defaults, as far as the schema can state them. `pg_get_expr` renders them
/// comma separated, one per defaulted argument; anything that is not a plain literal of the
/// argument's type is a default the schema cannot state.
fn arg_defaults(
	rendered: Option<String>,
	num_defaults: usize,
	arg_types: &[u32],
) -> Vec<Option<ArgDefault>> {
	let n = arg_types.len();
	let mut out = vec![None; n];
	let Some(rendered) = rendered else {
		return out;
	};
	if num_defaults == 0 {
		return out;
	}
	let parts: Vec<&str> = rendered.split(',').collect();
	if parts.len() != num_defaults || num_defaults > n {
		return out;
	}
	let start = n - num_defaults;
	for i in start..n {
		let part = parts[i - start].trim();
		let value = if part.starts_with("NULL::") {
			ArgDefault::Null
		} else {
			let parsed = match arg_types[i] {
				21 | 23 => part.parse::<i32>().ok().map(|x| x.to_string()),
				16 => part.parse::<bool>().ok().map(|x| x.to_string()),
				700 | 701 => part.parse::<f64>().ok().map(|x| x.to_string()),
				25 => part
					.strip_suffix("::text")
					.map(|s| format!("\"{}\"", s.trim_matches(',').trim_matches('\''))),
				_ => None,
			};
			parsed.map(ArgDefault::Value).unwrap_or(ArgDefault::Null)
		};
		out[i] = Some(value);
	}
	out
}
