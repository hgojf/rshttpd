use std::collections::HashMap;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use std::fmt::Write;

#[derive(Debug, PartialEq)]
enum Method {
	GET,
	HEAD,
}

#[derive(Debug, PartialEq)]
enum Version {
	One,
	OneOne,
	Two,
}

#[derive(Debug, PartialEq)]
pub struct Request {
	method: Method,
	path: String,
	version: Version,
	headers: HashMap<String, String>,
}

#[derive(Debug)]
pub enum Error {
	IoError(std::io::Error),
	Malformed,
	Missing,
	BadVersion,
	BadHeader,
	TooMuch,
	BadMethod,
}

impl From<std::io::Error> for Error {
	fn from(source: std::io::Error) -> Self {
		Self::IoError(source)
	}
}

impl Request {
	pub async fn read<T: AsyncBufReadExt + Unpin> (reader: &mut T) -> 
	Result<Self, Error> {
		let mut lines = reader.lines();
		let status = lines.next_line().await?
			.ok_or(Error::Missing)?;

		let mut words = status.split_ascii_whitespace();
		let method = words.next()
			.ok_or(Error::Missing)?;
		let method = match method {
			"GET" => Method::GET,
			"HEAD" => Method::HEAD,
			_ => return Err(Error::BadMethod),
		};
		let path = words.next()
			.ok_or(Error::Missing)?;

		let version = words.next()
			.ok_or(Error::Malformed)?;
		let version = match version {
			"HTTP/1.0" => Version::One,
			"HTTP/1.1" => Version::OneOne,
			"HTTP/2" => Version::Two,
			_ => return Err(Error::BadVersion),
		};

		if words.next().is_some() {
			return Err(Error::TooMuch);
		}

		let mut headers = HashMap::new();
		while let Some(line) = lines.next_line().await? {
			if line.is_empty() {
				break;
			}
			let (key, value) = line.split_once(':')
				.ok_or(Error::BadHeader)?;
			headers.insert(key.to_string(), value.to_string());
		}
		Ok(Self {
			method, path: path.to_string(), version, headers
		})
	}
	pub fn path(&self) -> &String {
		&self.path
	}
}

#[derive(Clone, Copy)]
pub enum ResponseCode {
	Ok,
	NotFound,
	PermissionDenied,
	InternalError,
}

pub struct Response {
	version: Version,
	response: ResponseCode,
	headers: HashMap<String, String>,
}

impl Response {
	pub fn new(response: ResponseCode, len: usize) -> Self {
		let mut headers = HashMap::new();
		headers.insert("Content-length".to_string(), len.to_string());
		Self {
			version: Version::OneOne,
			response,
			headers,
		}
	}
	pub async fn write<T: AsyncWriteExt + Unpin> (&self, writer: &mut T) 
	-> std::io::Result<()> {
		let version = match self.version {
			Version::One => "HTTP/1.0",
			Version::OneOne => "HTTP/1.1",
			Version::Two => "HTTP/2.0",
		};
		writer.write(version.as_bytes()).await?;
		writer.write_u8(b' ').await?;
		let response = match self.response {
			ResponseCode::Ok => "200 OK",
			ResponseCode::NotFound => "404 Not Found",
			ResponseCode::PermissionDenied => "403 Forbidden",
			ResponseCode::InternalError => "500 Internal Server Error",
		};
		writer.write(response.as_bytes()).await?;
		writer.write(b"\r\n").await?;
		for (key, value) in &self.headers {
			let mut str = String::new();
			write!(str, "{key}: {value}\r\n").unwrap();
			writer.write(str.as_bytes()).await?;
		}
		writer.write(b"\r\n").await?;
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use tokio::io::BufReader;
	#[tokio::test]
	async fn header() {
		let buf = b"GET / HTTP/1.1\r\n";
		let mut reader = BufReader::new(&buf[..]);
		let found = Request::read(&mut reader).await.unwrap();

		let wanted = Request {
			method: Method::GET,
			version: Version::OneOne,
			path: "/".into(),
			headers: HashMap::new(),
		};
		assert_eq!(found, wanted);
	}
}
