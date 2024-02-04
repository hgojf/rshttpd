use crate::proc;
use proc::{pledge, unveil};
use serde_derive::{Serialize, Deserialize};
use tokio_seqpacket::UnixSeqpacket;
use tokio_seqpacket::ancillary::{OwnedAncillaryMessage};
use std::os::fd::{OwnedFd};
use tokio::signal::unix::{signal, SignalKind};
use tokio::fs;
use std::sync::Arc;

pub async fn main() -> ! {
	pledge("stdio sendfd recvfd rpath unveil", None).expect("pledge");
	let mut parent = unsafe {
		proc::Peer::get_parent()
	};

	let mut buf: [u8; 4096] = [0; 4096];
	let (len, fd) = parent.recv_with_fd(&mut buf).await.expect("read");
	let mut peer = {
		let seq = UnixSeqpacket::try_from(fd).unwrap();
		proc::Peer::from_stream(seq)
	};
	let server: Server = serde_cbor::from_slice(&buf[..len]).expect("idk");
	let server = Arc::new(server);

	for location in &server.locations {
		if location.blocked {
			unveil(&location.path, "").expect("unveil");
		}
		else {
			unveil(&location.path, "r").expect("unveil");
		}
	}
	pledge("stdio sendfd recvfd rpath", None).expect("pledge");

	let mut buf: [u8; 4096] = [0; 4096];

	let mut sigterm = signal(SignalKind::terminate()).expect("sigaction");

	loop {
		tokio::select! {
			resp = peer.recv_with_fd(&mut buf) => {
				let server = server.clone();
				tokio::spawn(async move {
				let (len, stream) = match resp {
					Ok((len, fd)) => {
						let stream = UnixSeqpacket::try_from(fd).unwrap();
						(len, stream)
					}
					Err(err) => {
						eprintln!("error: {err}");
						return;
					}
				};
				let buf = &buf[..len];
				let mut peer = proc::Peer::from_stream(stream);
				let message: RecvMessageClient = serde_cbor::from_slice(&buf)
					.expect("serde_cbor");
				server.handle_request(&mut peer, &message).await.expect("handle_request");
				});
			}
			_ = sigterm.recv() => { 
				break;
			}
		}
	};

	std::process::exit(0);
}

impl Server {
	async fn handle_request(&self, peer: &mut proc::Peer, request: &RecvMessageClient<'_>) 
	-> std::io::Result<()> {
		match request {
			RecvMessageClient::Open(open) => self.handle_open(peer, open).await,
		}
	}

	async fn handle_open(&self, peer: &mut proc::Peer, path: &str)
	-> std::io::Result<()> 
	{
		let mut response = self.open(path).await;
		let resp = match response {
			Err(err) => OpenResponse::FileError(err),
			Ok(File::File(file)) => {
				let file = std::fs::File::from(file);
				let file = tokio::fs::File::from_std(file);
				let info = FileInfo { name: path.to_string() };
				OpenResponse::File(info, file)
			}
			Ok(File::Dir(ref mut dir)) => {
				let mut vec = Vec::new();
				while let Some(entry) = dir.next_entry().await? {
					let name = entry.file_name();
					let name = name.into_string().unwrap();
					vec.push(FileInfo {
						name
					});
				}
				OpenResponse::Dir(vec)
			}
		};
		resp.send(peer).await?;
		Ok(())
	}
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Location {
	path: String,
	blocked: bool,
}

impl Location {
	pub fn new(path: &str, blocked: bool) -> Self {
		Self { path: path.to_string(), blocked }
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
pub struct FileInfo {
	pub name: String,
}

#[derive(Debug)]
pub enum OpenResponse {
	FileError(FileError),
	File(FileInfo, tokio::fs::File),
	Dir(Vec<FileInfo>),
}

#[derive(Debug, Serialize, Deserialize)]
enum SerdeOpenResponse {
	FileError(FileError),
	File(FileInfo),
	Dir(Vec<FileInfo>),
}

impl From<OpenResponse> for SerdeOpenResponse {
	fn from(source: OpenResponse) -> Self {
		match source {
			OpenResponse::FileError(err) => Self::FileError(err),
			OpenResponse::Dir(dir) => Self::Dir(dir),
			OpenResponse::File(info, _) => Self::File(info),
		}
	}
}

impl OpenResponse {
	async fn send(self, writer: &proc::Peer) -> std::io::Result<()> {
		if let OpenResponse::File(info, file) = self {
			let file = file.into_std().await;
			let resp = SerdeOpenResponse::File(info);
			let resp = serde_cbor::to_vec(&resp).unwrap();
			writer.send_with_fd(file, &resp).await?;
		}
		else {
			let resp: SerdeOpenResponse = self.into();
			let resp = serde_cbor::to_vec(&resp).unwrap();
			writer.socket().send(&resp).await?;
		}
		Ok(())
	}
	pub async fn recv(reader: &UnixSeqpacket) -> std::io::Result<Self> {
		let mut buf: [u8; 1024] = [0; 1024];
		let slice = std::io::IoSliceMut::new(&mut buf);
		let mut anbuf = [0u8; 128];
		let (len, ancillary) = reader
			.recv_vectored_with_ancillary(&mut [slice], &mut anbuf)
			.await?;
		let message: SerdeOpenResponse = serde_cbor::from_slice(&buf[..len]).unwrap();
		match message {
			SerdeOpenResponse::Dir(dir) => Ok(Self::Dir(dir)),
			SerdeOpenResponse::FileError(err) => Ok(Self::FileError(err)),
			SerdeOpenResponse::File(file) => {
				let mut messages = ancillary.into_messages();
				let fd = match messages.next() {
					Some(OwnedAncillaryMessage::FileDescriptors(mut fds)) => {
						let fd = fds.next().unwrap();
						let file = std::fs::File::from(fd);
						tokio::fs::File::from_std(file)
					}
					_ => panic!(),
				};
				let file = FileInfo { name: file.name };
				Ok(Self::File(file, fd))
			}
		}
	}
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy)]
pub enum FileError {
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
	Dir(tokio::fs::ReadDir),
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
		let path = std::path::Path::new(path).canonicalize()?;
		let path = path.to_str().expect("Not valid UTF-8?");
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
			/* Under pledge you can't send directory file descriptors,
			 * so this has to do
			 */
			let dir = tokio::fs::read_dir(path).await?;
			Ok(File::Dir(dir))
		}
		else if metadata.is_file() {
			Ok(File::File(fd))
		}
		else {
			/* This is a character device or something?
			 * not sure how rust handles those
			 */
			Err(FileError::SpecialFile)
		}
	}
}

#[derive(Debug, Serialize, Deserialize)]
pub enum RecvMessageClient<'a> {
	Open(&'a str),
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
