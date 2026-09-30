//! A selection set as the fields it asks for: fragments expanded, `@skip` and `@include` applied,
//! and fields asked for twice under one response key merged into one.
use crate::error::{Error, Result};
use graphql_parser::query::{
	Field, FragmentDefinition, Selection, SelectionSet, TypeCondition, Value,
};

pub type Fragments<'a> = [FragmentDefinition<'a, String>];

pub fn response_key(field: &Field<'_, String>) -> String {
	field.alias.clone().unwrap_or_else(|| field.name.clone())
}

/// The fields of `set` on type `type_name`.
pub fn fields<'a>(
	set: &SelectionSet<'a, String>,
	fragments: &Fragments<'a>,
	type_name: &str,
	variables: &serde_json::Map<String, serde_json::Value>,
) -> Result<Vec<Field<'a, String>>> {
	let mut out: Vec<Field<'a, String>> = vec![];
	for selection in &set.items {
		for field in expand(selection, fragments, type_name, variables)? {
			merge_into(&mut out, field)?;
		}
	}
	Ok(out)
}

fn merge_into<'a>(out: &mut Vec<Field<'a, String>>, field: Field<'a, String>) -> Result<()> {
	let key = response_key(&field);
	match out.iter_mut().find(|f| response_key(f) == key) {
		None => out.push(field),
		Some(existing) => {
			if existing.name != field.name {
				return Err(Error::new(format!(
					"Fields `{}` and `{}` are different",
					field.name, existing.name
				)));
			}
			if !same_arguments(&field.arguments, &existing.arguments) {
				return Err(Error::new(format!(
					"Two fields named `{}` have different arguments",
					field.name
				)));
			}
			existing
				.selection_set
				.items
				.extend(field.selection_set.items);
		}
	}
	Ok(())
}

