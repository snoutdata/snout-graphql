//! A GraphQL request, start to finish: parse, pick the operation, plan each root field, run its
//! statement, and write the response.
use crate::answers;
use crate::codec::normalize_numbers;
use crate::error::{Error, Result};
use crate::intro::Intro;
use crate::pg;
use crate::plan::{Ctx, QField};
use crate::schema::{FieldKind, Returns, Schema};
use crate::select::{self, response_key};
use crate::sql::{Enums, Sql};
use graphql_parser::query::{
	Definition, OperationDefinition, SelectionSet, VariableDefinition, parse_query,
};
use serde_json::Value as Json;
use std::fmt::Write;

/// The response, as JSON text.
pub struct Response {
	data: Data,
	errors: Vec<String>,
	/// `extensions`' keys and their values as JSON text, when a request asked for any.
	extensions: Vec<(String, String)>,
}

/// What a request asked for through its `extensions`, where a schema directive allows it
/// (`Extras::explain`, `Extras::schema_report`). Without the directive, `extensions` is ignored,
/// as upstream ignores it.
#[derive(Default)]
struct Asked {
	explain: bool,
	schema_report: bool,
}

thread_local! {
	/// Each statement a request ran and its plan, while `explain` was asked for.
	static EXPLAINED: std::cell::RefCell<Option<Vec<Json>>> = const { std::cell::RefCell::new(None) };
}

/// Run a root field's statement, and when `explain` was asked for, record it with its plan first.
fn run_statement(field: &str, statement: &str, sql: &Sql, mutating: bool) -> Option<String> {
	EXPLAINED.with(|e| {
		if let Some(list) = e.borrow_mut().as_mut() {
			let plan =
				pg::query_utility(&format!("explain (format json) {statement}"), &sql.params)
					.first()
					.and_then(|r| r.text(0))
					.and_then(|t| serde_json::from_str::<Json>(&t).ok())
					.unwrap_or(Json::Null);
			let params: Vec<Json> = sql.params.iter().map(pg::Arg::to_json).collect();
			list.push(serde_json::json!({
				"field": field,
				"sql": statement,
				"parameters": params,
				"plan": plan,
			}));
		}
	});
	pg::run(statement, &sql.params, mutating)
}

enum Data {
	Omitted,
	Null,
	/// Response keys and their values as JSON text.
	Fields(Vec<(String, String)>),
}

impl Response {
	fn error(message: impl Into<String>) -> Response {
		Response {
			data: Data::Omitted,
			errors: vec![message.into()],
			extensions: vec![],
		}
	}

	pub fn to_json(&self) -> String {
		let mut out = String::from("{");
		let mut first = true;
		match &self.data {
			Data::Omitted => {}
			Data::Null => {
				out.push_str("\"data\": null");
				first = false;
			}
			Data::Fields(fields) => {
				out.push_str("\"data\": {");
				for (i, (k, v)) in fields.iter().enumerate() {
					if i > 0 {
						out.push_str(", ");
					}
					let _ = write!(out, "{}: {}", Json::String(k.clone()), v);
				}
				out.push('}');
				first = false;
			}
		}
		if !self.errors.is_empty() {
			if !first {
				out.push_str(", ");
			}
			out.push_str("\"errors\": [");
			for (i, e) in self.errors.iter().enumerate() {
				if i > 0 {
					out.push_str(", ");
				}
				let _ = write!(out, "{{\"message\": {}}}", Json::String(e.clone()));
			}
			out.push(']');
			first = false;
		}
		if !self.extensions.is_empty() {
			if !first {
				out.push_str(", ");
			}
			out.push_str("\"extensions\": {");
			for (i, (k, v)) in self.extensions.iter().enumerate() {
				if i > 0 {
					out.push_str(", ");
				}
				let _ = write!(out, "{}: {}", Json::String(k.clone()), v);
			}
			out.push('}');
		}
		out.push('}');
		out
	}
}

/// A request's answer: JSON text, with the key to keep it under when it is introspection only
/// (`answers.rs`), or an answer kept earlier, as `jsonb`.
pub enum Answer {
	Text(String, Option<answers::Key>),
	Kept(std::rc::Rc<Vec<u8>>),
}

