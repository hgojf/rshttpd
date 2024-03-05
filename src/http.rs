use std::collections::HashMap;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, AsyncWrite};
use std::fmt::Write;
use num_enum::{TryFromPrimitive, IntoPrimitive};
use thiserror::Error;

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Method {
	GET,
	HEAD,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, TryFromPrimitive, IntoPrimitive)]
pub enum Version {
	One,
	OneOne,
}

struct HttpPath(String);

impl HttpPath {
	fn from_str(str: &[u8]) -> Option<HttpPath> {
		if !str.starts_with(b"/") {
			return None;
		}
		let mut ret = Vec::new();
		let mut chars = str.iter();
		loop {
			let char = match chars.next() {
				Some(c) => c,
				None => break,
			};

			if *char == b'%' {
				let mut arr = [0u8; 2];
				arr[0] = *chars.next()?;
				arr[1] = *chars.next()?;
				let str = std::str::from_utf8(&arr).ok()?;
				let val = u8::from_str_radix(str, 16).ok()?;
				ret.push(val);
			}
			else {
				ret.push(*char);
			}
		}
		let str = String::from_utf8(ret).ok()?;
		Some(Self(str))
	}
}

#[derive(Debug, PartialEq)]
pub struct Request {
	method: Method,
	path: String,
	version: Version,
	headers: HashMap<String, Vec<String>>,
}

#[derive(Debug, Error)]
pub enum Error {
	#[error("I/O error: {source}")]
	Io {
		#[from]
		source: std::io::Error,
	},
	#[error("Malformed request")]
	Malformed,
	#[error("Missing data in request")]
	Missing,
	#[error("Bad version in request")]
	BadVersion,
	#[error("Bad header in request")]
	BadHeader,
	#[error("Bad path in request")]
	BadPath,
	#[error("Extra data in status line in request")]
	ExtraWord,
	#[error("Bad method in request")]
	BadMethod,
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
		let path = HttpPath::from_str(path.as_bytes()).ok_or(Error::BadPath)?;
		let path = path.0;

		let version = words.next()
			.ok_or(Error::Malformed)?;
		let version = match version {
			"HTTP/1.0" => Version::One,
			"HTTP/1.1" => Version::OneOne,
			_ => return Err(Error::BadVersion),
		};

		if words.next().is_some() {
			return Err(Error::ExtraWord);
		}

		let mut headers = HashMap::new();
		while let Some(line) = lines.next_line().await? {
			if line.is_empty() {
				break;
			}
			let (key, value) = line.split_once(':')
				.ok_or(Error::BadHeader)?;
			let values = value.split(',');
			for value in values {
				let value = value.trim();
				headers.entry(key.to_string()).and_modify(|e: &mut Vec<String>| {
					e.push(value.to_string());
				}).or_insert(vec![value.to_string()]);
			}
		}
		Ok(Self {
			method, path: path.to_string(), version, headers
		})
	}
	pub fn path(&self) -> &String {
		&self.path
	}
	pub fn method(&self) -> Method {
		self.method
	}
}

#[derive(Clone, Copy)]
pub enum ResponseCode {
	Ok,
	NotFound,
	PermissionDenied,
	InternalError,
}

pub struct Response<'a, T: Content> {
	content: &'a mut T,
	version: Version,
	head: bool,
	headers: &'a [(&'a str, &'a str)],
}

impl <'a, T: Content> Response<'a, T> {
	pub fn new(content: &'a mut T, headers: &'a [(&'a str, &'a str)], head: bool) 
	-> Response<'a, T> {
		Self {
			version: Version::OneOne,
			content,
			head,
			headers,
		}
	}
	pub async fn write<E: AsyncWriteExt + Unpin + Send> (&mut self, writer: &mut E) 
	-> std::io::Result<()> {
		let version = match self.version {
			Version::One => "HTTP/1.0",
			Version::OneOne => "HTTP/1.1",
		};
		writer.write_all(version.as_bytes()).await?;
		writer.write_u8(b' ').await?;
		let response = self.content.code();
		let response = response.val();
		writer.write_all(response.as_bytes()).await?;
		writer.write_all(b"\r\n").await?;

		let mut str = String::new();
		write!(str, "Content-Length: {}\r\n", self.content.len().await.expect("idk"))
			.unwrap();
		writer.write_all(str.as_bytes()).await?;

		for (key, value) in self.headers {
			let mut str = String::new();
			write!(str, "{key}: {value}\r\n").unwrap();
			writer.write_all(str.as_bytes()).await?;
		}
		writer.write_all(b"\r\n").await?;
		if !self.head {
			self.content.write(writer).await?;
		}
		Ok(())
	}
}

#[async_trait::async_trait]
pub trait Content {
	fn code(&self) -> ResponseCode 
	{
		ResponseCode::Ok
	}
	async fn len(&self) -> std::io::Result<usize>;
	async fn write<T: AsyncWrite + Unpin + Send> (&mut self, writer: &mut T) -> std::io::Result<()>;
}

impl ResponseCode {
	const fn val(&self) -> &'static str {
		match self {
			ResponseCode::Ok => "200 OK",
			ResponseCode::NotFound => "404 Not found",
			ResponseCode::PermissionDenied => "403 Forbidden",
			ResponseCode::InternalError => "500 Internal Server Error",
		}
	}
}

#[async_trait::async_trait]
impl Content for ResponseCode {
	fn code(&self) -> ResponseCode {
		*self
	}

	async fn len(&self) -> std::io::Result<usize> {
		Ok(0)
	}

	async fn write<T: AsyncWrite + Unpin + Send> (&mut self, _writer: &mut T) 
	-> std::io::Result<()> {
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use tokio::io::BufReader;
	#[tokio::test]
	async fn header() {
		let buf = b"GET /%20 HTTP/1.1\r\n";
		let mut reader = BufReader::new(&buf[..]);
		let found = Request::read(&mut reader).await.unwrap();

		let wanted = Request {
			method: Method::GET,
			version: Version::OneOne,
			path: "/ ".into(),
			headers: HashMap::new(),
		};
		assert_eq!(found, wanted);
	}
}
