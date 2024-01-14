use crate::{proc, tls};
use tokio::signal::unix::{signal, SignalKind};
use tokio_seqpacket::ancillary::OwnedAncillaryMessage;
use tokio::net::{TcpStream, UnixStream};
use tokio::io::AsyncWriteExt;
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
	let mut messages = ancillary.into_messages();
	let (certfile, keyfile) = match messages.next() {
		Some(OwnedAncillaryMessage::FileDescriptors(mut fds)) => {
			let cfd = fds.next().expect("no file descriptor");
			let pfd = fds.next().expect("no file descriptor");
			let c = std::fs::File::from(cfd);
			let p = std::fs::File::from(pfd);
			(c, p)
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
				let mut message = ancillary.into_messages();
				let (mut client, mut server) = match message.next() {
					Some(OwnedAncillaryMessage::FileDescriptors(mut fds)) => {
						let cfd = fds.next().expect("no file descriptor");
						let sfd = fds.next().expect("no file descriptor");
						let client = std::net::TcpStream::from(cfd);
						let client = TcpStream::from_std(client).unwrap();
						let server = std::os::unix::net::UnixStream
							::from(sfd);
						let server = UnixStream::from_std(server).unwrap();
						(client, server)
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
