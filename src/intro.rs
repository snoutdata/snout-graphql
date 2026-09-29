//! Introspection: `__schema` and `__type`, answered from the schema in memory.
//!
//! An introspection query asks the same questions of every type (`fields { name type { ... } }`),
//! so each selection set is expanded once and reused, not once per type.
use crate::error::{Error, Result};
use crate::plan::{Ctx, QField};
use crate::schema::{FieldDef, InputDef, Kind, Schema, TypeId, TypeRef};
use crate::select::response_key;
use crate::value::from_literal;
use serde_json::{Map, Value as Json};
use std::cell::RefCell;
use std::collections::HashMap;

/// A selection set's fields on one meta type, keyed by the set's address and the type's name, so
/// the same set under every described type is expanded once.
type Expanded<'d> = RefCell<HashMap<(usize, &'static str), std::rc::Rc<Vec<QField<'d>>>>>;

pub struct Intro<'c, 'a, 'd> {
	ctx: &'c Ctx<'a, 'd>,
	expanded: Expanded<'d>,
}

impl<'c, 'a, 'd> Intro<'c, 'a, 'd> {
	pub fn new(ctx: &'c Ctx<'a, 'd>) -> Self {
		Intro {
			ctx,
			expanded: RefCell::new(HashMap::new()),
		}
	}

	fn schema(&self) -> &Schema {
		self.ctx.schema
	}