fn same_arguments<'a>(
	a: &[(String, Value<'a, String>)],
	b: &[(String, Value<'a, String>)],
) -> bool {
	a.len() == b.len()
		&& b.iter()
			.all(|(name, value)| a.iter().any(|(n, v)| n == name && v == value))
}

fn expand<'a>(
	selection: &Selection<'a, String>,
	fragments: &Fragments<'a>,
	type_name: &str,
	variables: &serde_json::Map<String, serde_json::Value>,
) -> Result<Vec<Field<'a, String>>> {
	if skipped(selection, variables)? {
		return Ok(vec![]);
	}
	match selection {
		Selection::Field(field) => Ok(vec![field.clone()]),
		Selection::FragmentSpread(spread) => {
			let definition = fragments.iter().find(|d| {
				d.name == spread.fragment_name
					&& matches!(&d.type_condition, TypeCondition::On(t) if t == type_name)
			});
			match definition {
				Some(d) => fields(&d.selection_set, fragments, type_name, variables),
				None => Err(Error::new(format!(
					"no fragment named {} on type {}",
					spread.fragment_name, type_name
				))),
			}
		}
		Selection::InlineFragment(inline) => {
			let applies = match &inline.type_condition {
				Some(TypeCondition::On(t)) => t == type_name,
				None => true,
			};
			if applies {
				fields(&inline.selection_set, fragments, type_name, variables)
			} else {
				Ok(vec![])
			}
		}
	}
}

/// Whether `@skip` or `@include` leaves the selection out.
pub fn skipped(
	selection: &Selection<'_, String>,
	variables: &serde_json::Map<String, serde_json::Value>,
) -> Result<bool> {
	let directives = match selection {
		Selection::Field(x) => &x.directives,
		Selection::FragmentSpread(x) => &x.directives,
		Selection::InlineFragment(x) => &x.directives,
	};
	for directive in directives {
		let skip_when = match directive.name.as_str() {
			"skip" => true,
			"include" => false,
			other => return Err(Error::new(format!("Unknown directive {other}"))),
		};
		let name = &directive.name;
		if directive.arguments.len() != 1 {
			return Err(Error::new(format!(
				"Incorrect arguments to directive @{name}"
			)));
		}
		let (arg, value) = &directive.arguments[0];
		if arg != "if" {
			return Err(Error::new(format!("Unknown argument to @{name}: {arg}")));
		}
		let condition = match value {
			Value::Boolean(b) => Some(*b),
			Value::Variable(var) => match variables.get(var) {
				Some(serde_json::Value::Bool(b)) => Some(*b),
				_ => {
					return Err(Error::new(format!(
						"Value for \"if\" in @{name} directive is required"
					)));
				}
			},
			_ => None,
		};
		if condition == Some(skip_when) {
			return Ok(true);
		}
	}
	Ok(false)
}

/// Upstream's limits on a document's shape: nesting depth, and fragments that spread themselves.
pub const MAX_DEPTH: u32 = 32;
const FRAGMENT_STACK_LIMIT: u32 = 50;

/// How many selections a document may expand to, fragments spread where they are used. A document
/// of a few kilobytes whose fragments each spread the next twice expands to billions, which is a
/// request that never answers; this refuses it while it is being counted.
pub const MAX_EXPANDED: u64 = 1_000_000;

pub fn check_depth(
	set: &SelectionSet<'_, String>,
	fragments: &Fragments<'_>,
	depth: u32,
) -> Result<()> {
	let mut seen = 0;
	walk_depth(set, fragments, depth, &mut seen)
}

fn walk_depth(
	set: &SelectionSet<'_, String>,
	fragments: &Fragments<'_>,
	depth: u32,
	seen: &mut u64,
) -> Result<()> {
	if depth > MAX_DEPTH {
		return Err(Error::new(format!(
			"Query selection depth exceeds the maximum allowed depth of {MAX_DEPTH}"
		)));
	}
	for selection in &set.items {
		*seen += 1;
		if *seen > MAX_EXPANDED {
			return Err(Error::new(format!(
				"The document expands to more than {MAX_EXPANDED} selections once its fragments are spread"
			)));
		}
		match selection {
			Selection::Field(f) => walk_depth(&f.selection_set, fragments, depth + 1, seen)?,
			Selection::FragmentSpread(s) => {
				if let Some(d) = fragments.iter().find(|d| d.name == s.fragment_name) {
					walk_depth(&d.selection_set, fragments, depth, seen)?;
				}
			}
			Selection::InlineFragment(i) => walk_depth(&i.selection_set, fragments, depth, seen)?,
		}
	}
	Ok(())
}

pub fn check_fragment_cycles(fragments: &Fragments<'_>) -> Result<()> {
	let mut seen = 0;
	for f in fragments {
		let mut visiting = vec![];
		fragment_cycle(f, fragments, &mut visiting, 1, &mut seen)?;
	}
	Ok(())
}

fn fragment_cycle<'r, 'a>(
	f: &'r FragmentDefinition<'a, String>,
	fragments: &'r Fragments<'a>,
	visiting: &mut Vec<&'r str>,
	depth: u32,
	seen: &mut u64,
) -> Result<()> {
	if depth > FRAGMENT_STACK_LIMIT {
		return Err(Error::new(format!(
			"Fragment cycle depth is greater than {FRAGMENT_STACK_LIMIT}"
		)));
	}
	if visiting.contains(&f.name.as_str()) {
		return Err(Error::new("Found a cycle between fragments"));
	}
	visiting.push(&f.name);
	set_cycle(&f.selection_set, fragments, visiting, depth + 1, seen)?;
	visiting.pop();
	Ok(())
}

fn set_cycle<'r, 'a>(
	set: &'r SelectionSet<'a, String>,
	fragments: &'r Fragments<'a>,
	visiting: &mut Vec<&'r str>,
	depth: u32,
	seen: &mut u64,
) -> Result<()> {
	if depth > FRAGMENT_STACK_LIMIT {
		return Err(Error::new(format!(
			"Fragment cycle depth is greater than {FRAGMENT_STACK_LIMIT}"
		)));
	}
	for selection in &set.items {
		*seen += 1;
		if *seen > MAX_EXPANDED {
			return Err(Error::new(format!(
				"The document expands to more than {MAX_EXPANDED} selections once its fragments are spread"
			)));
		}
		match selection {
			Selection::Field(f) => {
				set_cycle(&f.selection_set, fragments, visiting, depth + 1, seen)?
			}
			Selection::FragmentSpread(s) => {
				if let Some(d) = fragments.iter().find(|d| d.name == s.fragment_name) {
					fragment_cycle(d, fragments, visiting, depth + 1, seen)?;
				}
			}
			Selection::InlineFragment(i) => {
				set_cycle(&i.selection_set, fragments, visiting, depth + 1, seen)?
			}
		}
	}
	Ok(())
}
