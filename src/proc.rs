use tokio_seqpacket::{ancillary::AncillaryMessageWriter, UnixSeqpacket};
use tokio_seqpacket::ancillary::OwnedAncillaryMessage;
use tokio::process;
use tokio_command_fds::{CommandFdExt, FdMapping};
use std::os::fd::{OwnedFd, AsFd};
use nix::sys::signal;
use nix::unistd::User;

const PROCESS_FD: std::os::fd::RawFd = 3;

pub fn privdrop(root: &str, user: &str) -> std::io::Result<()> {
	let user = User::from_name(user)?
		.ok_or::<std::io::Error>(std::io::ErrorKind::NotFound.into())?;
	nix::unistd::chroot(root).expect("chroot");
	nix::unistd::chdir("/").expect("chdir");
	nix::unistd::setgroups(&[user.gid]).expect("setgroups");
	nix::unistd::setresgid(user.gid, user.gid, user.gid).expect("setgid");
	nix::unistd::setresuid(user.uid, user.uid, user.uid).expect("setuid");
	Ok(())
}

pub struct ProcessBuilder<'a> {
	path: &'a str,
	name: &'a str,
	arg: &'a str,
}

impl <'a> ProcessBuilder<'a> {
	pub fn new(path: &'a str, name: &'a str, arg: &'a str) -> Self {
		Self { path, name, arg }
	}
	pub fn build(self) -> std::io::Result<Process> {
		let (a, socket) = UnixSeqpacket::pair()?;
		let mut command = process::Command::new(self.path);
		command.kill_on_drop(true);
		command.arg0(self.name);
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
	/* This should be safe so long as this process was launched using the
	 * Process struct
	 */
	pub unsafe fn get_parent() -> Self {
		Self {
			socket: UnixSeqpacket::from_raw_fd(PROCESS_FD).expect("from_raw_fd"),
		}
	}
	pub fn from_stream(socket: UnixSeqpacket) -> Self {
		Self { socket }
	}
	pub fn socket(&self) -> &UnixSeqpacket {
		&self.socket
	}
	pub async fn recv_fd(&mut self) -> std::io::Result<OwnedFd> {
		let mut buffer: [u8; 128] = [0; 128];
		let (_, ancillary) = self.socket
			.recv_vectored_with_ancillary(&mut [], &mut buffer).await?;
		let mut messages = ancillary.into_messages();
		let message = messages.next();
		match message {
			Some(OwnedAncillaryMessage::FileDescriptors(mut fds)) => {
				let fd = fds.next()
					.ok_or::<std::io::Error>(std::io::ErrorKind::NotFound.into())?;
				return Ok(fd);
			}
			_ => return Err(std::io::ErrorKind::NotFound.into()),
		}
	}
	pub async fn send(&mut self, bytes: &[u8]) -> std::io::Result<()> {
		self.socket.send(bytes).await?;
		Ok(())
	}
	pub async fn send_fds (&mut self, fds: &[OwnedFd])
	-> std::io::Result<()> 
	{
		let fds: Vec<_> = fds.iter().map(|fd| fd.as_fd()).collect();
		let mut buf: [u8; 128] = [0; 128];
		let mut writer = AncillaryMessageWriter::new(&mut buf);
		writer.add_fds(&fds).expect("add_fds");
		self.socket.send_vectored_with_ancillary(&[], &mut writer).await?;
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
