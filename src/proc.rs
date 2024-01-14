use tokio_seqpacket::{ancillary::AncillaryMessageWriter, UnixSeqpacket};
use tokio::process;
use tokio_command_fds::{CommandFdExt, FdMapping};
use std::os::fd::{OwnedFd, AsFd};
use nix::sys::signal;

const PROCESS_FD: std::os::fd::RawFd = 3;

pub struct ProcessBuilder<'a> {
	path: &'a str,
	arg: &'a str,
}

impl <'a> ProcessBuilder<'a> {
	pub fn new(path: &'a str, arg: &'a str) -> Self {
		Self { path, arg }
	}
	pub fn build(self) -> std::io::Result<Process> {
		let (a, socket) = UnixSeqpacket::pair()?;
		let mut command = process::Command::new(self.path);
		command.kill_on_drop(true);
		command.arg(self.arg);
		command.fd_mappings(vec![
			FdMapping {
				parent_fd: a.as_raw_fd(),
				child_fd: PROCESS_FD,
			},
		]).unwrap();
		let child = command.spawn()?;
		Ok(Process {
			peer: Peer { socket },
			child
		})
	}
}

pub struct Process {
	peer: Peer,
	child: process::Child,
}

impl Process {
	pub async fn end(mut self) -> std::io::Result<()> {
		let pid = self.child.id().unwrap();
		let pid: i32 = pid.try_into().unwrap();
		let pid = nix::unistd::Pid::from_raw(pid);
		signal::kill(pid, signal::Signal::SIGTERM).expect("kill");

		self.child.wait().await?;
		Ok(())
	}
	pub fn peer(&mut self) -> &mut Peer {
		&mut self.peer
	}
}

pub struct Peer {
	socket: UnixSeqpacket,
}

impl Peer {
	pub unsafe fn get_parent() -> Self {
		Self {
			socket: UnixSeqpacket::from_raw_fd(PROCESS_FD).expect("from_raw_fd"),
		}
	}
	pub fn from_stream(socket: UnixSeqpacket) -> Self {
		Self { socket }
	}
	pub fn socket(&mut self) -> &mut UnixSeqpacket {
		&mut self.socket
	}
	pub async fn send(&mut self, bytes: &[u8]) -> std::io::Result<()> {
		self.socket.send(bytes).await?;
		Ok(())
	}
	pub async fn send_fd<T> (&mut self, fd: T)
	-> std::io::Result<()> 
	where OwnedFd: From<T>
	{
		let fd = OwnedFd::from(fd);
		let mut buf: [u8; 128] = [0; 128];
		let mut writer = AncillaryMessageWriter::new(&mut buf);
		writer.add_fds(&[fd.as_fd()]).expect("add_fds");
		self.socket.send_vectored_with_ancillary(&[], &mut writer).await?;
		Ok(())
	}
}
