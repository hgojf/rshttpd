mod proc;
mod http;
mod tls;
mod mime;

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
	let server = Manager::new(PROGRAM_PATH, config, global_config).await.unwrap();

	proc::privdrop("/var/empty/", "www").expect("privdrop");
	pledge("stdio sendfd proc inet dns", None).expect("pledge");

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

struct GlobalConfig {
	mime: mime::MimeDb,
}

impl GlobalConfig {
	async fn new(path: &str) -> std::io::Result<Self> {
		let mime_file = tokio::fs::File::open(path).await?;
		let mut mime_file = tokio::io::BufReader::new(mime_file);
		let mime = mime::MimeDb::new(&mut mime_file).await.unwrap();
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
			Some(tls) => {
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

		let message = fs::RecvMessageMain::Config(config.fs);
		let buf = serde_cbor::to_vec(&message).expect("serde");
		fs.peer().send(&buf).await?;

		let (a, b) = UnixSeqpacket::pair()?;

		fs.peer().send_fd(a).await?;
		let message = serde_cbor::to_vec(&global_config.mime).unwrap();
		client.peer().send_with_fd(b, &message).await?;

		let listener = TcpListener::bind(config.addr).await?;
		Ok(Self {
			fs, client, acceptor, listener
		})
	}

	async fn serve(&self) -> std::io::Result<()> {
		let (con, _) = self.listener.accept().await?;
		let con = OwnedFd::from(con.into_std()?);
		match &self.acceptor {
			Acceptor::Plain => {
				self.client.peer().send_fds(&[con]).await?;
			}
			Acceptor::Tls(tls) => {
				let (a, b) = UnixStream::pair()?;
				let (a, b) = (a.into_std()?, b.into_std()?);
				tls.peer().send_fds(&[con, a.into()]).await?;
				self.client.peer().send_fds(&[b.into()]).await?;
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
