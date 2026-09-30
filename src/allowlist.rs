//! The operation allowlist, where a schema's directive turns it on:
//! `{"allowlist": {"table": "public.graphql_operations", "roles": ["anon", "authenticated"]}}`.
//!
//! The table is the project's own, with a `hash text` column (the SHA-256 of the document, in hex)
//! and a `document text` column. For the roles named (by default the two a public API key acts as),
//! a request runs only if its document is in the table: the one control that shrinks what a
//! published key can ask. Any role may also send just the hash, as Apollo's persisted queries do
//! (`extensions: {"persistedQuery": {"sha256Hash": "..."}}` with no query), and the document is
//! read from the table. The table is read as the caller, so those roles need SELECT on it.
use crate::catalog::Allowlist;
use crate::pg;
use crate::schema::Schema;
use crate::sql::ident;
use serde_json::Value as Json;

/// The document to run, or the sentence refusing the request. `None` means the request did not
/// say what to run at all.
pub fn document(
	schema: &Schema,
	query: Option<String>,
	extensions: Option<&Json>,
) -> Result<Option<String>, String> {
	let lists: Vec<&Allowlist> = schema
		.catalog
		.schemas
		.values()
		.filter_map(|s| s.allowlist.as_ref())
		.collect();
	if lists.is_empty() {
		return Ok(query);
	}
	let sent_hash = extensions
		.and_then(|e| e.get("persistedQuery"))
		.and_then(|p| p.get("sha256Hash"))
		.and_then(Json::as_str)
		.map(str::to_ascii_lowercase);
	let hash = match (&query, &sent_hash) {
		(Some(q), sent) => {
			let computed = pg::query(
				"select encode(sha256(convert_to($1, 'UTF8')), 'hex')",
				&[pg::Arg::Text(Some(q.clone()))],
			)
			.first()
			.and_then(|r| r.text(0))
			.unwrap_or_default();
			if sent.as_ref().is_some_and(|s| *s != computed) {
				return Err("provided sha does not match query".into());
			}
			computed
		}
		(None, Some(sent)) => sent.clone(),
		(None, None) => return Ok(None),
	};
	let mut found = None;
	for list in &lists {
		let sql = format!(
			"select document from {}.{} where hash = $1 limit 1",
			ident(&list.schema),
			ident(&list.table)
		);
		if let Some(doc) = pg::query(&sql, &[pg::Arg::Text(Some(hash.clone()))])
			.first()
			.map(|r| r.text(0))
		{
			found = Some(doc.unwrap_or_default());
			break;
		}
	}
	let role = pg::query("select current_user::text", &[])
		.first()
		.and_then(|r| r.text(0))
		.unwrap_or_default();
	let restricted = lists.iter().any(|l| l.roles.contains(&role));
	match (query, found) {
		(Some(q), Some(_)) => Ok(Some(q)),
		(Some(_), None) if restricted => {
			Err("This operation is not on the schema's allowlist".into())
		}
		(Some(q), None) => Ok(Some(q)),
		(None, Some(doc)) => Ok(Some(doc)),
		(None, None) => Err("PersistedQueryNotFound".into()),
	}
}
