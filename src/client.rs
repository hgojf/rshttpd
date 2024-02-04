use crate::{fs, proc, http, mime};
use http::Content;
use tokio_seqpacket::UnixSeqpacket;
use tokio::net::TcpStream;
use tokio::signal::unix::{signal, SignalKind};
use tokio::io::{AsyncWrite, AsyncWriteExt, BufStream, BufReader};
use proc::pledge;
use std::sync::Arc;
use std::fmt::Write;

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
	let mut parent = unsafe {
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
			stream = parent.recv_fd() => {
				let stream = stream.unwrap();
				let stream = std::net::TcpStream::from(stream);
				let stream = TcpStream::from_std(stream).unwrap();
				let fs = fs.clone();
				let mimedb = mimedb.clone();
				tokio::spawn(async move {
				let mut client = Client {
					client: BufStream::new(stream), fs: &fs, mimedb: &mimedb,
				};
				const KEEPALIVE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
				loop {
					let Ok(result) = tokio::time::timeout(KEEPALIVE_TIMEOUT, client.run()).await 
					else {
						return;
					};
					if let Err(err) = result {
						match err {
							http::Error::Missing => {}, /* EOF (probably) */
							other => eprintln!("{:?}", other),
						}
						return;
					}
				};
				});
			}
			_ = sigint.recv() => { break },
		}
	};
	std::process::exit(0);
}

struct Client<'a> {
	mimedb: &'a mime::MimeDb,
	fs: &'a proc::Peer,
	client: BufStream<TcpStream>,
}

impl Client<'_> {
	async fn resolve_path_under(&self, path: &str) -> std::io::Result<fs::OpenResponse> {
		let (mine, theirs) = UnixSeqpacket::pair()?;
		let message = fs::RecvMessageClient::Open(path);
		let vec = serde_cbor::to_vec(&message).expect("serde");
		self.fs.send_with_fd(theirs, &vec).await?;
		let message = fs::OpenResponse::recv(&mine).await?;
		Ok(message)
	}
	async fn resolve_path(&self, path: &str) -> std::io::Result<fs::OpenResponse> {
		if path.ends_with("/") {
			let index = format!("{}/index.html", path);
			let response = self.resolve_path_under(&index).await?;
			if let fs::OpenResponse::File(info, file) = response {
				return Ok(fs::OpenResponse::File(info, file));
			}
			else {
				return self.resolve_path_under(path).await;
			}
		}
		else {
			return self.resolve_path_under(path).await;
		}
	}

	async fn run(&mut self) -> Result<(), http::Error> {
		let request = http::Request::read(&mut self.client).await?;

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
			_ => panic!(),
		}
	}
}
