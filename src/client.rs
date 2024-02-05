use crate::{fs, proc, http, mime};
use http::Content;
use tokio_seqpacket::UnixSeqpacket;
use tokio::net::{UnixStream, TcpStream};
use tokio::signal::unix::{signal, SignalKind};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, AsyncReadExt, BufStream, BufReader};
use std::os::fd::OwnedFd;
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

	let mut sigint = signal(SignalKind::interrupt()).expect("signal");

	loop {
		tokio::select! {
			_ = sigint.recv() => { break },
			stream = Accept::recv(&parent) => {
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
						let version: http::Version = byte.try_into().unwrap();
						let mut client = Client {
							version, client, fs: &fs, mimedb: &mimedb
						};
						if let Err(err) = client.main().await {
							eprintln!("client error: {err}");
						}
					}
					Accept::Plain(stream) => {
						let client = BufStream::new(stream);
						let mut client = Client {
							version: http::Version::OneOne, client, fs: &fs, mimedb: &mimedb
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

pub enum Accept {
	Tls(UnixStream),
	Plain(TcpStream),
}

#[derive(Debug, Error)]
pub enum AcceptError {
	#[error("I/O error: {0}", source)]
	Io {
		#[from]
		source: std::io::Error,
	},
	#[error("Expected a file descriptor in control message")]
	MissingFd,
	#[error("Connection type was invalid")]
	BadType,
}

impl Accept {
	async fn recv(peer: &proc::Peer) -> Result<Self, AcceptError> {
		let mut buf = [0u8; 1];
		let (len, fd) = peer.recv_with_fd(&mut buf).await?;
		if len != 1 {
			return Err(AcceptError::MissingFd);
		}
		match buf[0] {
			0 => {
				let stream = std::os::unix::net::UnixStream::from(fd);
				let stream = UnixStream::from_std(stream)?;
				Ok(Self::Tls(stream))
			}
			1 => {
				let stream = std::net::TcpStream::from(fd);
				let stream = TcpStream::from_std(stream)?;
				Ok(Self::Plain(stream))
			}
			_ => Err(AcceptError::BadType),
		}
	}
	pub async fn send(self, peer: &proc::Peer) -> std::io::Result<()> {
		let (fd, byte) = match self {
			Self::Tls(stream) => {
				let stream = stream.into_std()?;
				let stream = OwnedFd::from(stream);
				(stream, 0)
			}
			Self::Plain(stream) => {
				let stream = stream.into_std()?;
				let stream = OwnedFd::from(stream);
				(stream, 1)
			}
		};
		peer.send_with_fd(fd, &[byte]).await?;
		Ok(())
	}
}

struct Client<'a, T: AsyncRead + AsyncWrite> {
	version: http::Version,
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
			http::Version::One => {
				Ok(self.run().await?)
			}
			http::Version::OneOne => {
				loop {
					self.run().await?;
				}
			}
			http::Version::Two => todo!(),
		}
	}

	const KEEPALIVE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
	async fn run(&mut self) -> Result<(), ClientError> {
		let request = tokio::time::timeout(Self::KEEPALIVE_TIMEOUT,
		http::Request::read(&mut self.client)).await??;

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
					write!(string, "<a href={0}/>{0}</a>\n", file.name).unwrap();
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
