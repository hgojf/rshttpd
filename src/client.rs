use crate::{fs, proc, http, mime};
use http::Content;
use tokio_seqpacket::UnixSeqpacket;
use tokio::net::{UnixStream, TcpStream};
use tokio::signal::unix::{signal, SignalKind};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, AsyncReadExt, BufStream, BufReader};
use serde::{Serialize, Deserialize};
use num_enum::{TryFromPrimitive, IntoPrimitive};
use proc::pledge;
use std::sync::Arc;
use std::fmt::Write;
use thiserror::Error;

#[async_trait::async_trait]
impl Content for tokio::fs::File {
	async fn len(&self) -> std::io::Result<usize> {
		let len = self.metadata().await?.len().try_into().unwrap();
		Ok(len)
	}
	async fn write<T: AsyncWrite + Unpin + Send> (&mut self, writer: &mut T) -> std::io::Result<()> {
		tokio::io::copy(self, writer).await?;
		Ok(())
	}
}

struct Directory(String);

#[async_trait::async_trait]
impl Content for Directory {
	async fn len(&self) -> std::io::Result<usize> {
		Ok(self.0.len())
	}
	async fn write<T: AsyncWrite + Unpin + Send> (&mut self, writer: &mut T) -> std::io::Result<()> {
		writer.write_all(self.0.as_bytes()).await?;
		Ok(())
	}
}

#[derive(Serialize, Deserialize)]
pub struct ClientConfig {
	pub tls: bool,
}

pub async fn main() -> ! {
	pledge("stdio recvfd sendfd", None).expect("pledge");
	let parent = unsafe {
		proc::Peer::get_parent()
	};
	
	let (fs, mimedb) = {
		let (_, (ffd, mfd)) = parent.recv_with_fds(&mut []).await.expect("no file descriptor");
		let sock = UnixSeqpacket::try_from(ffd).unwrap();
		let peer = Arc::new(proc::Peer::from_stream(sock));
		let mimedb = std::fs::File::from(mfd);
		let mimedb = tokio::fs::File::from_std(mimedb);
		let mut mimedb = BufReader::new(mimedb);
		let mimedb = crate::mime::MimeDb::new(&mut mimedb).await.unwrap();
		let mimedb = Arc::new(mimedb);
		(peer, mimedb)
	};

	let config: ClientConfig = {
		let mut buf = [0u8; 128];
		let len = parent.socket().recv(&mut buf).await.unwrap();
		serde_cbor::from_slice(&buf[..len]).expect("serde")
	};

	let mut sigint = signal(SignalKind::interrupt()).expect("signal");

	loop {
		tokio::select! {
			_ = sigint.recv() => { break },
			stream = Accept::accept(&parent, config.tls) => {
				let stream = match stream {
					Ok(stream) => stream,
					Err(err) => {
						eprintln!("Parent sent bad stream: {err}");
						std::process::exit(1);
					}
				};
				let fs = fs.clone();
				let mimedb = mimedb.clone();
				match stream {
					Accept::Tls(stream) => {
						let mut client = BufStream::new(stream);
						let Ok(byte) = client.read_u8().await else {
							continue;
						};
						let version: HttpVersion = byte.try_into().unwrap();
						let mut client = Client {
							version: version.into(), client, fs: &fs, mimedb: &mimedb
						};
						if let Err(err) = client.main().await {
							eprintln!("client error: {err}");
						}
					}
					Accept::Plain(stream) => {
						let client = BufStream::new(stream);
						let mut client = Client {
							version: HttpVersion::Unknown, client, fs: &fs, mimedb: &mimedb
						};
						if let Err(err) = client.main().await {
							eprintln!("client error: {err}");
						}
					}
				}
			}
		}
	};
	std::process::exit(0);
}

#[repr(u8)]
#[derive(TryFromPrimitive, IntoPrimitive)]
pub enum HttpVersion {
	One,
	OneOne,
	Unknown,
}

impl From<http::Version> for HttpVersion {
	fn from(source: http::Version) -> Self {
		match source {
			http::Version::One => Self::One,
			http::Version::OneOne => Self::OneOne,
		}
	}
}

pub enum Accept {
	Tls(UnixStream),
	Plain(TcpStream),
}

impl Accept {
	async fn accept(peer: &proc::Peer, tls: bool) -> std::io::Result<Self> {
		let (_, fd) = peer.recv_with_fd(&mut []).await?;
		if tls {
			let stream = std::os::unix::net::UnixStream::from(fd);
			let stream = UnixStream::from_std(stream)?;
			Ok(Self::Tls(stream))
		}
		else {
			let stream = std::net::TcpStream::from(fd);
			let stream = TcpStream::from_std(stream)?;
			Ok(Self::Plain(stream))
		}
	}
}