	fn fields_of(
		&self,
		qf: &QField<'d>,
		type_name: &'static str,
	) -> Result<std::rc::Rc<Vec<QField<'d>>>> {
		let key = (&qf.selection_set as *const _ as usize, type_name);
		if let Some(f) = self.expanded.borrow().get(&key) {
			return Ok(std::rc::Rc::clone(f));
		}
		let fields = std::rc::Rc::new(crate::select::fields(
			&qf.selection_set,
			self.ctx.fragments,
			type_name,
			self.ctx.variables,
		)?);
		self.expanded
			.borrow_mut()
			.insert(key, std::rc::Rc::clone(&fields));
		Ok(fields)
	}

	/// `__type(name:)`.
	pub fn type_by_name(&self, field: &FieldDef, qf: &QField<'d>) -> Result<Json> {
		let name = match qf.arguments.iter().find(|(n, _)| n == "name") {
			None => return Err(Error::new("Internal Error: failed to parse validated name")),
			Some((_, literal)) => {
				let value = from_literal(literal, self.ctx.variables, self.ctx.definitions)
					.and_then(|v| crate::coerce::coerce(self.schema(), &field.args[0].ty, &v));
				match value {
					Ok(crate::value::In::Str(s)) => s,
					Ok(_) => {
						return Err(Error::new("Internal Error: failed to parse validated name"));
					}
					Err(_) => return Err(Error::new("no name found for __type")),
				}
			}
		};
		match self.schema().lookup(&name) {
			Some(id) if self.listed(id) && self.schema().introspectable(id) => {
				self.type_json(&TypeRef::named(id), qf)
			}
			_ => Ok(Json::Null),
		}
	}

	fn listed(&self, id: TypeId) -> bool {
		self.schema().ty(id).listed || Some(id) == self.schema().mutation_type()
	}

	/// `__schema`.
	pub fn schema_json(&self, qf: &QField<'d>) -> Result<Json> {
		let s = self.schema();
		let mut out = Map::new();
		for sel in self.fields_of(qf, "__Schema")?.iter() {
			let key = response_key(sel);
			let value = match sel.name.as_str() {
				"description" => {
					Json::String("Represents the GraphQL schema of the database".into())
				}
				"types" => Json::Array(
					s.listed_types()
						.into_iter()
						.filter(|&id| s.introspectable(id))
						.map(|id| self.type_json(&TypeRef::named(id), sel))
						.collect::<Result<_>>()?,
				),
				"queryType" => self.type_json(&TypeRef::named(s.query), sel)?,
				"mutationType" => match s.mutation_type() {
					Some(id) => self.type_json(&TypeRef::named(id), sel)?,
					None => Json::Null,
				},
				"subscriptionType" => Json::Null,
				"directives" => Json::Array(vec![
					self.directive_json(
						sel,
						"include",
						"This field or fragment will be included only when the `if` argument is true.",
						"Included when true",
					)?,
					self.directive_json(
						sel,
						"skip",
						"This field or fragment will be skipped when the `if` argument is true.",
						"Skipped when true",
					)?,
				]),
				"__typename" => Json::String("__Schema".into()),
				other => return Err(Error::new(format!("unknown field in __Schema: {other}"))),
			};
			out.insert(key, value);
		}
		Ok(Json::Object(out))
	}

	fn directive_json(
		&self,
		qf: &QField<'d>,
		name: &str,
		description: &str,
		arg_description: &str,
	) -> Result<Json> {
		let s = self.schema();
		let arg = InputDef {
			name: "if".into(),
			description: Some(arg_description.into()),
			ty: s.scalar(crate::schema::Scalar::Boolean).non_null(),
			default_value: None,
			kind: crate::schema::InputKind::Plain,
		};
		let mut out = Map::new();
		for sel in self.fields_of(qf, "__Directive")?.iter() {
			let key = response_key(sel);
			let value = match sel.name.as_str() {
				"name" => Json::String(name.into()),
				"description" => Json::String(description.into()),
				"locations" => Json::Array(
					["FIELD", "FRAGMENT_SPREAD", "INLINE_FRAGMENT"]
						.iter()
						.map(|l| Json::String((*l).into()))
						.collect(),
				),
				"args" => Json::Array(vec![self.input_json(&arg, sel)?]),
				"isRepeatable" => Json::Bool(false),
				"__typename" => Json::String("__Directive".into()),
				other => return Err(Error::new(format!("unknown field {other} in __Directive"))),
			};
			out.insert(key, value);
		}
		Ok(Json::Object(out))
	}

	pub fn type_json(&self, ty: &TypeRef, qf: &QField<'d>) -> Result<Json> {
		let s = self.schema();
		let named = match ty {
			TypeRef::Named { id, .. } => Some(*id),
			_ => None,
		};
		let mut out = Map::new();
		for sel in self.fields_of(qf, "__Type")?.iter() {
			let key = response_key(sel);
			let value = match sel.name.as_str() {
				"kind" => Json::String(
					match ty {
						TypeRef::List(_) => "LIST",
						TypeRef::NonNull(_) => "NON_NULL",
						TypeRef::Named { id, .. } => s.ty(*id).kind.as_str(),
					}
					.into(),
				),
				"name" => named
					.map(|id| Json::String(s.ty(id).name.clone()))
					.unwrap_or(Json::Null),
				"description" => named
					.and_then(|id| s.description(id))
					.map(Json::String)
					.unwrap_or(Json::Null),
				"fields" => match named.and_then(|id| s.fields(id)) {
					None => Json::Null,
					Some(fields) => {
						let mut list = vec![];
						for f in fields {
							if f.name == "__type" || f.name == "__schema" {
								continue;
							}
							let owner = f.function_schema.or_else(|| s.type_schema(f.ty.base()));
							if let Some(oid) = owner
								&& !s.catalog.introspection_in(oid)
							{
								continue;
							}
							list.push(self.field_json(f, sel)?);
						}
						Json::Array(list)
					}
				},
				"inputFields" => match named.and_then(|id| s.inputs(id)) {
					None => Json::Null,
					Some(inputs) => Json::Array(
						inputs
							.iter()
							.map(|i| self.input_json(i, sel))
							.collect::<Result<_>>()?,
					),
				},
				"interfaces" => {
					let has_node = named
						.map(
							|id| matches!(&s.ty(id).source, crate::schema::Source::Node(t) if t.primary_key().is_some()),
						)
						.unwrap_or(false);
					if has_node {
						Json::Array(vec![
							self.type_json(&TypeRef::named(s.node_interface), sel)?,
						])
					} else {
						Json::Array(vec![])
					}
				}
				"possibleTypes" => match named {
					Some(id) if id == s.node_interface => Json::Array(
						s.node_types()
							.into_iter()
							.map(|t| self.type_json(&TypeRef::named(t), sel))
							.collect::<Result<_>>()?,
					),
					_ => Json::Null,
				},
				"enumValues" => {
					let values = named.and_then(|id| s.enum_values(id)).unwrap_or_default();
					Json::Array(
						values
							.iter()
							.map(|(name, description)| {
								self.enum_value_json(name, *description, sel)
							})
							.collect::<Result<_>>()?,
					)
				}
				"ofType" => match ty {
					TypeRef::List(inner) | TypeRef::NonNull(inner) => self.type_json(inner, sel)?,
					_ => Json::Null,
				},
				"specifiedByURL" => Json::Null,
				// The described type's name, as upstream answers (and its own tests rely on).
				"__typename" => named
					.map(|id| Json::String(s.ty(id).name.clone()))
					.unwrap_or(Json::Null),
				other => return Err(Error::new(format!("unknown field on __Type: {other}"))),
			};
			out.insert(key, value);
		}
		let _ = Kind::Scalar;
		Ok(Json::Object(out))
	}

	fn field_json(&self, field: &FieldDef, qf: &QField<'d>) -> Result<Json> {
		let mut out = Map::new();
		for sel in self.fields_of(qf, "__Field")?.iter() {
			let key = response_key(sel);
			let value = match sel.name.as_str() {
				"name" => Json::String(field.name.clone()),
				"description" => field
					.description
					.clone()
					.map(Json::String)
					.unwrap_or(Json::Null),
				"args" => Json::Array(
					field
						.args
						.iter()
						.map(|a| self.input_json(a, sel))
						.collect::<Result<_>>()?,
				),
				"type" => self.type_json(&field.ty, sel)?,
				"isDeprecated" => Json::Bool(false),
				"deprecationReason" => Json::Null,
				"__typename" => Json::String("__Field".into()),
				other => return Err(Error::new(format!("unknown field in __Field {other}"))),
			};
			out.insert(key, value);
		}
		Ok(Json::Object(out))
	}

	fn input_json(&self, input: &InputDef, qf: &QField<'d>) -> Result<Json> {
		let mut out = Map::new();
		for sel in self.fields_of(qf, "__InputValue")?.iter() {
			let key = response_key(sel);
			let value = match sel.name.as_str() {
				"name" => Json::String(input.name.clone()),
				"description" => input
					.description
					.clone()
					.map(Json::String)
					.unwrap_or(Json::Null),
				"type" => self.type_json(&input.ty, sel)?,
				"defaultValue" => input
					.default_value
					.clone()
					.map(Json::String)
					.unwrap_or(Json::Null),
				"isDeprecated" => Json::Bool(false),
				"deprecationReason" => Json::Null,
				"__typename" => Json::String("__InputValue".into()),
				other => {
					return Err(Error::new(format!(
						"unknown field in __InputValue: {other}"
					)));
				}
			};
			out.insert(key, value);
		}
		Ok(Json::Object(out))
	}

	fn enum_value_json(
		&self,
		name: &str,
		description: Option<&str>,
		qf: &QField<'d>,
	) -> Result<Json> {
		let mut out = Map::new();
		for sel in self.fields_of(qf, "__EnumValue")?.iter() {
			let key = response_key(sel);
			let value = match sel.name.as_str() {
				"name" => Json::String(name.into()),
				"description" => description
					.map(|d| Json::String(d.into()))
					.unwrap_or(Json::Null),
				"isDeprecated" => Json::Bool(false),
				"deprecationReason" => Json::Null,
				"__typename" => Json::String("__EnumValue".into()),
				other => return Err(Error::new(format!("unknown field in __EnumValue: {other}"))),
			};
			out.insert(key, value);
		}
		Ok(Json::Object(out))
	}
}
