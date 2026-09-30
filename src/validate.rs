//! The specification's validation rules, on unless a schema's directive turns them off:
//! `{"validation": {"enabled": false}}` (DIVERGENCES.md D13). Upstream runs a few of them (#485) and answers documents
//! the specification says are invalid: a variable used where its type does not fit, an unknown
//! field inside a skipped selection, an argument of the wrong type in a field that is never
//! reached, an unused fragment. graphql-js's `validate()` is the reference (`diff/fuzz`), and the
//! sentences are its own, without the "Did you mean" suggestions.
//!
//! Every error is reported, as graphql-js reports them, before anything runs. The request's
//! variables are then coerced against their declared types, as graphql-js's execution does.
use crate::schema::{Kind, Schema, TypeId, TypeRef};
use graphql_parser::query::{
	Definition, Directive, Document, Field, FragmentDefinition, OperationDefinition, Selection,
	SelectionSet, Type, TypeCondition, Value, VariableDefinition,
};
use serde_json::Value as Json;
use std::collections::{HashMap, HashSet};

type Doc<'a> = Document<'a, String>;
type Fragment<'a> = FragmentDefinition<'a, String>;

/// A type as validation compares them: a name under list and non-null wrappers.
#[derive(Clone, Debug, PartialEq, Eq)]
enum VType {
	Named(String),
	List(Box<VType>),
	NonNull(Box<VType>),
}

impl std::fmt::Display for VType {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			VType::Named(n) => write!(f, "{n}"),
			VType::List(t) => write!(f, "[{t}]"),
			VType::NonNull(t) => write!(f, "{t}!"),
		}
	}
}

impl VType {
	fn of_doc(t: &Type<'_, String>) -> VType {
		match t {
			Type::NamedType(n) => VType::Named(n.clone()),
			Type::ListType(t) => VType::List(Box::new(VType::of_doc(t))),
			Type::NonNullType(t) => VType::NonNull(Box::new(VType::of_doc(t))),
		}
	}

	fn named(&self) -> &str {
		match self {
			VType::Named(n) => n,
			VType::List(t) | VType::NonNull(t) => t.named(),
		}
	}
}

fn of_schema(schema: &Schema, t: &TypeRef) -> VType {
	match t {
		TypeRef::Named { id, .. } => VType::Named(schema.ty(*id).name.clone()),
		TypeRef::List(t) => VType::List(Box::new(of_schema(schema, t))),
		TypeRef::NonNull(t) => VType::NonNull(Box::new(of_schema(schema, t))),
	}
}

/// A variable used somewhere: the type expected there, and whether that place has a default.
struct Usage {
	name: String,
	expected: VType,
	has_default: bool,
}

struct V<'s, 'd, 'a> {
	schema: &'s Schema,
	fragments: HashMap<&'d str, &'d Fragment<'a>>,
	errors: Vec<String>,
}

