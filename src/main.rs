mod proc;
mod http;

mod fs;
mod client;

use tokio_seqpacket::UnixSeqpacket;
use std::os::fd::AsFd;
use pledge::pledge;
use tokio::net::TcpListener;
use tokio::signal::unix::{signal, SignalKind};

const PROGRAM_PATH: &'static str = "/home/user/src/personal/httpd-rs/target/debug/httpd";

struct Server {
	fs: fs::Server,
}

#[tokio::main]
async fn main() {
	let mut args = std::env::args();
	args.next().expect("what?");
	if let Some(arg) = args.next() {
		if arg == "-f" {
			fs::main().await;
		}
		if arg == "-c" {
			client::main().await;
		}
	}

	unveil::unveil(PROGRAM_PATH, "x").expect("unveil");
	pledge("stdio sendfd proc exec inet dns", None).expect("pledge");

	let mut fs = proc::ProcessBuilder::new(PROGRAM_PATH, "-f")
		.build().expect("exec");
	let mut client = proc::ProcessBuilder::new(PROGRAM_PATH, "-c")
		.build().expect("exec");

	pledge("stdio sendfd proc inet dns", None).expect("pledge");

	let server = Server {
		fs: fs::Server::new(vec![
			fs::Location::new("/var/www/htdocs/".to_string(), false),
			//fs::Location::new("/var/www/htdocs/bgplg/".to_string(), true),
		]),
	};

	let message = fs::RecvMessageMain::Config(server.fs);
	let buf = serde_cbor::to_vec(&message).expect("fuck you");
	fs.peer().send(&buf).await.expect("write");

	let (a, b) = UnixSeqpacket::pair().expect("socketpair");
	fs.peer().send_fd(a.as_fd()).await.expect("send_fd");
	client.peer().send_fd(b.as_fd()).await.expect("send_fd");

	let socket = TcpListener::bind("127.0.0.1:1999").await.unwrap();

	pledge("stdio sendfd proc inet", None).expect("pledge");

	let mut sigchld = signal(SignalKind::child()).expect("signal");
	let mut sigint = signal(SignalKind::child()).expect("signal");
	let mut sigterm = signal(SignalKind::child()).expect("signal");

	loop {
		tokio::select! {
			_ = sigchld.recv() => {
				break;
			}
			_ = sigint.recv() => { break }
			_ = sigterm.recv() => { break }
			con = socket.accept() => {
				let sock = match con {
					Ok((sock, _)) => sock,
					Err(_) => continue,
				};
				client.peer().send_fd(sock.as_fd()).await.expect("send_fd");
			}
		}
	};

	fs.end().await.expect("wait");
	client.end().await.expect("wait");
}
