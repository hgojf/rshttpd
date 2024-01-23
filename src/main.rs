mod proc;
mod http;
mod tls;

mod fs;
mod client;
mod crypto;

use tokio_seqpacket::UnixSeqpacket;
use pledge::pledge;
use tokio::net::{UnixStream, TcpListener};
use tokio::signal::unix::{signal, SignalKind};
use tokio_util::sync::CancellationToken;
use std::os::fd::OwnedFd;

const PROGRAM_PATH: &'static str = "/home/user/src/personal/httpd-rs/target/debug/httpd";

#[tokio::main(flavor = "current_thread")]
async fn main() {
	let mut args = std::env::args();
	args.next().expect("what?");
	if let Some(arg) = args.next() {
		if arg == "-f" {
			proc::privdrop("/var/www/htdocs/", "www").expect("privdrop");
			fs::main().await;
		}
		if arg == "-c" {
			proc::privdrop("/var/empty", "www").expect("privdrop");
			client::main().await;
		}
		if arg == "-e" {
			proc::privdrop("/var/empty", "www").expect("privdrop");
			crypto::main().await;
		}
	}

	let config = ManagerConfig {
		cert: "./cert.pem",
		key: "./key.pem",
		fs: fs::Server::new(vec![
			fs::Location::new("/", false),
			fs::Location::new("/private/", true),
		]),
		addr: "127.0.0.1:199".parse().unwrap(),
	};
	let server = Manager::new(PROGRAM_PATH, config).await.unwrap();

	proc::privdrop("/var/empty/", "www").expect("privdrop");
	pledge("stdio sendfd proc inet dns", None).expect("pledge");

	let token = CancellationToken::new();
	let mytok = token.clone();
	tokio::spawn(async move {
		loop {
			tokio::select! {
				err = server.serve() => {
					eprintln!("{:?}", err);
				}
				_ = mytok.cancelled() => break,
			}
		};
		if let Err(err) = server.end().await {
			eprintln!("{err}");
		}
	});

	let mut sigchld = signal(SignalKind::child()).expect("signal");
	let mut sigint = signal(SignalKind::child()).expect("signal");
	let mut sigterm = signal(SignalKind::child()).expect("signal");

	tokio::select! {
		_ = sigchld.recv() => {
			eprintln!("received SIGCHLD, exiting...");
		},
		_ = sigint.recv() => {},
		_ = sigterm.recv() => {},
	}
	token.cancel();
}

struct ManagerConfig<'a> {
	cert: &'a str,
	key: &'a str,
	fs: fs::Server,
	addr: std::net::SocketAddr,
}

struct Manager {
	fs: proc::Process, 
	client: proc::Process,
	crypto: proc::Process,

	listener: TcpListener,
}

/* XXX: do priviledged things (opening socket, exec-ing) 
 * before doing serde things
 */
impl Manager {
	async fn new(prog: &str, config: ManagerConfig<'_>) -> std::io::Result<Self> {
		let certfile = tokio::fs::File::open(config.cert).await?.into_std().await;
		let keyfile = tokio::fs::File::open(config.key).await?.into_std().await;

		let fs = proc::ProcessBuilder::new(prog, "httpd: filesystem", "-f")
			.build()?;
		let client = proc::ProcessBuilder::new(prog, "httpd: filesystem", "-c")
			.build()?;
		let crypto = proc::ProcessBuilder::new(prog, "httpd: filesystem", "-e")
			.build()?;

		crypto.peer().send_fds(&[certfile.into(), keyfile.into()]).await?;

		let message = fs::RecvMessageMain::Config(config.fs);
		let buf = serde_cbor::to_vec(&message).expect("serde");
		fs.peer().send(&buf).await?;

		let (a, b) = UnixSeqpacket::pair()?;

		fs.peer().send_fd(a).await?;
		client.peer().send_fd(b).await?;

		let listener = TcpListener::bind(config.addr).await?;
		Ok(Self {
			fs, client, crypto, listener
		})
	}

	async fn serve(&self) -> std::io::Result<()> {
		let (con, _) = self.listener.accept().await?;
		let con = OwnedFd::from(con.into_std()?);
		let (a, b) = {
			let (a, b) = UnixStream::pair()?;
			(OwnedFd::from(a.into_std()?), OwnedFd::from(b.into_std()?))
		};
		self.crypto.peer().send_fds(&[con, a]).await?;
		self.client.peer().send_fds(&[b]).await?;
		Ok(())
	}

	async fn end(self) -> std::io::Result<()> {
		self.crypto.end().await?;
		self.fs.end().await?;
		self.client.end().await?;
		Ok(())
	}
}
