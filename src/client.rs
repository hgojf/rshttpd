use crate::{fs, proc, http};
use http::Content;
use tokio_seqpacket::UnixSeqpacket;
use tokio_seqpacket::ancillary::OwnedAncillaryMessage;
use tokio::net::TcpStream;
use tokio::io::{AsyncWrite, AsyncWriteExt, BufReader};
use pledge::pledge;
use std::sync::Arc;

impl Content for tokio::fs::File {
	async fn len(&self) -> std::io::Result<usize> {
		let len = self.metadata().await?.len().try_into().unwrap();
		Ok(len)
	}
	async fn write<T: AsyncWrite + Unpin> (&mut self, writer: &mut T) -> std::io::Result<()> {
		tokio::io::copy(self, writer).await?;
		Ok(())
	}
}

struct Directory(String);

impl Content for Directory {
	async fn len(&self) -> std::io::Result<usize> {
		Ok(self.0.len())
	}
	async fn write<T: AsyncWrite + Unpin> (&mut self, writer: &mut T) -> std::io::Result<()> {
		writer.write(self.0.as_bytes()).await?;
		Ok(())
	}
}

pub async fn main() -> ! {
	pledge("stdio recvfd sendfd", None).expect("pledge");
	let mut parent = unsafe {
		proc::Peer::get_parent()
	};
	
	let fs = {
		let fd = parent.recv_fd().await.expect("no file descriptor");
		let sock = UnixSeqpacket::try_from(fd).unwrap();
		Arc::new(proc::Peer::from_stream(sock))
	};

	loop {
		let stream = {
			let fd = parent.recv_fd().await.expect("no file descriptor");
			let stream = std::net::TcpStream::from(fd);
			TcpStream::from_std(stream).unwrap()
		};
		let fs = fs.clone();
		tokio::spawn(async move {
			let mut client = Client {
				client: stream, fs: &fs,
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
	};
}

struct Client<'a> {
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

		match message {
			fs::OpenResponse::File => {
				let mut messages = ancillary.into_messages();
				let mut file = match messages.next() {
					Some(OwnedAncillaryMessage::FileDescriptors(mut fds)) => {
						let fd = fds.next().expect("no file descriptors");
						let file = std::fs::File::from(fd);
						tokio::fs::File::from_std(file)
					}
					_ => panic!("bad message"),
				};
				eprintln!("hi");
				let mut response = http::Response::new(&mut file);
				response.write(&mut reader).await?;
			}
			fs::OpenResponse::Dir(dir) => {
				eprintln!("dir");
				let mut string = String::new();
				for file in dir {
					string.push_str(&file.name);
					string.push('\n');
				}
				let mut dir = Directory(string);
				let mut response = http::Response::new(&mut dir);
				response.write(&mut reader).await?;
			}
			fs::OpenResponse::FileError(error) => {
				let mut response = http::ResponseCode::from(error);
				let mut response = http::Response::new(&mut response);
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
