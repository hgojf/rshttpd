use crate::proc;
use pledge::pledge;
use unveil::unveil;
use serde_derive::{Serialize, Deserialize};
use tokio_seqpacket::UnixSeqpacket;
use tokio_seqpacket::ancillary::{AncillaryMessageWriter};
use std::os::fd::{AsFd, OwnedFd};
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
			RecvMessageClient::Open(open) => return self.handle_open(peer, open).await,
		}
	}

	async fn handle_open(&self, peer: &mut proc::Peer, path: &str)
	-> std::io::Result<()> 
	{
		let mut response = self.open(path).await;
		let resp = match response {
			Err(err) => OpenResponse::FileError(err),
			Ok(File::File(_, index)) => {
				let name = {
					/* HACK to figure out if this was resolved from a directory */
					if index {
						format!("{path}/index.html")
					}
					else {
						path.to_string()
					}
				};
				let info = FileInfo { name };
				OpenResponse::File(info)
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
		let buf = serde_cbor::to_vec(&resp).expect("serde_cbor");
	
		let slice = std::io::IoSlice::new(&buf);
		let mut buf: [u8; 128] = [0; 128];
		let mut writer = AncillaryMessageWriter::new(&mut buf);
		if let Ok(File::File(fd, _)) = &response {
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

#[derive(Debug, Serialize, Deserialize)]
pub enum OpenResponse {
	FileError(FileError),
	File(FileInfo),
	Dir(Vec<FileInfo>),
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
	File(OwnedFd, bool),
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
			let index = format!("{path}/index.html");
			if let Ok(file) = fs::File::open(index).await {
				let fd = OwnedFd::from(file.into_std().await);
				return Ok(File::File(fd, true))
			}
			let dir = tokio::fs::read_dir(path).await?;
			return Ok(File::Dir(dir));
		}
		else if metadata.is_file() {
			return Ok(File::File(fd, false));
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