pub fn resolve(
	query: Option<String>,
	variables: Option<Json>,
	operation_name: Option<String>,
	extensions: Option<Json>,
) -> Answer {
	// Upstream answers a null query before anything else, schema directives included; so do we,
	// unless a directive has turned on persisted documents (`allowlist.rs`).
	let schema = match (&query, crate::cache::schema()) {
		(_, Ok(s)) => s,
		(None, Err(_)) => return Answer::Text(null_query(), None),
		(Some(q), Err(e)) => {
			// A document that does not parse is reported as such before the schema is read.
			if let Err(p) = parse_query::<String>(q) {
				return Answer::Text(Response::error(p.to_string()).to_json(), None);
			}
			return Answer::Text(Response::error(e.0).to_json(), None);
		}
	};
	let query = match crate::allowlist::document(&schema, query, extensions.as_ref()) {
		Ok(Some(q)) => q,
		Ok(None) => return Answer::Text(null_query(), None),
		Err(message) => return Answer::Text(Response::error(message).to_json(), None),
	};
	let query = query.as_str();
	let document = match parse_query::<String>(query) {
		Ok(d) => d,
		Err(e) => return Answer::Text(Response::error(e.to_string()).to_json(), None),
	};
	let asked = asked(&schema, extensions.as_ref());
	if asked.explain || asked.schema_report {
		EXPLAINED.with(|e| *e.borrow_mut() = asked.explain.then(Vec::new));
		let (mut response, _) = answer(&document, &schema, variables, operation_name);
		let mut extensions = vec![];
		if let Some(list) = EXPLAINED.with(|e| e.borrow_mut().take()) {
			extensions.push(("explain".to_string(), Json::Array(list).to_string()));
		}
		if asked.schema_report {
			extensions.push((
				"schemaReport".to_string(),
				crate::report::report(&schema).to_string(),
			));
		}
		response.extensions = extensions;
		return Answer::Text(response.to_json(), None);
	}
	let key = answers::Key {
		schema: schema.id,
		query: query.to_string(),
		variables: variables.as_ref().map(Json::to_string).unwrap_or_default(),
		operation: operation_name.clone(),
	};
	if let Some(kept) = answers::get(&key) {
		return Answer::Kept(kept);
	}
	let (response, introspection) = answer(&document, &schema, variables, operation_name);
	let text = response.to_json();
	let keep = introspection && response.errors.is_empty();
	Answer::Text(text, keep.then_some(key))
}

fn null_query() -> String {
	"{\"errors\": [{\"message\": \"query must not be null\"}]}".into()
}

fn asked(schema: &Schema, extensions: Option<&Json>) -> Asked {
	let Some(Json::Object(e)) = extensions else {
		return Asked::default();
	};
	let allowed = |f: fn(&crate::catalog::Extras) -> bool| {
		schema.catalog.schemas.values().any(|s| f(&s.extras))
	};
	Asked {
		explain: e.get("explain") == Some(&Json::Bool(true)) && allowed(|x| x.explain),
		schema_report: e.get("schemaReport") == Some(&Json::Bool(true))
			&& allowed(|x| x.schema_report),
	}
}

