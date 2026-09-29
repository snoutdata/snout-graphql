//! What a client sends encoded, decoded (node ids, cursors, base64), and numbers in a result as
//! clients have always read them. No Postgres here, so the fuzz targets can run all of it.
use crate::error::{Error, Result};
use crate::value::In;
use serde_json::Value as Json;

#[derive(Clone, Debug)]
pub struct NodeId {
	pub schema: String,
	pub table: String,
	pub values: Vec<Json>,
}

pub fn parse_node_id(value: &In) -> Result<NodeId> {
	let In::Str(encoded) = value else {
		return Err(Error::new(
			"Invalid value passed to nodeId argument, Error 1",
		));
	};
	let bytes = base64_decode(encoded)
		.ok_or_else(|| Error::new("Invalid value passed to nodeId argument. Error 2"))?;
	let text = String::from_utf8(bytes)
		.map_err(|_| Error::new("Invalid value passed to nodeId argument. Error 3"))?;
	let json: Json = serde_json::from_str(&text)
		.map_err(|_| Error::new("Invalid value passed to nodeId argument. Error 4"))?;
	let Json::Array(mut items) = json else {
		return Err(Error::new(
			"Invalid value passed to nodeId argument. Error 10",
		));
	};
	if items.len() < 3 {
		return Err(Error::new(
			"Invalid value passed to nodeId argument. Error 5",
		));
	}
	let values = items.split_off(2);
	let Json::String(schema) = items.remove(0) else {
		return Err(Error::new(
			"Invalid value passed to nodeId argument. Error 6",
		));
	};
	let Json::String(table) = items.remove(0) else {
		return Err(Error::new(
			"Invalid value passed to nodeId argument. Error 7",
		));
	};
	Ok(NodeId {
		schema,
		table,
		values,
	})
}

pub fn decode_cursor(text: &str) -> Result<Vec<Json>> {
	let bytes =
		base64_decode(text).ok_or_else(|| Error::new("Failed to decode cursor, error 1"))?;
	let text =
		String::from_utf8(bytes).map_err(|_| Error::new("Failed to decode cursor, error 2"))?;
	match serde_json::from_str::<Json>(&text) {
		Ok(Json::Array(items)) => Ok(items),
		Ok(_) => Err(Error::new("Failed to decode cursor, error 4")),
		Err(_) => Err(Error::new("Failed to decode cursor, error 3")),
	}
}

/// Standard base64, with or without its padding.
pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
	let trimmed = text.trim_end_matches('=');
	let mut out = Vec::with_capacity(trimmed.len() * 3 / 4);
	let mut buffer: u32 = 0;
	let mut bits = 0;
	for c in trimmed.bytes() {
		let v = match c {
			b'A'..=b'Z' => c - b'A',
			b'a'..=b'z' => c - b'a' + 26,
			b'0'..=b'9' => c - b'0' + 52,
			b'+' => 62,
			b'/' => 63,
			_ => return None,
		};
		buffer = (buffer << 6) | u32::from(v);
		bits += 6;
		if bits >= 8 {
			bits -= 8;
			out.push((buffer >> bits) as u8);
			buffer &= (1 << bits) - 1;
		}
	}
	if trimmed.len() % 4 == 1 {
		return None;
	}
	Some(out)
}

/// Numbers in a result as clients have always received them: an integer exactly, a decimal as
/// the nearest double (so an average reads `2.5`, not `2.5000000000000000`). The database's JSON
/// keeps a numeric's full scale; the response keeps the value a JSON parser would read.
pub fn normalize_numbers(text: &str) -> String {
	let bytes = text.as_bytes();
	let mut out = String::with_capacity(text.len());
	let mut i = 0;
	let mut in_string = false;
	let mut start = 0;
	while i < bytes.len() {
		let c = bytes[i];
		if in_string {
			if c == b'\\' {
				i += 2;
				continue;
			}
			if c == b'"' {
				in_string = false;
			}
			i += 1;
			continue;
		}
		if c == b'"' {
			in_string = true;
			i += 1;
			continue;
		}
		if c == b'-' || c.is_ascii_digit() {
			let token_start = i;
			while i < bytes.len()
				&& matches!(bytes[i], b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E')
			{
				i += 1;
			}
			let token = &text[token_start..i];
			let is_integer = !token.contains(['.', 'e', 'E']);
			let keep = is_integer && (token.parse::<i64>().is_ok() || token.parse::<u64>().is_ok());
			if !keep
				&& let Some(n) = token
					.parse::<f64>()
					.ok()
					.and_then(serde_json::Number::from_f64)
			{
				out.push_str(&text[start..token_start]);
				out.push_str(&n.to_string());
				start = i;
			}
			continue;
		}
		i += 1;
	}
	out.push_str(&text[start..]);
	out
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn base64() {
		assert_eq!(base64_decode("aGVsbG8=").as_deref(), Some(&b"hello"[..]));
		assert_eq!(base64_decode("aGVsbG8").as_deref(), Some(&b"hello"[..]));
		assert_eq!(
			base64_decode("WyJwdWJsaWMiLCAiYWNjb3VudCIsIDFd").as_deref(),
			Some(&b"[\"public\", \"account\", 1]"[..])
		);
		assert!(base64_decode("a").is_none());
		assert!(base64_decode("a\nb").is_none());
	}

	#[test]
	fn numbers() {
		assert_eq!(
			normalize_numbers(r#"{"a": 2.5000000000000000, "b": 10}"#),
			r#"{"a": 2.5, "b": 10}"#
		);
		assert_eq!(
			normalize_numbers(r#"{"a": 2.0000000000000000}"#),
			r#"{"a": 2.0}"#
		);
		assert_eq!(
			normalize_numbers(r#"{"s": "1.50", "n": -19.9900}"#),
			r#"{"s": "1.50", "n": -19.99}"#
		);
		assert_eq!(
			normalize_numbers(r#"[18446744073709551616]"#),
			r#"[1.8446744073709552e+19]"#
		);
		assert_eq!(normalize_numbers(r#"{"e\"x": 1.10}"#), r#"{"e\"x": 1.1}"#);
	}
}
