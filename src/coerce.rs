//! Checking an argument's value against its type, before any SQL exists.
//!
//! The sentences are part of what a client sees, so they are the ones clients already know. What
//! is checked is more than it was: a `BigInt`, a `UUID`, a date or a time that Postgres would
//! refuse is refused here, as a GraphQL error naming the type, instead of reaching the database.
use crate::error::{Error, Result};
use crate::schema::{InputDef, Kind, Scalar, Schema, Source, TypeId, TypeRef};
use crate::value::In;
use std::collections::BTreeMap;

pub fn coerce(schema: &Schema, ty: &TypeRef, value: &In) -> Result<In> {
	match ty {
		TypeRef::NonNull(inner) => {
			let out = coerce(schema, inner, value)?;
			if out.is_missing() {
				return Err(Error::new("Invalid input for NonNull type"));
			}
			Ok(out)
		}
		TypeRef::List(inner) => match value {
			In::Absent | In::Null => Ok(value.clone()),
			In::List(items) => Ok(In::List(
				items
					.iter()
					.map(|i| coerce(schema, inner, i))
					.collect::<Result<_>>()?,
			)),
			// A single value where a list is expected is a list of one.
			other => Ok(In::List(vec![coerce(schema, inner, other)?])),
		},
		TypeRef::Named { id, max_len } => coerce_named(schema, *id, *max_len, value),
	}
}

fn coerce_named(schema: &Schema, id: TypeId, max_len: Option<i32>, value: &In) -> Result<In> {
	let t = schema.ty(id);
	match &t.source {
		Source::Scalar(s) => coerce_scalar(*s, max_len, value),
		Source::Enum(e) => match value {
			In::Absent | In::Null => Ok(value.clone()),
			In::Str(s) if e.values.iter().any(|v| &e.to_graphql(v) == s) => {
				Ok(In::Str(e.db_label(s).unwrap_or_else(|| s.clone())))
			}
			_ => Err(Error::new(format!("Invalid input for {} type", t.name))),
		},
		Source::FilterIs | Source::OrderByDirection | Source::ExtraEnum(..) => match value {
			In::Absent | In::Null => Ok(value.clone()),
			In::Str(s)
				if schema
					.enum_values(id)
					.unwrap_or_default()
					.iter()
					.any(|(v, _)| v == s) =>
			{
				Ok(value.clone())
			}
			_ => Err(Error::new(format!("Invalid input for {} type", t.name))),
		},
		Source::FilterScalar(Scalar::String) if max_len.is_some() => coerce_object(
			schema,
			&t.name,
			&schema.scalar_filter_inputs(Scalar::String, max_len),
			value,
		),
		Source::InsertInput(_)
		| Source::UpdateInput(_)
		| Source::OrderByEntity(_)
		| Source::FilterScalar(_)
		| Source::FilterList(_)
		| Source::FilterEnum(_)
		| Source::FilterEnumList(_)
		| Source::CompositeFilter(_)
		| Source::GeoDistance
		| Source::FilterEntity(_)
		| Source::CollectionFilter(_)
		| Source::OnConflict(_)
		| Source::CollectionOrderBy(_) => {
			debug_assert_eq!(t.kind, Kind::InputObject);
			coerce_object(schema, &t.name, schema.inputs(id).unwrap_or(&[]), value)
		}
		_ => Err(Error::new(format!(
			"Invalid Type used as input argument {}",
			t.name
		))),
	}
}

fn coerce_object(schema: &Schema, name: &str, fields: &[InputDef], value: &In) -> Result<In> {
	match value {
		In::Absent | In::Null => Ok(value.clone()),
		In::Object(given) => {
			let extra: Vec<&String> = given
				.keys()
				.filter(|k| !fields.iter().any(|f| &f.name == *k))
				.collect();
			if !extra.is_empty() {
				return Err(Error::new(format!(
					"Input for type {name} contains extra keys {extra:?}"
				)));
			}
			let mut out = BTreeMap::new();
			for field in fields {
				match given.get(&field.name) {
					None => {
						coerce(schema, &field.ty, &In::Null)?;
					}
					Some(v) => {
						out.insert(field.name.clone(), coerce(schema, &field.ty, v)?);
					}
				}
			}
			Ok(In::Object(out))
		}
		_ => Err(Error::new(format!("Invalid input for {name} type"))),
	}
}

/// How the type is named in the sentence that refuses it.
fn debug_name(s: Scalar, max_len: Option<i32>) -> String {
	match s {
		Scalar::String => match max_len {
			None => "String(None)".to_string(),
			Some(n) => format!("String(Some({n}))"),
		},
		other => other.name().to_string(),
	}
}