/// The response, and whether it is introspection only.
fn answer(
	document: &graphql_parser::query::Document<'_, String>,
	schema: &Schema,
	variables: Option<Json>,
	operation_name: Option<String>,
) -> (Response, bool) {
	let variables = match variables.unwrap_or_else(|| Json::Object(Default::default())) {
		Json::Object(m) => m,
		_ => return (Response::error("variables must be an object"), false),
	};

	let mut operations = vec![];
	let mut fragments = vec![];
	for definition in &document.definitions {
		match definition {
			Definition::Operation(op) => operations.push(op.clone()),
			Definition::Fragment(f) => fragments.push(f.clone()),
		}
	}
	let names: Vec<Option<String>> = operations
		.iter()
		.map(|op| match op {
			OperationDefinition::Query(q) => q.name.clone(),
			OperationDefinition::Mutation(m) => m.name.clone(),
			_ => None,
		})
		.collect();
	if names.len() > 1 && names.iter().any(Option::is_none) {
		return (
			Response::error("Anonymous operations must be the only defined operation"),
			false,
		);
	}
	let mut unique = names.clone();
	unique.sort();
	unique.dedup();
	if unique.len() != names.len() {
		return (Response::error("Operation names must be unique"), false);
	}
	let count = operations.len();
	let operation = operations
		.into_iter()
		.zip(&names)
		.find(|(_, name)| **name == operation_name || (count == 1 && operation_name.is_none()))
		.map(|(op, _)| op);
	if let Err(e) = select::check_fragment_cycles(&fragments) {
		return (Response::error(e.0), false);
	}
	let Some(operation) = operation else {
		return (Response::error("Operation not found"), false);
	};
	let depth = match &operation {
		OperationDefinition::Query(q) => select::check_depth(&q.selection_set, &fragments, 1),
		OperationDefinition::SelectionSet(s) => select::check_depth(s, &fragments, 1),
		OperationDefinition::Mutation(m) => select::check_depth(&m.selection_set, &fragments, 1),
		OperationDefinition::Subscription(_) => Ok(()),
	};
	if let Err(e) = depth {
		return (Response::error(e.0), false);
	}
	if schema.catalog.schemas.values().any(|s| s.extras.validation) {
		let errors = crate::validate::validate(schema, document, &operation, &variables);
		if !errors.is_empty() {
			return (
				Response {
					data: Data::Omitted,
					errors,
					extensions: vec![],
				},
				false,
			);
		}
	}
	if let Some(limits) = crate::limits::Limits::of(schema) {
		let (set, root) = match &operation {
			OperationDefinition::Query(q) => (&q.selection_set, Some(schema.query)),
			OperationDefinition::SelectionSet(s) => (s, Some(schema.query)),
			OperationDefinition::Mutation(m) => (&m.selection_set, schema.mutation_type()),
			OperationDefinition::Subscription(s) => (&s.selection_set, None),
		};
		if let Some(root) = root
			&& let Err(e) = crate::limits::check(schema, &limits, set, root, &fragments, &variables)
		{
			return (Response::error(e.0), false);
		}
	}

	let empty: Vec<VariableDefinition<String>> = vec![];
	match &operation {
		OperationDefinition::Query(q) => run_query(
			schema,
			&q.selection_set,
			&variables,
			&q.variable_definitions,
			&fragments,
		),
		OperationDefinition::SelectionSet(s) => {
			run_query(schema, s, &variables, &empty, &fragments)
		}
		OperationDefinition::Mutation(m) => (
			run_mutation(
				schema,
				&m.selection_set,
				&variables,
				&m.variable_definitions,
				&fragments,
			),
			false,
		),
		OperationDefinition::Subscription(_) => {
			(Response::error("Subscriptions are not supported"), false)
		}
	}
}

fn run_query<'d>(
	schema: &Schema,
	set: &SelectionSet<'d, String>,
	variables: &serde_json::Map<String, Json>,
	definitions: &[VariableDefinition<'d, String>],
	fragments: &[graphql_parser::query::FragmentDefinition<'d, String>],
) -> (Response, bool) {
	let ctx = Ctx {
		schema,
		variables,
		definitions,
		fragments,
	};
	let fields = match select::fields(set, fragments, "Query", variables) {
		Ok(f) => f,
		Err(e) => return (Response::error(e.0), false),
	};
	if fields.is_empty() {
		return (Response::error("Selection set must not be empty"), false);
	}
	let introspection = fields
		.iter()
		.all(|f| matches!(f.name.as_str(), "__schema" | "__type" | "__typename"));
	let intro = Intro::new(&ctx);
	let mut data = vec![];
	let mut errors = vec![];
	for qf in &fields {
		match query_field(&ctx, &intro, qf) {
			Ok(value) => data.push((response_key(qf), value)),
			Err(e) => errors.push(e.0),
		}
	}
	let response = Response {
		data: if errors.is_empty() || !data.is_empty() {
			Data::Fields(data)
		} else {
			Data::Null
		},
		errors,
		extensions: vec![],
	};
	(response, introspection)
}

fn query_field<'d>(
	ctx: &Ctx<'_, 'd>,
	intro: &Intro<'_, '_, 'd>,
	qf: &QField<'d>,
) -> Result<String> {
	let schema = ctx.schema;
	if qf.name == "__typename" {
		return Ok("\"Query\"".into());
	}
	let Some(field) = schema.field(schema.query, &qf.name) else {
		return Err(Error::new(format!(
			"Unknown field {:?} on type Query",
			qf.name
		)));
	};
	let enums = Enums(&schema.catalog);
	let mut sql = Sql::new();
	let statement = match &field.kind {
		FieldKind::Collection(table) => {
			let plan = ctx.connection(field, qf, table, crate::plan::Rows::Table, &[])?;
			sql.root_connection(&plan, &enums)?
		}
		FieldKind::NodeEntry => sql.node_entry(&ctx.node_entry(field, qf)?, &enums)?,
		FieldKind::ByPk(table) => sql.by_pk(&ctx.by_pk(field, qf, table)?, &enums)?,
		FieldKind::QueryFunction(function, returns) => {
			sql.function_call(&ctx.function_call(field, qf, function, returns)?, &enums)?
		}
		FieldKind::IntroType => return Ok(intro.type_by_name(field, qf)?.to_string()),
		FieldKind::IntroSchema => return Ok(intro.schema_json(qf)?.to_string()),
		_ => {
			return Err(Error::new(format!(
				"Unknown field {:?} on type Query",
				qf.name
			)));
		}
	};
	Ok(run_statement(&response_key(qf), &statement, &sql, false)
		.map(|t| normalize_numbers(&t))
		.unwrap_or_else(|| "null".into()))
}

