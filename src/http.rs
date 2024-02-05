use std::collections::HashMap;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, AsyncWrite};
use std::fmt::Write;

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Method {
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
	headers: HashMap<String, Vec<String>>,
}

#[derive(Debug)]
pub enum Error {
	IoError(std::io::Error),
	Malformed,
	Missing,
	BadVersion,
	BadHeader,
	BadPath,
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
		if !path.starts_with('/') {
			return Err(Error::BadPath);
		}

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
			Version::Two => "HTTP/2.0",
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
