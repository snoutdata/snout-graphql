//! An error a client is told about, as one sentence in the response's `errors`.

#[derive(Clone, Debug, PartialEq)]
pub struct Error(pub String);

impl Error {
	pub fn new(message: impl Into<String>) -> Error {
		Error(message.into())
	}
}

impl std::fmt::Display for Error {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str(&self.0)
	}
}

impl From<String> for Error {
	fn from(s: String) -> Error {
		Error(s)
	}
}

impl From<&str> for Error {
	fn from(s: &str) -> Error {
		Error(s.to_string())
	}
}

pub type Result<T> = std::result::Result<T, Error>;
