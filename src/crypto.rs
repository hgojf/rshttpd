use crate::{proc, tls, http::Version as HttpVersion};
use tokio::signal::unix::{signal, SignalKind};
use tokio::net::{TcpStream, UnixStream};
use tokio::io::AsyncWriteExt;
use pledge::pledge;
use std::sync::Arc;

pub async fn main() -> ! {
	pledge("stdio recvfd", None).expect("pledge");

	let parent = unsafe {
		proc::Peer::get_parent()
	};

	let (certfile, keyfile) = {
		let (_, (cfd, pfd)) = parent.recv_with_fds(&mut []).await.expect("no file descriptors");
		let c = std::fs::File::from(cfd);
		let p = std::fs::File::from(pfd);
		(c, p)
	};

	let cert = tls::certs_from_file(certfile).unwrap();
	let key = tls::keys_from_file(keyfile).unwrap();

	let mut config = rustls::server::ServerConfig::builder()
		.with_no_client_auth()
		.with_single_cert(cert, key)
		.unwrap();
	config.alpn_protocols = vec![b"http/1.1".to_vec(), b"http/1.0".to_vec()];
	let config = Arc::new(config);

	let mut sigint = signal(SignalKind::terminate()).expect("signal");

	loop {
		tokio::select! {
			_ = sigint.recv() => { break }

			res = parent.recv_with_fds(&mut []) => {
				let (client, server) = {
					let (_, (cfd, sfd)) = res.unwrap();
					let client = std::net::TcpStream::from(cfd);
					let client = TcpStream::from_std(client).unwrap();
					let server = std::os::unix::net::UnixStream
						::from(sfd);
					let server = UnixStream::from_std(server).unwrap();
					(client, server)
				};
				let acceptor = tokio_rustls::TlsAcceptor::from(config.clone());
				let stream = CryptoStream {
					client, server, acceptor
				};
				tokio::spawn(async move {
					if let Err(err) = stream.run().await {
						eprintln!("{err}");
					}
				});
			}
		}
	}

	std::process::exit(0);
}

struct CryptoStream {
	client: TcpStream,
	server: UnixStream,
	acceptor: tokio_rustls::TlsAcceptor,
}

impl CryptoStream {
	async fn run(mut self) -> std::io::Result<()> {
		let mut client = self.acceptor.accept(self.client).await?;
		let version = match client.get_ref().1.alpn_protocol() {
			Some(b"http/1.0") => HttpVersion::One,
			Some(b"http/1.1") => HttpVersion::OneOne,
			_ => HttpVersion::One,
		};
		let byte: u8 = version.into();
		self.server.write_u8(byte).await?;
		tokio::io::copy_bidirectional(&mut client, &mut self.server).await?;
		client.flush().await?;
		Ok(())
	}
}
