use crate::{fs, proc, http};
use tokio_seqpacket::UnixSeqpacket;
use tokio_seqpacket::ancillary::AncillaryMessage;
use tokio::net::TcpStream;
use tokio::io::{AsyncWriteExt, BufReader};
use std::os::fd::{FromRawFd, AsRawFd};
use pledge::pledge;

enum File {
	File(tokio::fs::File),
	Dir(tokio::fs::ReadDir),
	Error(http::ResponseCode),
}

impl File {
	async fn len(&self) -> std::io::Result<usize> {
		match self {
			Self::File(file) => {
				Ok(file.metadata().await?.len().try_into().unwrap())
			}
			Self::Dir(_) => {
				panic!()
			}
			Self::Error(_) => Ok(3),
		}
	}
	async fn write<T: AsyncWriteExt + Unpin > (&mut self, writer: &mut T) 
	-> std::io::Result<()> {
		match self {
			Self::File(ref mut file) => {
				tokio::io::copy(file, writer).await?;
			}
			Self::Dir(_) => {
				panic!()
			}
			Self::Error(http::ResponseCode::NotFound) => {
				writer.write(b"404").await?;
			}
			Self::Error(http::ResponseCode::PermissionDenied) => {
				writer.write(b"403").await?;
			}
			Self::Error(http::ResponseCode::InternalError) => {
				writer.write(b"500").await?;
			}
			Self::Error(error) => {
				writer.write(b"123").await?;
			}
		}
		Ok(())
	}
}

pub async fn main() -> ! {
	pledge("stdio recvfd", None).expect("pledge");
	let mut parent = unsafe {
		proc::Peer::get_parent()
	};
	
	let mut buffer: [u8; 128] = [0; 128];
	let (_, ancillary) = parent.socket()
		.recv_vectored_with_ancillary(&mut [], &mut buffer).await.expect("recv");
	let mut message = ancillary.messages();
	let fs = match message.next() {
		Some(AncillaryMessage::FileDescriptors(fds)) => {
			let fd = fds.get(0).expect("no file descriptor");
			unsafe {
				UnixSeqpacket::from_raw_fd(fd.as_raw_fd()).unwrap()
			}
		}
		_ => panic!("bad message"),
	};
	let mut fs = proc::Peer::from_stream(fs);

	loop {
		let mut buffer: [u8; 128] = [0; 128];
		let (_, ancillary) = parent.socket()
			.recv_vectored_with_ancillary(&mut [], &mut buffer).await.expect("recv");
		let mut message = ancillary.messages();
		let stream = match message.next() {
			Some(AncillaryMessage::FileDescriptors(fds)) => {
				let fd = fds.get(0).expect("no file descriptor");
				unsafe {
					let stream = std::net::TcpStream::from_raw_fd(fd.as_raw_fd());
					TcpStream::from_std(stream).unwrap()
				}
			}
			_ => panic!("bad message"),
		};
		let mut reader = BufReader::new(stream);
		let request = http::Request::read(&mut reader).await.expect("shit");

		let message = fs::RecvMessageClient::Open(request.path().to_string());
		let vec = serde_cbor::to_vec(&message).expect("serde");
		fs.send(&vec).await.expect("send");

		let mut anbuf: [u8; 128] = [0; 128];
		let mut buf: [u8; 1024] = [0; 1024];
		let slice = std::io::IoSliceMut::new(&mut buf);
		let (len, ancillary) = fs.socket()
			.recv_vectored_with_ancillary(&mut [slice], &mut anbuf)
			.await.expect("recv");
		let buf = &buf[..len];
		let message: fs::OpenResponse = serde_cbor::from_slice(&buf).expect("serde");
		let (response, mut file) = match message {
			fs::OpenResponse::File => {
				let mut messages = ancillary.messages();
				let file = match messages.next() {
					Some(AncillaryMessage::FileDescriptors(fds)) => {
						let fd = fds.get(0).expect("no file descriptors");
						unsafe {
							tokio::fs::File::from_raw_fd(fd.as_raw_fd())
						}
					}
					_ => panic!("bad message"),
				};
				(http::ResponseCode::Ok, File::File(file))
			}
			fs::OpenResponse::Dir => panic!(),
			fs::OpenResponse::NotFound => {
				let code = http::ResponseCode::NotFound;
				(code, File::Error(code))
			}
			fs::OpenResponse::NotAllowed => {
				let code = http::ResponseCode::PermissionDenied;
				(code, File::Error(code))
			}
			fs::OpenResponse::OtherError => {
				let code = http::ResponseCode::InternalError;
				(code, File::Error(code))
			}
		};

		let len = file.len().await.expect("fstat");
		let response = http::Response::new(response, len);
		response.write(&mut reader).await.expect("send");
		file.write(&mut reader).await.expect("send");
	};
}
