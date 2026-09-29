//! The reflected schema, kept per connection so a request does not read the catalogue.
//!
//! One schema is kept per (role, search path, role memberships): a new schema version replaces
//! the old one rather than sitting beside it, so a connection's memory does not grow with every
//! migration it lives through. When the version moves under the trigger older installs have, which
//! moves it for every DDL statement, a fingerprint of the rows the schema is read from decides
//! whether anything it shows changed: DDL elsewhere (a temporary table, a partition in another
//! schema) keeps the schema. And a schema read inside a transaction that has written something
//! remembers that transaction: it is trusted again only once that transaction has committed, so a
//! migration that rolls back takes the schema it showed with it.
#![allow(unsafe_code)]

use crate::catalog::{self, Key, LoadError};
use crate::pg;
use crate::schema::Schema;
use pgrx::pg_sys;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// Distinct roles and search paths a connection keeps a schema for. A pooled connection serves a
/// handful (anon, authenticated, service_role); this bounds the rest.
const MAX_ENTRIES: usize = 8;

type Identity = (Vec<u32>, String, Vec<i64>);

struct Entry {
	version: i32,
	/// The catalogue fingerprint the schema was read at (catalog::fingerprint), once the schema
	/// version has moved at least once: a connection's first read skips it, since most connections
	/// never see a change.
	fingerprint: Option<i64>,
	schema: Rc<Schema>,
	used: u64,
	/// The writing transaction the schema was read in, until it is known to have committed.
	read_in: Option<pg_sys::TransactionId>,
}

#[derive(Default)]
struct Cache {
	entries: HashMap<Identity, Entry>,
	clock: u64,
}

thread_local! {
	static CACHE: RefCell<Cache> = RefCell::new(Cache::default());
}

/// The request's top-level transaction, if it has written anything. The top level, because
/// `graphql.resolve` runs inside a subtransaction of its own (its exception block), which has
/// written nothing even when the transaction around it has.
fn writing_transaction() -> Option<pg_sys::TransactionId> {
	let xid = unsafe { pg_sys::GetTopTransactionIdIfAny() };
	(xid != pg_sys::InvalidTransactionId).then_some(xid)
}

/// Whether a schema read in transaction `xid` may be used now.
fn still_valid(read_in: &mut Option<pg_sys::TransactionId>) -> bool {
	let Some(xid) = *read_in else {
		return true;
	};
	if writing_transaction() == Some(xid) {
		return true;
	}
	// The commit log is only consulted for transactions it still holds; older than that, the
	// schema is simply read again.
	let committed = unsafe {
		let oldest = (*pg_sys::TransamVariables).oldestClogXid;
		!pg_sys::TransactionIdPrecedes(xid, oldest) && pg_sys::TransactionIdDidCommit(xid)
	};
	if committed {
		*read_in = None;
	}
	committed
}

enum Lookup {
	Hit(Rc<Schema>),
	/// Kept, but the schema version has moved since: the fingerprint decides.
	Moved(Option<i64>),
	Miss,
}

pub fn schema() -> Result<Rc<Schema>, LoadError> {
	let key = catalog::key();
	let identity: Identity = (
		key.search_path.clone(),
		key.role.clone(),
		key.memberships.clone(),
	);
	let version = key.schema_version;
	let lookup = CACHE.with(|c| {
		let mut c = c.borrow_mut();
		c.clock += 1;
		let clock = c.clock;
		let Some(entry) = c.entries.get_mut(&identity) else {
			return Lookup::Miss;
		};
		if !still_valid(&mut entry.read_in) {
			return Lookup::Miss;
		}
		if entry.version != version {
			return Lookup::Moved(entry.fingerprint);
		}
		entry.used = clock;
		Lookup::Hit(Rc::clone(&entry.schema))
	});

	// The fingerprint is read BEFORE the catalogue: if a change commits in between, the schema
	// read is newer than the fingerprint kept with it, so the next comparison rebuilds rather than
	// keeping something stale.
	let fingerprint = match lookup {
		Lookup::Hit(s) => return Ok(s),
		Lookup::Moved(_) if catalog::version_is_precise() => None,
		Lookup::Moved(kept) => {
			let now = pg::generic_plans(|| catalog::fingerprint(&key));
			if kept == Some(now) {
				let reused = CACHE.with(|c| {
					let mut c = c.borrow_mut();
					let clock = c.clock;
					c.entries.get_mut(&identity).map(|e| {
						e.version = version;
						e.used = clock;
						Rc::clone(&e.schema)
					})
				});
				if let Some(s) = reused {
					return Ok(s);
				}
			}
			Some(now)
		}
		Lookup::Miss => None,
	};

	let schema = Rc::new(Schema::build(pg::generic_plans(|| {
		catalog::load(Key { ..key })
	})?));
	let read_in = writing_transaction();
	CACHE.with(|c| {
		let mut c = c.borrow_mut();
		if !c.entries.contains_key(&identity)
			&& c.entries.len() >= MAX_ENTRIES
			&& let Some(oldest) = c
				.entries
				.iter()
				.min_by_key(|(_, e)| e.used)
				.map(|(k, _)| k.clone())
		{
			c.entries.remove(&oldest);
		}
		let used = c.clock;
		c.entries.insert(
			identity,
			Entry {
				version,
				fingerprint,
				schema: Rc::clone(&schema),
				used,
				read_in,
			},
		);
	});
	Ok(schema)
}