struct Client<'a, T: AsyncRead + AsyncWrite> {
	version: HttpVersion,
	mimedb: &'a mime::MimeDb,
	fs: &'a proc::Peer,
	client: BufStream<T>,
}

#[derive(Debug, Error)]
enum ClientError {
	#[error("Keepalive timeout elapsed")]
	Elapsed {
		#[from]
		source: tokio::time::error::Elapsed,
	},
	#[error("HTTP error: {source}")]
	Http {
		#[from]
		source: http::Error,
	},
	#[error("I/O error: {source}")]
	Io {
		#[from]
		source: std::io::Error,
	}
}

impl <T: Unpin + Send + AsyncRead + AsyncWrite> Client<'_, T> {
	async fn resolve_path_under(&self, path: &str) -> std::io::Result<fs::OpenResponse> {
		let (mine, theirs) = UnixSeqpacket::pair()?;
		let message = fs::RecvMessageClient::Open(path);
		let vec = serde_cbor::to_vec(&message).expect("serde");
		self.fs.send_with_fd(theirs, &vec).await?;
		let message = fs::OpenResponse::recv(&mine).await?;
		Ok(message)
	}
	async fn resolve_path(&self, path: &str) -> std::io::Result<fs::OpenResponse> {
		if path.ends_with('/') {
			let index = format!("{}/index.html", path);
			let response = self.resolve_path_under(&index).await?;
			if let fs::OpenResponse::File(info, file) = response {
				Ok(fs::OpenResponse::File(info, file))
			}
			else {
				self.resolve_path_under(path).await
			}
		}
		else {
			self.resolve_path_under(path).await
		}
	}

	async fn main(&mut self) -> Result<(), ClientError> {
		match self.version {
			HttpVersion::One => {
				Ok(self.run().await?)
			}
			HttpVersion::OneOne => {
				loop {
					self.run().await?;
				}
			}
			HttpVersion::Unknown => {
				let request = self.get_request().await?;
				self.respond(&request).await?;
				match request.version() {
					http::Version::One => return Ok(()),
					http::Version::OneOne => {
						loop {
							self.run().await?;
						}
					}
				}
			}
		}
	}

	const KEEPALIVE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
	async fn get_request(&mut self) -> Result<http::Request, ClientError> {
		let request = tokio::time::timeout(Self::KEEPALIVE_TIMEOUT,
			http::Request::read(&mut self.client)).await??;
		Ok(request)
	}

	async fn respond(&mut self, request: &http::Request) -> Result<(), ClientError> {
		let response = self.resolve_path(request.path()).await.unwrap();
		let head = request.method() == http::Method::HEAD;

		match response {
			fs::OpenResponse::File(info, mut file) => {
				let kind = self.mimedb.get(&info.name).unwrap_or("application/octet-stream");
				let headers = [("Content-Type", kind)];
				let mut response = http::Response::new(&mut file, &headers, head);
				response.write(&mut self.client).await?;
			}
			fs::OpenResponse::Dir(dir) => {
				let mut string = String::new();
				string.push_str("<!DOCTYPE html>\n<html>\n<body>\n<pre>\n");
				string.push_str("<a href=../>../</a>\n");
				for file in dir {
					write!(string, "<a href={0}>{0}</a>\n", file.name).unwrap();
				}
				string.push_str("</pre>\n</body>\n</html>\n");
				let mut dir = Directory(string);
				let headers = [("Content-Type", "text/html")];
				let mut response = http::Response::new(&mut dir, &headers, head);
				response.write(&mut self.client).await?;
			}
			fs::OpenResponse::FileError(error) => {
				let mut response = http::ResponseCode::from(error);
				let mut response = http::Response::new(&mut response, &[], head);
				response.write(&mut self.client).await?;
			}
		}
		self.client.flush().await?;
		Ok(())
	}

	async fn run(&mut self) -> Result<(), ClientError> {
		let request = self.get_request().await?;
		self.respond(&request).await?;
		Ok(())
	}
}

impl From<fs::FileError> for http::ResponseCode {
	fn from(resp: fs::FileError) -> http::ResponseCode {
		match resp {
			fs::FileError::NotFound => http::ResponseCode::NotFound,
			fs::FileError::NotAllowed => http::ResponseCode::PermissionDenied,
			fs::FileError::SpecialFile => http::ResponseCode::InternalError,
			fs::FileError::Io => http::ResponseCode::InternalError,
		}
	}
}