pub fn validate<'d, 'a>(
	schema: &Schema,
	document: &'d Doc<'a>,
	operation: &OperationDefinition<'_, String>,
	variables: &serde_json::Map<String, Json>,
) -> Vec<String> {
	let mut v = V {
		schema,
		fragments: HashMap::new(),
		errors: vec![],
	};
	let mut fragment_names: HashSet<&str> = HashSet::new();
	for d in &document.definitions {
		if let Definition::Fragment(f) = d {
			if !fragment_names.insert(&f.name) {
				v.errors.push(format!(
					"There can be only one fragment named \"{}\".",
					f.name
				));
			}
			v.fragments.entry(&f.name).or_insert(f);
		}
	}

	// Each fragment's body, once, on its own type.
	let mut fragment_usages: HashMap<&str, Vec<Usage>> = HashMap::new();
	let mut fragment_spreads: HashMap<&str, Vec<String>> = HashMap::new();
	for d in &document.definitions {
		let Definition::Fragment(f) = d else { continue };
		let TypeCondition::On(on) = &f.type_condition;
		let mut usages = vec![];
		let mut spreads = vec![];
		match v.composite(on) {
			Err(e) => v
				.errors
				.push(e.replace("{name}", &format!("Fragment \"{}\"", f.name))),
			Ok(ty) => {
				v.directives(&f.directives, "FRAGMENT_DEFINITION", &mut usages);
				v.selections(&f.selection_set, ty, &mut usages, &mut spreads);
			}
		}
		fragment_usages.insert(&f.name, usages);
		fragment_spreads.insert(&f.name, spreads);
	}

	let mut used_fragments: HashSet<String> = HashSet::new();
	for d in &document.definitions {
		let Definition::Operation(op) = d else {
			continue;
		};
		let (name, defs, directives, set, root, location) = match op {
			OperationDefinition::Query(q) => (
				q.name.as_deref(),
				&q.variable_definitions[..],
				&q.directives[..],
				&q.selection_set,
				Some(schema.query),
				"QUERY",
			),
			OperationDefinition::SelectionSet(s) => {
				(None, &[][..], &[][..], s, Some(schema.query), "QUERY")
			}
			OperationDefinition::Mutation(m) => (
				m.name.as_deref(),
				&m.variable_definitions[..],
				&m.directives[..],
				&m.selection_set,
				schema.mutation_type(),
				"MUTATION",
			),
			OperationDefinition::Subscription(s) => (
				s.name.as_deref(),
				&s.variable_definitions[..],
				&s.directives[..],
				&s.selection_set,
				None,
				"SUBSCRIPTION",
			),
		};
		let Some(root) = root else {
			v.errors.push(format!(
				"Schema is not configured to execute {} operation.",
				location.to_lowercase()
			));
			continue;
		};
		let mut usages = vec![];
		let mut spreads = vec![];
		v.variable_definitions(defs);
		v.directives(directives, location, &mut usages);
		v.selections(set, root, &mut usages, &mut spreads);
		// Every fragment this operation reaches, through fragments.
		let mut reached: Vec<String> = vec![];
		let mut queue = spreads;
		while let Some(f) = queue.pop() {
			if reached.contains(&f) {
				continue;
			}
			if let Some(more) = fragment_spreads.get(f.as_str()) {
				queue.extend(more.iter().cloned());
			}
			reached.push(f);
		}
		for f in &reached {
			if let Some(u) = fragment_usages.get(f.as_str()) {
				usages.extend(u.iter().map(|u| Usage {
					name: u.name.clone(),
					expected: u.expected.clone(),
					has_default: u.has_default,
				}));
			}
		}
		used_fragments.extend(reached);
		v.variable_usages(name, defs, &usages);
	}
	for d in &document.definitions {
		if let Definition::Fragment(f) = d
			&& !used_fragments.contains(&f.name)
		{
			v.errors
				.push(format!("Fragment \"{}\" is never used.", f.name));
		}
	}
	if v.errors.is_empty() {
		let defs = match operation {
			OperationDefinition::Query(q) => &q.variable_definitions[..],
			OperationDefinition::Mutation(m) => &m.variable_definitions[..],
			OperationDefinition::Subscription(s) => &s.variable_definitions[..],
			OperationDefinition::SelectionSet(_) => &[][..],
		};
		v.coerce_variables(defs, variables);
	}
	v.errors
}

impl<'s, 'd, 'a> V<'s, 'd, 'a> {
	fn kind(&self, name: &str) -> Option<(TypeId, Kind)> {
		let id = self.schema.lookup(name)?;
		Some((id, self.schema.ty(id).kind))
	}

	/// A type a selection can be made on. `{name}` in the error stands for the fragment.
	fn composite(&self, name: &str) -> Result<TypeId, String> {
		match self.kind(name) {
			None => Err(format!("Unknown type \"{name}\".")),
			Some((id, Kind::Object | Kind::Interface)) => Ok(id),
			Some(_) => Err(format!(
				"{{name}} cannot condition on non composite type \"{name}\"."
			)),
		}
	}

	fn is_input(&self, t: &VType) -> Option<bool> {
		self.kind(t.named())
			.map(|(_, k)| matches!(k, Kind::Scalar | Kind::Enum | Kind::InputObject))
	}