fn run_mutation<'d>(
	schema: &Schema,
	set: &SelectionSet<'d, String>,
	variables: &serde_json::Map<String, Json>,
	definitions: &[VariableDefinition<'d, String>],
	fragments: &[graphql_parser::query::FragmentDefinition<'d, String>],
) -> Response {
	let Some(mutation_type) = schema.mutation_type() else {
		return Response {
			data: Data::Null,
			errors: vec!["Unknown type Mutation".into()],
			extensions: vec![],
		};
	};
	let ctx = Ctx {
		schema,
		variables,
		definitions,
		fragments,
	};
	let fields = match select::fields(set, fragments, "Mutation", variables) {
		Ok(f) => f,
		Err(e) => return Response::error(e.0),
	};
	let mut data = vec![];
	let outcome = (|| -> Result<()> {
		if fields.is_empty() {
			return Err(Error::new("Selection set must not be empty"));
		}
		for qf in &fields {
			data.push((response_key(qf), mutation_field(&ctx, mutation_type, qf)?));
		}
		Ok(())
	})();
	match outcome {
		Ok(()) => Response {
			data: Data::Fields(data),
			errors: vec![],
			extensions: vec![],
		},
		// A failed mutation undoes the ones before it: the whole request is one statement's
		// worth of work, raised as an error the caller's transaction sees.
		Err(e) => {
			pgrx::ereport!(ERROR, pgrx::PgSqlErrorCode::ERRCODE_INTERNAL_ERROR, e.0);
		}
	}
}

fn mutation_field<'d>(ctx: &Ctx<'_, 'd>, mutation_type: usize, qf: &QField<'d>) -> Result<String> {
	let schema = ctx.schema;
	if qf.name == "__typename" {
		return Ok("\"Mutation\"".into());
	}
	let Some(field) = schema.field(mutation_type, &qf.name) else {
		return Err(Error::new(format!(
			"Unknown field \"{}\" on type Mutation",
			qf.name
		)));
	};
	let enums = Enums(&schema.catalog);
	let mut sql = Sql::new();
	let statement = match &field.kind {
		FieldKind::Insert(table) => sql.insert(&ctx.insert(field, qf, table)?, &enums)?,
		FieldKind::Update(table) => sql.update(&ctx.update(field, qf, table)?, &enums)?,
		FieldKind::Delete(table) => sql.delete(&ctx.delete(field, qf, table)?, &enums)?,
		FieldKind::MutationFunction(function, returns) => {
			sql.function_call(&ctx.function_call(field, qf, function, returns)?, &enums)?
		}
		_ => {
			return Err(Error::new(format!(
				"Unknown field \"{}\" on type Mutation",
				qf.name
			)));
		}
	};
	let _: Option<&Returns> = None;
	Ok(run_statement(&response_key(qf), &statement, &sql, true)
		.map(|t| normalize_numbers(&t))
		.unwrap_or_else(|| "null".into()))
}
