use crate::{proc, tls};
use tokio::signal::unix::{signal, SignalKind};
use tokio_seqpacket::ancillary::AncillaryMessage;
use tokio::net::{TcpStream, UnixStream};
use tokio::io::AsyncWriteExt;
use std::os::fd::{FromRawFd, AsRawFd};
use pledge::pledge;
use std::sync::Arc;

pub async fn main() -> ! {
	pledge("stdio recvfd", None).expect("pledge");

	let mut parent = unsafe {
		proc::Peer::get_parent()
	};

	let mut buffer: [u8; 128] = [0; 128];
	let (_, ancillary) = parent.socket()
		.recv_vectored_with_ancillary(&mut [], &mut buffer).await.expect("recv");
	let mut messages = ancillary.messages();
	let (certfile, keyfile) = match messages.next() {
		Some(AncillaryMessage::FileDescriptors(fds)) => {
			let cfd = fds.get(0).expect("no file descriptor");
			let pfd = fds.get(1).expect("no file descriptor");
			unsafe {
				let c = std::fs::File::from_raw_fd(cfd.as_raw_fd());
				let p = std::fs::File::from_raw_fd(pfd.as_raw_fd());
				(c, p)
			}
		}
		_ => panic!("bad message"),
	};

	let cert = tls::certs_from_file(certfile).unwrap();
	let key = tls::keys_from_file(keyfile).unwrap();

	let config = rustls::server::ServerConfig::builder()
		.with_no_client_auth()
		.with_single_cert(cert, key)
		.unwrap();
	let config = Arc::new(config);

	let mut sigint = signal(SignalKind::terminate()).expect("signal");

	let mut buffer: [u8; 128] = [0; 128];
	loop {
		tokio::select! {
			_ = sigint.recv() => { break }

			read = parent.socket()
			.recv_vectored_with_ancillary(&mut [], &mut buffer) => {
				let (_, ancillary) = read.expect("bad message");
				let mut message = ancillary.messages();
				let (mut client, mut server) = match message.next() {
					Some(AncillaryMessage::FileDescriptors(fds)) => {
						let cfd = fds.get(0).expect("no file descriptor");
						let sfd = fds.get(1).expect("no file descriptor");
						unsafe {
							let client = std::net::TcpStream::from_raw_fd(cfd.as_raw_fd());
							let client = TcpStream::from_std(client).unwrap();
							let server = std::os::unix::net::UnixStream
								::from_raw_fd(sfd.as_raw_fd());
							let server = UnixStream::from_std(server).unwrap();
							(client, server)
						}
					}
					_ => panic!("bad message"),
				};
				let stream = tokio_rustls::TlsAcceptor::from(config.clone());
				let mut client = stream.accept(client).await.unwrap();
				tokio::io::copy_bidirectional(&mut client, &mut server).await.unwrap();
				client.flush().await.unwrap();
			}
		}
	}

	std::process::exit(0);
}
