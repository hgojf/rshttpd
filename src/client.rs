use crate::{fs, proc, http, mime};
use http::Content;
use tokio_seqpacket::UnixSeqpacket;
use tokio_seqpacket::ancillary::OwnedAncillaryMessage;
use tokio::net::TcpStream;
use tokio::signal::unix::{signal, SignalKind};
use tokio::io::{AsyncWrite, AsyncWriteExt, BufReader};
use pledge::pledge;
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
		writer.write(self.0.as_bytes()).await?;
		Ok(())
	}
}

pub async fn main() -> ! {
	pledge("stdio recvfd sendfd", None).expect("pledge");
	let mut parent = unsafe {
		proc::Peer::get_parent()
	};
	
	let mut buf: [u8; 4096] = [0; 4096];
	let (fs, mimedb) = {
		let (len, fd) = parent.recv_with_fd(&mut buf).await.expect("no file descriptor");
		let sock = UnixSeqpacket::try_from(fd).unwrap();
		let peer = Arc::new(proc::Peer::from_stream(sock));
		let mimedb: mime::MimeDb = serde_cbor::from_slice(&buf[..len]).unwrap();
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
					client: stream, fs: &fs, mimedb: &mimedb,
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
	client: TcpStream,
}

impl Client<'_> {
	async fn run(&mut self) -> Result<(), http::Error> {
		let mut reader = BufReader::new(&mut self.client);
		let request = http::Request::read(&mut reader).await?;

		let (mine, theirs) = UnixSeqpacket::pair()?;
		let message = fs::RecvMessageClient::Open(request.path());
		let vec = serde_cbor::to_vec(&message).expect("serde");
		self.fs.send_with_fd(theirs, &vec).await?;

		let mut anbuf: [u8; 128] = [0; 128];
		let mut buf: [u8; 1024] = [0; 1024];
		let slice = std::io::IoSliceMut::new(&mut buf);
		let (len, ancillary) = mine
			.recv_vectored_with_ancillary(&mut [slice], &mut anbuf)
			.await.expect("recv");
		let buf = &buf[..len];
		let message: fs::OpenResponse = serde_cbor::from_slice(&buf).expect("serde");

		let mut headers = http::Headers::new();
		match message {
			fs::OpenResponse::File(info) => {
				let mut messages = ancillary.into_messages();
				let mut file = match messages.next() {
					Some(OwnedAncillaryMessage::FileDescriptors(mut fds)) => {
						let fd = fds.next().expect("no file descriptors");
						let file = std::fs::File::from(fd);
						tokio::fs::File::from_std(file)
					}
					_ => panic!("bad message"),
				};
				let kind = self.mimedb.get(&info.name).unwrap_or("application/octet-stream");
				headers.insert("Content-Type", kind);
				let mut response = http::Response::new(&mut file, &mut headers);
				response.write(&mut reader).await?;
			}
			fs::OpenResponse::Dir(dir) => {
				eprintln!("dir");
				let mut string = String::new();
				string.push_str("<!DOCTYPE html>\n<html>\n<body>\n<pre>\n");
				string.push_str("<a href=../>../</a>\n");
				for file in dir {
					write!(string, "<a href={0}>{0}</a>\n", file.name).unwrap();
				}
				string.push_str("</pre>\n</body>\n</html>\n");
				let mut dir = Directory(string);
				headers.insert("Content-Type", "text/html");
				let mut response = http::Response::new(&mut dir, &mut headers);
				response.write(&mut reader).await?;
			}
			fs::OpenResponse::FileError(error) => {
				let mut response = http::ResponseCode::from(error);
				let mut response = http::Response::new(&mut response, &mut headers);
				response.write(&mut reader).await?;
			}
		};
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
