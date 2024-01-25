use tokio::io::AsyncBufReadExt;
use std::collections::HashMap;
use serde::{Serialize, Deserialize};

#[derive(Serialize, Deserialize)]
pub struct MimeDb(HashMap<String, String>);

#[derive(Debug)]
pub enum MimeDbError {
	Malformed,
}

impl MimeDb {
	pub async fn new<T: AsyncBufReadExt + Unpin> (reader: &mut T) -> Result<Self, MimeDbError> {
		let mut db = HashMap::new();
		let mut lines = reader.lines();
		while let Ok(string) = lines.next_line().await {
			let Some(string) = string else { break; };
			if string.starts_with("#") || string.is_empty() {
				continue;
			}
			let mut components = string.split_ascii_whitespace();
			let mime = components.next().ok_or(MimeDbError::Malformed)?;

			for kind in components {
				db.insert(kind.to_string(), mime.to_string());
			}
		}
		Ok(Self(db))
	}
	pub fn get(&self, file: &str) -> Option<&str> {
		let (_, file) = file.rsplit_once('.')?;
		self.0.get(file).map(|x| x.as_str())
	}
}
