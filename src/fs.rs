use crate::proc;
use pledge::pledge;
use unveil::unveil;
use serde_derive::{Serialize, Deserialize};
use tokio_seqpacket::UnixSeqpacket;
use tokio_seqpacket::ancillary::{AncillaryMessage, AncillaryMessageWriter};
use std::os::fd::{AsFd, OwnedFd, AsRawFd};
use tokio::signal::unix::{signal, SignalKind};
use tokio::fs;

pub async fn main() -> ! {
	pledge("stdio sendfd recvfd rpath unveil", None).expect("pledge");
	let mut parent = unsafe {
		proc::Peer::get_parent()
	};

	let mut buf: [u8; 4096] = [0; 4096];
	let len = parent.socket().recv(&mut buf).await.expect("read");
	let message: RecvMessageMain = serde_cbor::from_slice(&buf[..len]).expect("idk");

	let server = match message {
		RecvMessageMain::Config(server) => server,
	};

	for location in &server.locations {
		unveil(&location.path, "r").expect("unveil");
	}
	pledge("stdio sendfd recvfd rpath", None).expect("pledge");

	let mut buffer: [u8; 128] = [0; 128];
	let (_, ancillary) = parent.socket()
		.recv_vectored_with_ancillary(&mut [], &mut buffer).await.expect("recv");
	let mut messages = ancillary.messages();
	let stream = match messages.next() {
		Some(AncillaryMessage::FileDescriptors(fds)) => {
			let fd = fds.get(0).expect("no file descriptor");
			unsafe {
				UnixSeqpacket::from_raw_fd(fd.as_raw_fd()).unwrap()
			}
		}
		_ => panic!("bad message"),
	};

	pledge("stdio sendfd rpath", None).expect("pledge");

	let mut peer = proc::Peer::from_stream(stream);

	let mut buf: [u8; 4096] = [0; 4096];

	let mut sigterm = signal(SignalKind::terminate()).expect("sigaction");

	loop {
		tokio::select! {
			len = peer.socket().recv(&mut buf) => {
				let buf = &buf[..len.unwrap()];
				let message: RecvMessageClient = serde_cbor::from_slice(&buf)
					.expect("serde_cbor");
				server.handle_request(&mut peer, &message).await.expect("handle_request");
			}
			_ = sigterm.recv() => { 
				break;
			}
		}
	};

	std::process::exit(0);
}

impl Server {
	async fn handle_request(&self, peer: &mut proc::Peer, request: &RecvMessageClient) 
	-> std::io::Result<()> {
		match request {
			RecvMessageClient::Open(open) => return self.handle_open(peer, open).await,
		}
	}

	async fn handle_open(&self, peer: &mut proc::Peer, path: &str)
	-> std::io::Result<()> 
	{
		let response = self.open(path).await;
		let resp: OpenResponse = (&response).into();
		let buf = serde_cbor::to_vec(&resp).expect("serde_cbor");
		eprintln!("sent: {:?}", &buf);
	
		let slice = std::io::IoSlice::new(&buf);
		let mut buf: [u8; 128] = [0; 128];
		let mut writer = AncillaryMessageWriter::new(&mut buf);
		if let Ok(file) = &response {
			let fd = match &file {
				File::File(fd) => fd,
				File::Dir(fd) => fd,
			};
			writer.add_fds(&[fd.as_fd()]).expect("add_fds");
		}
		peer.socket().send_vectored_with_ancillary(&[slice], &mut writer)
			.await.expect("send");
		Ok(())
	}
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Location {
	path: String,
	blocked: bool,
}

impl Location {
	pub fn new(path: String, blocked: bool) -> Self {
		Self { path, blocked }
	}
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Server {
	locations: Vec<Location>,
}

impl Server {
	pub fn new(locations: Vec<Location>) -> Self {
		Self { locations }
	}
}

#[derive(Debug, Serialize, Deserialize)]
pub enum OpenResponse {
	NotFound,
	NotAllowed,
	OtherError,
	File,
	Dir,
}

impl From <&Result<File, FileError>> for OpenResponse {
	fn from(source: &Result<File, FileError>) -> Self {
		match source {
			Ok(File::Dir(_)) => Self::Dir,
			Ok(File::File(_)) => Self::File,
			Err(FileError::NotFound) => Self::NotFound,
			Err(FileError::NotAllowed) => Self::NotAllowed,
			Err(FileError::Io) | Err(FileError::SpecialFile) => Self::OtherError,
		}
	}
}

#[derive(Debug, Serialize, Deserialize)]
enum FileError {
	NotFound,
	NotAllowed,
	SpecialFile,
	Io,
}

impl From<std::io::Error> for FileError {
	fn from(source: std::io::Error) -> Self {
		match source.kind() {
			std::io::ErrorKind::NotFound => Self::NotFound,
			std::io::ErrorKind::PermissionDenied => Self::NotAllowed,
			_ => Self::Io,
		}
	}
}

#[derive(Debug)]
enum File {
	File(OwnedFd),
	Dir(OwnedFd),
}

/* The path has to start with the location
 * longest match wins
 */
impl Server {
	fn matching<'a> (&'a self, path: &str) -> Option<&'a Location> {
		let mut bestlen = 0;
		let mut best = None;
		for (idx, location) in self.locations.iter().enumerate() {
			if let Some(rest) = path.strip_prefix(&location.path) {
				let len = path.len() - rest.len();
				if len > bestlen {
					bestlen = len;
					best = Some(&self.locations[idx]);
				}
			}
		}
		best
	}
	async fn open(&self, path: &str) -> Result<File, FileError> {
		match self.matching(path) {
			None => {
				return Err(FileError::NotAllowed);
			}
			Some(matched) => {
				if matched.blocked {
					return Err(FileError::NotAllowed);
				}
			}
		}
		let file = fs::File::open(path).await?;
		let metadata = file.metadata().await?;
		let fd = OwnedFd::from(file.into_std().await);
		if metadata.is_dir() {
			return Ok(File::Dir(fd));
		}
		else if metadata.is_file() {
			return Ok(File::File(fd));
		}
		else {
			/* This is a character device or something?
			 * not sure how rust handles those
			 */
			return Err(FileError::SpecialFile);
		}
	}
}

#[derive(Debug, Serialize, Deserialize)]
pub enum RecvMessageMain {
	Config(Server),
}

#[derive(Debug, Serialize, Deserialize)]
pub enum RecvMessageClient {
	Open(String),
}

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn matching() {
		let server = Server {
			locations: vec![
				Location { path: "/".to_string(), blocked: false },
				Location { path: "/home/".to_string(), blocked: true },
			],
		};
		let matched = server.matching("/tmp/normalstuff").unwrap();
		assert!(!matched.blocked);
		let matched = server.matching("/home/user/secretstuff").unwrap();
		assert!(matched.blocked);
	}
}
