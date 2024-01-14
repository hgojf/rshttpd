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
use std::os::fd::OwnedFd;

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

	let certfile = tokio::fs::File::open("./cert.pem").await.expect("open");
	let certfile = certfile.into_std().await;
	let keyfile = tokio::fs::File::open("./key.pem").await.expect("open");
	let keyfile = keyfile.into_std().await;

	let mut fs = proc::ProcessBuilder::new(PROGRAM_PATH, "httpd: filesystem", "-f")
		.build().expect("exec");
	let mut client = proc::ProcessBuilder::new(PROGRAM_PATH, "httpd: client", "-c")
		.build().expect("exec");
	let mut crypto = proc::ProcessBuilder::new(PROGRAM_PATH, "httpd: crypto", "-e")
		.build().expect("exec");

	proc::privdrop("/var/empty/", "www").expect("privdrop");
	pledge("stdio sendfd proc inet dns", None).expect("pledge");

	let server = Server {
		fs: fs::Server::new(vec![
			fs::Location::new("/".to_string(), false),
			//fs::Location::new("/var/www/htdocs/bgplg/".to_string(), true),
		]),
	};

	crypto.peer().send_fds(&[certfile.into(), keyfile.into()]).await.expect("send_fd");

	let message = fs::RecvMessageMain::Config(server.fs);
	let buf = serde_cbor::to_vec(&message).expect("fuck you");
	fs.peer().send(&buf).await.expect("write");

	let (a, b) = UnixSeqpacket::pair().expect("socketpair");
	fs.peer().send_fd(a).await.expect("send_fd");
	client.peer().send_fd(b).await.expect("send_fd");

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
				let sock = sock.into_std().expect("into_std");
				let sock = OwnedFd::from(sock);
				let (a, b) = UnixStream::pair().expect("socketpair");
				let (a, b) = {
					let a = a.into_std().unwrap();
					let b = b.into_std().unwrap();
					(OwnedFd::from(a), OwnedFd::from(b))
				};
				crypto.peer().send_fds(&[sock, a]).await.expect("send_fds");
				client.peer().send_fd(b).await.expect("send_fd");
			}
		}
	};

	crypto.end().await.expect("wait");
	fs.end().await.expect("wait");
	client.end().await.expect("wait");
}