fn coerce_scalar(s: Scalar, max_len: Option<i32>, value: &In) -> Result<In> {
	let refuse = || {
		Err(Error::new(format!(
			"Invalid input for {} type",
			debug_name(s, max_len)
		)))
	};
	if value.is_missing() {
		return Ok(value.clone());
	}
	match s {
		Scalar::String => match value {
			In::Str(text) => {
				if let Some(max) = max_len
					&& text.chars().count() as i64 > i64::from(max)
				{
					return Err(Error::new(format!(
						"Invalid input for String type. Maximum character length {max}"
					)));
				}
				Ok(value.clone())
			}
			_ => refuse(),
		},
		Scalar::Int => match value {
			In::Int(_) => Ok(value.clone()),
			_ => refuse(),
		},
		Scalar::Float => match value {
			In::Int(_) | In::Float(_) => Ok(value.clone()),
			_ => refuse(),
		},
		Scalar::Boolean => match value {
			In::Bool(_) => Ok(value.clone()),
			_ => refuse(),
		},
		Scalar::BigInt => match value {
			In::Int(_) => Ok(value.clone()),
			In::Float(f) if f.fract() == 0.0 && f.abs() < 9.2e18 => Ok(value.clone()),
			// A string must be an integer Postgres can read as a bigint.
			In::Str(text) if is_bigint(text) => Ok(value.clone()),
			In::Str(_) | In::Float(_) => refuse(),
			_ => refuse(),
		},
		Scalar::BigFloat => match value {
			In::Str(_) => Ok(value.clone()),
			_ => Err(Error::new(format!(
				"Invalid input for {} type. String required",
				debug_name(s, max_len)
			))),
		},
		Scalar::Uuid => match value {
			In::Str(text) if is_uuid(text) => Ok(value.clone()),
			_ => refuse(),
		},
		Scalar::Date
		| Scalar::Time
		| Scalar::Datetime
		| Scalar::Json
		| Scalar::Cursor
		| Scalar::ID => match value {
			In::Str(_) => Ok(value.clone()),
			_ => refuse(),
		},
		// No check is possible for a type the schema does not know; Postgres reads it.
		Scalar::Opaque => Ok(value.clone()),
		// GeoJSON as an object (the natural literal) or as its text; PostGIS reads it.
		Scalar::GeoJson => match value {
			In::Str(_) => Ok(value.clone()),
			In::Object(_) => Ok(In::Str(value.to_json()?.to_string())),
			_ => refuse(),
		},
	}
}

/// Whether Postgres's `int8in` accepts the text: optional surrounding space, an optional sign,
/// decimal digits (with `_` separators between digits), in range.
fn is_bigint(text: &str) -> bool {
	let t = text.trim_matches(|c: char| c.is_ascii_whitespace());
	let (negative, digits) = match t.as_bytes().first() {
		Some(b'-') => (true, &t[1..]),
		Some(b'+') => (false, &t[1..]),
		_ => (false, t),
	};
	if digits.is_empty()
		|| digits.starts_with('_')
		|| digits.ends_with('_')
		|| digits.contains("__")
	{
		return false;
	}
	if !digits.bytes().all(|b| b.is_ascii_digit() || b == b'_') {
		return false;
	}
	let plain: String = digits.chars().filter(|c| *c != '_').collect();
	let signed = if negative { format!("-{plain}") } else { plain };
	signed.parse::<i64>().is_ok()
}

/// Whether Postgres's `uuid_in` accepts the text: 32 hex digits, optionally in braces, with a
/// hyphen allowed after any group of four.
fn is_uuid(text: &str) -> bool {
	let t = text
		.strip_prefix('{')
		.and_then(|x| x.strip_suffix('}'))
		.unwrap_or(text);
	let mut digits = 0;
	let mut since_hyphen = 0;
	let mut prev_hyphen = true;
	for c in t.chars() {
		if c == '-' {
			if prev_hyphen || since_hyphen % 4 != 0 || digits == 32 {
				return false;
			}
			prev_hyphen = true;
			continue;
		}
		if !c.is_ascii_hexdigit() {
			return false;
		}
		digits += 1;
		since_hyphen = if prev_hyphen { 1 } else { since_hyphen + 1 };
		prev_hyphen = false;
	}
	digits == 32 && !prev_hyphen
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn bigint_text() {
		for ok in [
			"1",
			"-9223372036854775808",
			"9223372036854775807",
			" 42 ",
			"+7",
			"1_000",
		] {
			assert!(is_bigint(ok), "{ok}");
		}
		for bad in [
			"",
			"not-an-int",
			"1.5",
			"9223372036854775808",
			"1__0",
			"_1",
			"0x10",
			"--1",
		] {
			assert!(!is_bigint(bad), "{bad}");
		}
	}

	#[test]
	fn uuid_text() {
		for ok in [
			"a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11",
			"A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11",
			"{a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11}",
			"a0eebc999c0b4ef8bb6d6bb9bd380a11",
			"a0ee-bc99-9c0b-4ef8-bb6d-6bb9-bd38-0a11",
		] {
			assert!(is_uuid(ok), "{ok}");
		}
		for bad in [
			"",
			"abc",
			"a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a1",
			"a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11-",
			"g0eebc999c0b4ef8bb6d6bb9bd380a11",
		] {
			assert!(!is_uuid(bad), "{bad}");
		}
	}
}
