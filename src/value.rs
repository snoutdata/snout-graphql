//! Input values: argument literals and variables, before and after they are checked against the
//! argument's type. `Absent` is a value that was not given at all, which is different from `null`.
use crate::error::Error;
use graphql_parser::query::{Value as Literal, VariableDefinition};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq)]
pub enum In {
	Absent,
	Null,
	Int(i64),
	Float(f64),
	Str(String),
	Bool(bool),
	List(Vec<In>),
	Object(BTreeMap<String, In>),
}

impl In {
	pub fn is_absent(&self) -> bool {
		matches!(self, In::Absent)
	}

	pub fn is_missing(&self) -> bool {
		matches!(self, In::Absent | In::Null)
	}

	pub fn from_json(v: &serde_json::Value) -> Result<In, Error> {
		use serde_json::Value as J;
		Ok(match v {
			J::Null => In::Null,
			J::Bool(b) => In::Bool(*b),
			J::String(s) => In::Str(s.clone()),
			J::Number(n) => match n.as_i64() {
				Some(i) => In::Int(i),
				None => In::Float(
					n.as_f64()
						.ok_or_else(|| Error::new("Failed to handle numeric user input"))?,
				),
			},
			J::Array(items) => In::List(items.iter().map(In::from_json).collect::<Result<_, _>>()?),
			J::Object(m) => {
				let mut out = BTreeMap::new();
				for (k, v) in m {
					out.insert(k.clone(), In::from_json(v)?);
				}
				In::Object(out)
			}
		})
	}

	/// The value as JSON. An absent value has no JSON form; asking for one is a mistake the
	/// caller made, reported as upstream words it.
	pub fn to_json(&self) -> Result<serde_json::Value, Error> {
		use serde_json::Value as J;
		Ok(match self {
			In::Absent => {
				return Err(Error::new(
					"Encountered `Absent` value while transforming between GraphQL intermediate object notation and JSON",
				));
			}
			In::Null => J::Null,
			In::Bool(b) => J::Bool(*b),
			In::Str(s) => J::String(s.clone()),
			In::Int(i) => J::from(*i),
			In::Float(f) => serde_json::Number::from_f64(*f)
				.map(J::Number)
				.unwrap_or(J::Null),
			In::List(items) => J::Array(items.iter().map(In::to_json).collect::<Result<_, _>>()?),
			In::Object(m) => {
				let mut out = serde_json::Map::new();
				for (k, v) in m {
					out.insert(k.clone(), v.to_json()?);
				}
				J::Object(out)
			}
		})
	}
}

/// A literal from the document, with variables substituted: the value sent, else the variable's
/// default, else absent.
pub fn from_literal(
	literal: &Literal<'_, String>,
	variables: &serde_json::Map<String, serde_json::Value>,
	definitions: &[VariableDefinition<'_, String>],
) -> Result<In, Error> {
	Ok(match literal {
		Literal::Null => In::Null,
		Literal::Boolean(b) => In::Bool(*b),
		Literal::Int(n) => In::Int(n.as_i64().ok_or_else(|| Error::new("Invalid Int input"))?),
		Literal::Float(f) => In::Float(*f),
		Literal::String(s) => In::Str(s.clone()),
		Literal::Enum(e) => In::Str(e.clone()),
		Literal::List(items) => In::List(
			items
				.iter()
				.map(|i| from_literal(i, variables, definitions))
				.collect::<Result<_, _>>()?,
		),
		Literal::Object(m) => {
			let mut out = BTreeMap::new();
			for (k, v) in m {
				out.insert(k.clone(), from_literal(v, variables, definitions)?);
			}
			In::Object(out)
		}
		Literal::Variable(name) => match variables.get(name) {
			Some(v) => In::from_json(v)?,
			None => match definitions
				.iter()
				.find(|d| &d.name == name)
				.and_then(|d| d.default_value.as_ref())
			{
				Some(default) => from_literal(default, variables, definitions)?,
				None => In::Absent,
			},
		},
	})
}
