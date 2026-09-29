//! Answers to introspection, kept per connection. A request whose every root field is `__schema`,
//! `__type` or `__typename` is answered from the schema alone, so the same request against the same
//! schema has the same answer, and tools that introspect again and again (an IDE's schema view, a
//! code generator in watch mode) get it without building it or parsing it into `jsonb` again.
//!
//! Kept as the finished `jsonb`: a few entries, each bounded, the least recently used dropped first
//! (an answer about a schema that has since been replaced is never asked for again, so it goes).
use std::cell::RefCell;
use std::rc::Rc;

/// Entries a connection keeps, and the largest answer kept (a larger one is answered each time).
const MAX_ENTRIES: usize = 4;
const MAX_BYTES: usize = 4 << 20;

/// What an answer depends on: the schema, and the request exactly as sent.
#[derive(Clone, PartialEq, Eq)]
pub struct Key {
	pub schema: u64,
	pub query: String,
	pub variables: String,
	pub operation: Option<String>,
}

thread_local! {
	static ANSWERS: RefCell<Vec<(Key, Rc<Vec<u8>>)>> = const { RefCell::new(Vec::new()) };
}

pub fn get(key: &Key) -> Option<Rc<Vec<u8>>> {
	ANSWERS.with(|a| {
		let mut a = a.borrow_mut();
		let at = a.iter().position(|(k, _)| k == key)?;
		// Most recently used last.
		let entry = a.remove(at);
		let bytes = Rc::clone(&entry.1);
		a.push(entry);
		Some(bytes)
	})
}

pub fn put(key: Key, bytes: Vec<u8>) {
	if bytes.len() > MAX_BYTES {
		return;
	}
	ANSWERS.with(|a| {
		let mut a = a.borrow_mut();
		a.retain(|(k, _)| *k != key);
		if a.len() >= MAX_ENTRIES {
			a.remove(0);
		}
		a.push((key, Rc::new(bytes)));
	});
}