	fn variable_definitions(&mut self, defs: &[VariableDefinition<'_, String>]) {
		let mut seen: HashSet<&str> = HashSet::new();
		for d in defs {
			if !seen.insert(&d.name) {
				self.errors.push(format!(
					"There can be only one variable named \"${}\".",
					d.name
				));
			}
			let t = VType::of_doc(&d.var_type);
			match self.is_input(&t) {
				None => self.errors.push(format!("Unknown type \"{}\".", t.named())),
				Some(false) => self.errors.push(format!(
					"Variable \"${}\" cannot be non-input type \"{t}\".",
					d.name
				)),
				Some(true) => {
					if let Some(default) = &d.default_value {
						let mut ignored = vec![];
						self.value(default, &t, &mut ignored);
					}
				}
			}
		}
	}

	fn variable_usages(
		&mut self,
		operation: Option<&str>,
		defs: &[VariableDefinition<'_, String>],
		usages: &[Usage],
	) {
		for u in usages {
			let Some(def) = defs.iter().find(|d| d.name == u.name) else {
				self.errors.push(match operation {
					Some(op) => format!(
						"Variable \"${}\" is not defined by operation \"{op}\".",
						u.name
					),
					None => format!("Variable \"${}\" is not defined.", u.name),
				});
				continue;
			};
			let declared = VType::of_doc(&def.var_type);
			if !allowed(
				self,
				&declared,
				def.default_value.as_ref(),
				&u.expected,
				u.has_default,
			) {
				self.errors.push(format!(
					"Variable \"${}\" of type \"{declared}\" used in position expecting type \"{}\".",
					u.name, u.expected
				));
			}
		}
		for d in defs {
			if !usages.iter().any(|u| u.name == d.name) {
				self.errors.push(match operation {
					Some(op) => format!(
						"Variable \"${}\" is never used in operation \"{op}\".",
						d.name
					),
					None => format!("Variable \"${}\" is never used.", d.name),
				});
			}
		}
	}

	fn directives(
		&mut self,
		directives: &[Directive<'_, String>],
		location: &str,
		usages: &mut Vec<Usage>,
	) {
		let mut seen: HashSet<&str> = HashSet::new();
		for d in directives {
			let allowed_here = matches!(location, "FIELD" | "FRAGMENT_SPREAD" | "INLINE_FRAGMENT");
			if !matches!(d.name.as_str(), "skip" | "include") {
				self.errors
					.push(format!("Unknown directive \"@{}\".", d.name));
				continue;
			}
			if !allowed_here {
				self.errors.push(format!(
					"Directive \"@{}\" may not be used on {location}.",
					d.name
				));
			}
			if !seen.insert(&d.name) {
				self.errors.push(format!(
					"The directive \"@{}\" can only be used once at this location.",
					d.name
				));
			}
			let boolean = VType::NonNull(Box::new(VType::Named("Boolean".into())));
			let mut names: HashSet<&str> = HashSet::new();
			for (name, value) in &d.arguments {
				if !names.insert(name) {
					self.errors
						.push(format!("There can be only one argument named \"{name}\"."));
				}
				if name != "if" {
					self.errors.push(format!(
						"Unknown argument \"{name}\" on directive \"@{}\".",
						d.name
					));
					continue;
				}
				self.value(value, &boolean, usages);
			}
			if !d.arguments.iter().any(|(n, _)| n == "if") {
				self.errors.push(format!(
					"Directive \"@{}\" argument \"if\" of type \"Boolean!\" is required, but it was not provided.",
					d.name
				));
			}
		}
	}

	fn selections(
		&mut self,
		set: &'d SelectionSet<'a, String>,
		parent: TypeId,
		usages: &mut Vec<Usage>,
		spreads: &mut Vec<String>,
	) {
		let mut seen: Vec<(String, &'d Field<'a, String>, TypeId)> = vec![];
		self.collect(set, parent, &mut seen, usages, spreads);
		self.overlaps(&seen);
	}

	/// Validate each selection, and gather the fields under each response key for the merge rule.
	fn collect(
		&mut self,
		set: &'d SelectionSet<'a, String>,
		parent: TypeId,
		seen: &mut Vec<(String, &'d Field<'a, String>, TypeId)>,
		usages: &mut Vec<Usage>,
		spreads: &mut Vec<String>,
	) {
		for s in &set.items {
			match s {
				Selection::Field(f) => {
					self.field(f, parent, usages, spreads);
					seen.push((f.alias.clone().unwrap_or_else(|| f.name.clone()), f, parent));
				}
				Selection::InlineFragment(i) => {
					self.directives(&i.directives, "INLINE_FRAGMENT", usages);
					let ty = match &i.type_condition {
						None => parent,
						Some(TypeCondition::On(on)) => match self.composite(on) {
							Err(e) => {
								self.errors.push(e.replace("{name}", "Fragment"));
								continue;
							}
							Ok(ty) => {
								self.possible(None, ty, parent);
								ty
							}
						},
					};
					self.collect(&i.selection_set, ty, seen, usages, spreads);
				}
				Selection::FragmentSpread(sp) => {
					self.directives(&sp.directives, "FRAGMENT_SPREAD", usages);
					let Some(frag) = self.fragments.get(sp.fragment_name.as_str()).copied() else {
						self.errors
							.push(format!("Unknown fragment \"{}\".", sp.fragment_name));
						continue;
					};
					spreads.push(sp.fragment_name.clone());
					let TypeCondition::On(on) = &frag.type_condition;
					if let Some((ty, Kind::Object | Kind::Interface)) = self.kind(on) {
						self.possible(Some(&sp.fragment_name), ty, parent);
						// The merge rule looks through the fragment; its own rules ran on its own.
						self.collect_only(&frag.selection_set, ty, seen, 0);
					}
				}
			}
		}
	}

	/// The fields of a fragment for the merge rule, without validating them again.
	fn collect_only(
		&self,
		set: &'d SelectionSet<'a, String>,
		parent: TypeId,
		seen: &mut Vec<(String, &'d Field<'a, String>, TypeId)>,
		depth: u32,
	) {
		if depth > 50 {
			return;
		}
		for s in &set.items {
			match s {
				Selection::Field(f) => {
					seen.push((f.alias.clone().unwrap_or_else(|| f.name.clone()), f, parent));
				}
				Selection::InlineFragment(i) => {
					let ty = match &i.type_condition {
						Some(TypeCondition::On(on)) => {
							self.kind(on).map(|(t, _)| t).unwrap_or(parent)
						}
						None => parent,
					};
					self.collect_only(&i.selection_set, ty, seen, depth + 1);
				}
				Selection::FragmentSpread(sp) => {
					if let Some(frag) = self.fragments.get(sp.fragment_name.as_str()).copied() {
						let TypeCondition::On(on) = &frag.type_condition;
						let ty = self.kind(on).map(|(t, _)| t).unwrap_or(parent);
						self.collect_only(&frag.selection_set, ty, seen, depth + 1);
					}
				}
			}
		}
	}

	/// Whether objects of `fragment` can be objects of `parent` (the Node interface and the types
	/// that implement it are the only abstract relation this schema has).
	fn possible(&mut self, name: Option<&str>, fragment: TypeId, parent: TypeId) {
		if fragment == parent {
			return;
		}
		let overlap = |a: TypeId, b: TypeId| {
			a == self.schema.node_interface && self.schema.node_types().contains(&b)
		};
		if overlap(fragment, parent) || overlap(parent, fragment) {
			return;
		}
		let (f, p) = (&self.schema.ty(fragment).name, &self.schema.ty(parent).name);
		self.errors.push(match name {
			Some(n) => format!(
				"Fragment \"{n}\" cannot be spread here as objects of type \"{p}\" can never be of type \"{f}\"."
			),
			None => format!(
				"Fragment cannot be spread here as objects of type \"{p}\" can never be of type \"{f}\"."
			),
		});
	}

	fn field(
		&mut self,
		f: &'d Field<'a, String>,
		parent: TypeId,
		usages: &mut Vec<Usage>,
		spreads: &mut Vec<String>,
	) {
		self.directives(&f.directives, "FIELD", usages);
		let parent_name = self.schema.ty(parent).name.clone();
		if f.name == "__typename" {
			self.leaf(f, "String!");
			return;
		}
		let Some(def) = self.schema.field(parent, &f.name) else {
			self.errors.push(format!(
				"Cannot query field \"{}\" on type \"{parent_name}\".",
				f.name
			));
			return;
		};
		// Arguments: each known, once, of its type; the required ones given.
		let mut names: HashSet<&str> = HashSet::new();
		for (name, value) in &f.arguments {
			if !names.insert(name) {
				self.errors
					.push(format!("There can be only one argument named \"{name}\"."));
			}
			match def.args.iter().find(|a| &a.name == name) {
				None => self.errors.push(format!(
					"Unknown argument \"{name}\" on field \"{parent_name}.{}\".",
					f.name
				)),
				Some(a) => {
					let t = of_schema(self.schema, &a.ty);
					self.value_at(value, &t, a.default_value.is_some(), usages);
				}
			}
		}
		for a in &def.args {
			if matches!(a.ty, TypeRef::NonNull(_))
				&& a.default_value.is_none()
				&& !f.arguments.iter().any(|(n, _)| n == &a.name)
			{
				self.errors.push(format!(
					"Field \"{parent_name}.{}\" argument \"{}\" of type \"{}\" is required, but it was not provided.",
					f.name,
					a.name,
					of_schema(self.schema, &a.ty)
				));
			}
		}
		let inner = def.ty.base();
		let type_str = of_schema(self.schema, &def.ty).to_string();
		match self.schema.ty(inner).kind {
			Kind::Object | Kind::Interface => {
				if f.selection_set.items.is_empty() {
					self.errors.push(format!(
						"Field \"{}\" of type \"{type_str}\" must have a selection of subfields. Did you mean \"{} {{ ... }}\"?",
						f.name, f.name
					));
				} else {
					self.selections(&f.selection_set, inner, usages, spreads);
				}
			}
			_ => self.leaf(f, &type_str),
		}
	}

	fn leaf(&mut self, f: &Field<'_, String>, type_str: &str) {
		if !f.selection_set.items.is_empty() {
			self.errors.push(format!(
				"Field \"{}\" must not have a selection since type \"{type_str}\" has no subfields.",
				f.name
			));
		}
	}

	/// Fields under one response key must be the same field with the same arguments, and their
	/// selections must merge (`OverlappingFieldsCanBeMerged`), for fields on the same type.
	fn overlaps(&mut self, seen: &[(String, &'d Field<'a, String>, TypeId)]) {
		let mut reported: HashSet<&str> = HashSet::new();
		for (i, (key, a, pa)) in seen.iter().enumerate() {
			for (key_b, b, pb) in &seen[i + 1..] {
				if key != key_b || reported.contains(key.as_str()) {
					continue;
				}
				if let Some(reason) = self.conflict(a, *pa, b, *pb, 0) {
					reported.insert(key);
					self.errors.push(format!(
						"Fields \"{key}\" conflict because {reason}. Use different aliases on the fields to fetch both if this was intentional."
					));
				}
			}
		}
	}

	fn conflict(
		&mut self,
		a: &'d Field<'a, String>,
		pa: TypeId,
		b: &'d Field<'a, String>,
		pb: TypeId,
		depth: u32,
	) -> Option<String> {
		if depth > 32 {
			return None;
		}
		// Two different object types can give the same key different fields; only a shared
		// parent (or an abstract one) must agree.
		let same_parent = pa == pb
			|| !matches!(self.schema.ty(pa).kind, Kind::Object)
			|| !matches!(self.schema.ty(pb).kind, Kind::Object);
		let ta = self.field_type(pa, &a.name);
		let tb = self.field_type(pb, &b.name);
		if same_parent {
			if a.name != b.name {
				return Some(format!(
					"\"{}\" and \"{}\" are different fields",
					a.name, b.name
				));
			}
			if !same_arguments(&a.arguments, &b.arguments) {
				return Some("they have differing arguments".into());
			}
		}
		if let (Some(ta), Some(tb)) = (&ta, &tb)
			&& ta != tb
			&& (is_leaf_or_wrapped(self, ta) || is_leaf_or_wrapped(self, tb))
		{
			return Some(format!(
				"they return conflicting types \"{ta}\" and \"{tb}\""
			));
		}
		let (Some(ta), Some(tb)) = (ta, tb) else {
			return None;
		};
		let (Some(ia), Some(ib)) = (
			self.schema.lookup(ta.named()),
			self.schema.lookup(tb.named()),
		) else {
			return None;
		};
		let mut fa = vec![];
		let mut fb = vec![];
		self.collect_only(&a.selection_set, ia, &mut fa, 0);
		self.collect_only(&b.selection_set, ib, &mut fb, 0);
		for (ka, x, px) in &fa {
			for (kb, y, py) in &fb {
				if ka == kb
					&& let Some(reason) = self.conflict(x, *px, y, *py, depth + 1)
				{
					return Some(format!("subfields \"{ka}\" conflict because {reason}"));
				}
			}
		}
		None
	}

	fn field_type(&self, parent: TypeId, name: &str) -> Option<VType> {
		if name == "__typename" {
			return Some(VType::NonNull(Box::new(VType::Named("String".into()))));
		}
		self.schema
			.field(parent, name)
			.map(|d| of_schema(self.schema, &d.ty))
	}

	/// A value where the place it is used has, or has not, a default of its own.
	fn value_at(
		&mut self,
		v: &Value<'_, String>,
		t: &VType,
		has_default: bool,
		usages: &mut Vec<Usage>,
	) {
		if let Value::Variable(name) = v {
			usages.push(Usage {
				name: name.clone(),
				expected: t.clone(),
				has_default,
			});
			return;
		}
		self.value(v, t, usages);
	}

	/// A literal checked against its type (`ValuesOfCorrectType`); variables inside are recorded.
	fn value(&mut self, v: &Value<'_, String>, t: &VType, usages: &mut Vec<Usage>) {
		match (v, t) {
			(Value::Variable(name), _) => usages.push(Usage {
				name: name.clone(),
				expected: t.clone(),
				has_default: false,
			}),
			(Value::Null, VType::NonNull(_)) => {
				self.errors
					.push(format!("Expected value of type \"{t}\", found null."));
			}
			(_, VType::NonNull(inner)) => self.value(v, inner, usages),
			(Value::Null, _) => {}
			(Value::List(items), VType::List(inner)) => {
				for i in items {
					self.value(i, inner, usages);
				}
			}
			(_, VType::List(inner)) => self.value(v, inner, usages),
			(_, VType::Named(name)) => self.named_value(v, name, t, usages),
		}
	}

	fn named_value(
		&mut self,
		v: &Value<'_, String>,
		name: &str,
		t: &VType,
		usages: &mut Vec<Usage>,
	) {
		let Some((id, kind)) = self.kind(name) else {
			return;
		};
		let shown = print(v);
		match kind {
			Kind::InputObject => {
				let Value::Object(fields) = v else {
					self.errors
						.push(format!("Expected value of type \"{t}\", found {shown}."));
					return;
				};
				let defs = self.schema.inputs(id).unwrap_or(&[]).to_vec();
				for (k, fv) in fields {
					match defs.iter().find(|d| &d.name == k) {
						None => self
							.errors
							.push(format!("Field \"{k}\" is not defined by type \"{name}\".")),
						Some(d) => {
							let ft = of_schema(self.schema, &d.ty);
							self.value(fv, &ft, usages);
						}
					}
				}
				for d in &defs {
					if matches!(d.ty, TypeRef::NonNull(_))
						&& d.default_value.is_none()
						&& !fields.contains_key(&d.name)
					{
						self.errors.push(format!(
							"Field \"{name}.{}\" of required type \"{}\" was not provided.",
							d.name,
							of_schema(self.schema, &d.ty)
						));
					}
				}
			}
			Kind::Enum => {
				let values = self.schema.enum_values(id).unwrap_or_default();
				match v {
					Value::Enum(e) if values.iter().any(|(x, _)| x == e) => {}
					Value::Enum(e) => {
						self.errors
							.push(format!("Value \"{e}\" does not exist in \"{name}\" enum."));
					}
					_ => self.errors.push(format!(
						"Enum \"{name}\" cannot represent non-enum value: {shown}."
					)),
				}
			}
			Kind::Scalar => {
				if let Some(e) = scalar_literal(name, v) {
					self.errors.push(e);
				}
			}
			Kind::Object | Kind::Interface => {}
		}
	}

	/// The request's variables coerced against their declared types, as graphql-js's execution
	/// does before it runs anything.
	fn coerce_variables(
		&mut self,
		defs: &[VariableDefinition<'_, String>],
		variables: &serde_json::Map<String, Json>,
	) {
		for d in defs {
			let t = VType::of_doc(&d.var_type);
			match variables.get(&d.name) {
				None => {
					if matches!(t, VType::NonNull(_)) && d.default_value.is_none() {
						self.errors.push(format!(
							"Variable \"${}\" of required type \"{t}\" was not provided.",
							d.name
						));
					}
				}
				Some(Json::Null) if matches!(t, VType::NonNull(_)) => {
					self.errors.push(format!(
						"Variable \"${}\" of non-null type \"{t}\" must not be null.",
						d.name
					));
				}
				Some(value) => {
					if let Some(e) = self.json_value(value, &t, "") {
						self.errors.push(format!(
							"Variable \"${}\" got invalid value {}; {e}",
							d.name,
							inspect(value)
						));
					}
				}
			}
		}
	}

	/// Why a variable's JSON value does not fit its type, if it does not.
	fn json_value(&self, v: &Json, t: &VType, at: &str) -> Option<String> {
		match (v, t) {
			(Json::Null, VType::NonNull(_)) => Some(format!(
				"Expected non-nullable type \"{t}\" not to be null{at}."
			)),
			(_, VType::NonNull(inner)) => self.json_value(v, inner, at),
			(Json::Null, _) => None,
			(Json::Array(items), VType::List(inner)) => items
				.iter()
				.enumerate()
				.find_map(|(i, x)| self.json_value(x, inner, &format!("{at}[{i}]"))),
			(_, VType::List(inner)) => self.json_value(v, inner, at),
			(_, VType::Named(name)) => {
				let (id, kind) = self.kind(name)?;
				match kind {
					Kind::InputObject => {
						let Json::Object(fields) = v else {
							return Some(format!("Expected type \"{name}\" to be an object{at}."));
						};
						let defs = self.schema.inputs(id).unwrap_or(&[]);
						for (k, fv) in fields {
							match defs.iter().find(|d| &d.name == k) {
								None => {
									return Some(format!(
										"Field \"{k}\" is not defined by type \"{name}\"{at}."
									));
								}
								Some(d) => {
									let ft = of_schema(self.schema, &d.ty);
									if let Some(e) = self.json_value(fv, &ft, &format!("{at}.{k}"))
									{
										return Some(e);
									}
								}
							}
						}
						defs.iter()
							.find(|d| {
								matches!(d.ty, TypeRef::NonNull(_))
									&& d.default_value.is_none()
									&& !fields.contains_key(&d.name)
							})
							.map(|d| {
								format!(
									"Field \"{}\" of required type \"{}\" was not provided{at}.",
									d.name,
									of_schema(self.schema, &d.ty)
								)
							})
					}
					Kind::Enum => {
						let values = self.schema.enum_values(id).unwrap_or_default();
						match v {
							Json::String(s) if values.iter().any(|(x, _)| x == s) => None,
							Json::String(s) => Some(format!(
								"Value \"{s}\" does not exist in \"{name}\" enum{at}."
							)),
							other => Some(format!(
								"Enum \"{name}\" cannot represent non-string value: {}{at}.",
								inspect(other)
							)),
						}
					}
					Kind::Scalar => scalar_json(name, v),
					_ => None,
				}
			}
		}
	}
}

/// graphql-js's `isTypeSubTypeOf`, with the allowance a default makes for a nullable variable.
fn allowed(
	v: &V<'_, '_, '_>,
	var: &VType,
	var_default: Option<&Value<'_, String>>,
	location: &VType,
	location_default: bool,
) -> bool {
	if let VType::NonNull(inner) = location
		&& !matches!(var, VType::NonNull(_))
	{
		let non_null_default = var_default.is_some_and(|d| !matches!(d, Value::Null));
		if !non_null_default && !location_default {
			return false;
		}
		return subtype(v, var, inner);
	}
	subtype(v, var, location)
}

fn subtype(v: &V<'_, '_, '_>, sub: &VType, sup: &VType) -> bool {
	if sub == sup {
		return true;
	}
	match (sub, sup) {
		(VType::NonNull(a), VType::NonNull(b)) => subtype(v, a, b),
		(_, VType::NonNull(_)) => false,
		(VType::NonNull(a), _) => subtype(v, a, sup),
		(VType::List(a), VType::List(b)) => subtype(v, a, b),
		(_, VType::List(_)) | (VType::List(_), _) => false,
		(VType::Named(a), VType::Named(b)) => {
			a == b
				|| (v.schema.lookup(b) == Some(v.schema.node_interface)
					&& v.schema
						.lookup(a)
						.is_some_and(|id| v.schema.node_types().contains(&id)))
		}
	}
}

fn is_leaf_or_wrapped(v: &V<'_, '_, '_>, t: &VType) -> bool {
	!matches!(t, VType::Named(_))
		|| v.kind(t.named())
			.is_some_and(|(_, k)| matches!(k, Kind::Scalar | Kind::Enum))
}

fn same_arguments(a: &[(String, Value<'_, String>)], b: &[(String, Value<'_, String>)]) -> bool {
	a.len() == b.len()
		&& a.iter()
			.all(|(n, v)| b.iter().any(|(m, w)| n == m && print(v) == print(w)))
}

/// A literal of one of the specification's scalars that graphql-js would not read. Every other
/// scalar is custom, and graphql-js's reading of a custom scalar takes any literal.
fn scalar_literal(name: &str, v: &Value<'_, String>) -> Option<String> {
	let shown = print(v);
	match name {
		"Int" => match v {
			Value::Int(n) => match n.as_i64() {
				Some(x) if i32::try_from(x).is_ok() => None,
				_ => Some(format!(
					"Int cannot represent non 32-bit signed integer value: {shown}"
				)),
			},
			_ => Some(format!("Int cannot represent non-integer value: {shown}")),
		},
		"Float" => match v {
			Value::Int(_) | Value::Float(_) => None,
			_ => Some(format!("Float cannot represent non numeric value: {shown}")),
		},
		"String" => match v {
			Value::String(_) => None,
			_ => Some(format!(
				"String cannot represent a non string value: {shown}"
			)),
		},
		"Boolean" => match v {
			Value::Boolean(_) => None,
			_ => Some(format!(
				"Boolean cannot represent a non boolean value: {shown}"
			)),
		},
		"ID" => match v {
			Value::String(_) | Value::Int(_) => None,
			_ => Some(format!(
				"ID cannot represent a non-string and non-integer value: {shown}"
			)),
		},
		_ => None,
	}
}

fn scalar_json(name: &str, v: &Json) -> Option<String> {
	let shown = inspect(v);
	match name {
		"Int" => match v.as_f64() {
			Some(f)
				if f.fract() == 0.0 && (f64::from(i32::MIN)..=f64::from(i32::MAX)).contains(&f) =>
			{
				None
			}
			Some(f) if f.fract() == 0.0 => Some(format!(
				"Int cannot represent non 32-bit signed integer value: {shown}"
			)),
			_ => Some(format!("Int cannot represent non-integer value: {shown}")),
		},
		"Float" => match v {
			Json::Number(_) => None,
			_ => Some(format!("Float cannot represent non numeric value: {shown}")),
		},
		"String" => match v {
			Json::String(_) => None,
			_ => Some(format!(
				"String cannot represent a non string value: {shown}"
			)),
		},
		"Boolean" => match v {
			Json::Bool(_) => None,
			_ => Some(format!(
				"Boolean cannot represent a non boolean value: {shown}"
			)),
		},
		"ID" => match v {
			Json::String(_) => None,
			Json::Number(n) if n.is_i64() => None,
			_ => Some(format!("ID cannot represent value: {shown}")),
		},
		_ => None,
	}
}

/// A literal as graphql-js prints it in a message.
fn print(v: &Value<'_, String>) -> String {
	match v {
		Value::Variable(n) => format!("${n}"),
		Value::Int(n) => n.as_i64().map(|x| x.to_string()).unwrap_or_default(),
		Value::Float(f) => f.to_string(),
		Value::String(s) => Json::String(s.clone()).to_string(),
		Value::Boolean(b) => b.to_string(),
		Value::Null => "null".into(),
		Value::Enum(e) => e.clone(),
		Value::List(items) => format!(
			"[{}]",
			items.iter().map(print).collect::<Vec<_>>().join(", ")
		),
		Value::Object(fields) => format!(
			"{{ {} }}",
			fields
				.iter()
				.map(|(k, v)| format!("{k}: {}", print(v)))
				.collect::<Vec<_>>()
				.join(", ")
		),
	}
}

/// A JSON value as graphql-js's `inspect` shows it.
fn inspect(v: &Json) -> String {
	match v {
		Json::Array(items) => format!(
			"[{}]",
			items.iter().map(inspect).collect::<Vec<_>>().join(", ")
		),
		Json::Object(m) => {
			if m.is_empty() {
				return "{}".into();
			}
			let parts: Vec<String> = m
				.iter()
				.map(|(k, v)| format!("{k}: {}", inspect(v)))
				.collect();
			format!("{{ {} }}", parts.join(", "))
		}
		other => other.to_string(),
	}
}
