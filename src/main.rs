mod proc;
mod http;
mod tls;
mod mime;

mod fs;
mod client;
mod crypto;

use tokio_seqpacket::UnixSeqpacket;
use proc::pledge;
use tokio::net::{UnixStream, TcpListener};
use tokio::signal::unix::{signal, SignalKind};
use tokio_util::sync::CancellationToken;
use std::os::fd::OwnedFd;

use client::ClientConfig;

#[tokio::main(flavor = "current_thread")]
async fn main() {
	let mut args = std::env::args();
	args.next().expect("what?");
	if let Some(arg) = args.next() {
		if arg == "-p" {
			let arg = args.next().unwrap();
			match arg.as_str() {
				"client" => {
					proc::privdrop("/var/empty", "www").expect("privdrop");
					client::main().await;
				}
				"crypto" => {
					proc::privdrop("/var/empty", "www").expect("privdrop");
					crypto::main().await;
				}
				"filesystem" => {
					proc::privdrop("/var/www/htdocs/", "www").expect("privdrop");
					fs::main().await;
				}
				_ => {},
			}
		}
	}

	let current_exe = std::env::current_exe().unwrap();
	let current_exe = current_exe.into_os_string().into_string().unwrap();

	let global_config = GlobalConfig::new("/usr/share/misc/mime.types").await.unwrap();

	let config = ManagerConfig {
		/*
		tls: Some(
			TlsConfig {
				cert: "./cert.pem",
				key: "./key.pem",
			}
		),
		*/
		tls: None,
		fs: fs::Server::new(vec![
			fs::Location::new("/", false),
			fs::Location::new("/private/", true),
		]),
		addr: "127.0.0.1:199".parse().unwrap(),
	};
	let server = Manager::new(&current_exe, config, global_config).await.unwrap();

	proc::privdrop("/var/empty/", "www").expect("privdrop");
	pledge("stdio sendfd proc inet", None).expect("pledge");

	let token = CancellationToken::new();
	let mytok = token.clone();
	tokio::spawn(async move {
		loop {
			tokio::select! {
				err = server.serve() => {
					if let Err(err) = err {
						eprintln!("{:?}", err);
					}
				}
				_ = mytok.cancelled() => break,
			}
		};
		if let Err(err) = server.end().await {
			eprintln!("{err}");
		}
	});

	let mut sigchld = signal(SignalKind::child()).expect("signal");
	let mut sigint = signal(SignalKind::interrupt()).expect("signal");
	let mut sigterm = signal(SignalKind::terminate()).expect("signal");

	tokio::select! {
		_ = sigchld.recv() => {
			eprintln!("received SIGCHLD, exiting...");
		},
		_ = sigint.recv() => {},
		_ = sigterm.recv() => {},
	}
	token.cancel();
}

struct GlobalConfig {
	mime: tokio::fs::File,
}

impl GlobalConfig {
	async fn new(path: &str) -> std::io::Result<Self> {
		let mime = tokio::fs::File::open(path).await?;
		Ok(Self {
			mime
		})
	}
}

struct TlsConfig<'a> {
	cert: &'a str,
	key: &'a str,
}

struct ManagerConfig<'a> {
	tls: Option<TlsConfig<'a>>,
	fs: fs::Server,
	addr: std::net::SocketAddr,
}

enum Acceptor {
	Tls(proc::Process),
	Plain,
}

struct Manager {
	fs: proc::Process, 
	client: proc::Process,
	acceptor: Acceptor,

	listener: TcpListener,
}

/* XXX: do priviledged things (opening socket, exec-ing) 
 * before doing serde things
 */
impl Manager {
	async fn new(prog: &str, config: ManagerConfig<'_>, global_config: GlobalConfig) 
	-> std::io::Result<Self> {

		let fs = proc::ProcessBuilder::new(prog, "filesystem")
			.build()?;
		let client = proc::ProcessBuilder::new(prog, "client")
			.build()?;
		let acceptor = match config.tls {
			Some(ref tls) => {
				let certfile = tokio::fs::File::open(tls.cert).await?.into_std().await;
				let keyfile = tokio::fs::File::open(tls.key).await?.into_std().await;
				let crypto = proc::ProcessBuilder::new(prog, "crypto")
					.build()?;

				crypto.peer().send_fds(&[certfile.into(), keyfile.into()]).await?;
				Acceptor::Tls(crypto)
			}
			None => {
				Acceptor::Plain
			}
		};

		let (a, b) = UnixSeqpacket::pair()?;

		let buf = serde_cbor::to_vec(&config.fs).expect("serde");
		fs.peer().send_with_fd(a, &buf).await?;
		let mime = global_config.mime.into_std().await;
		let mime = OwnedFd::from(mime);
		client.peer().send_fds(&[b.into(), mime]).await?;

		let client_config = ClientConfig {
			tls: config.tls.is_some(),
		};
		let buf = serde_cbor::to_vec(&client_config).expect("serde");
		client.peer().socket().send(&buf).await?;

		let listener = TcpListener::bind(config.addr).await?;
		Ok(Self {
			fs, client, acceptor, listener
		})
	}

	async fn serve(&self) -> std::io::Result<()> {
		let (con, _) = self.listener.accept().await?;
		match &self.acceptor {
			Acceptor::Plain => {
				let con = OwnedFd::from(con.into_std()?);
				self.client.peer().send_with_fd(con, &[]).await?;
			}
			Acceptor::Tls(tls) => {
				let (a, b) = UnixStream::pair()?;
				let a = a.into_std()?;
				let con = OwnedFd::from(con.into_std()?);
				tls.peer().send_fds(&[con, a.into()]).await?;
				self.client.peer().send_with_fd(b.into_std()?, &[]).await?;
			}
		}
		Ok(())
	}

	async fn end(self) -> std::io::Result<()> {
		if let Acceptor::Tls(crypto) = self.acceptor {
			crypto.end().await?;
		}
		self.fs.end().await?;
		self.client.end().await?;
		Ok(())
	}
}
