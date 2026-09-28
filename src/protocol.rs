//! Wire format shared by the TeX engine helper (`--tex-engine`) and [`crate::driver`].
//!
//! This mirrors texpresso's `sprotocol.h` / `texpresso_protocol.c`:
//! - the engine sends *queries*: `tag u32` + `time u32` + fields, all little-endian, with
//!   `PACK(a,b,c,d)` tags; strings are `u32 len` + bytes;
//! - the driver replies with *answers* (`DONE`, `PASS`, `SIZE`, `MTIM`, `READ`, `FORK`, `OPEN`)
//!   and may interleave the *ask* `FLSH`, which the engine consumes while waiting for an answer;
//! - `CHLD` carries a child pid plus the child's socket end through `SCM_RIGHTS`.
//!
//! [`EngineChannel`] holds the blocking, unbuffered-read helpers used inside the (forking) engine;
//! [`DriverChannel`] is the buffered, poll-able driver side.

use std::collections::VecDeque;
use std::io::{self, IoSlice, IoSliceMut};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::time::Duration;

use nix::errno::Errno;
use nix::libc;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::socket::{ControlMessage, ControlMessageOwned, MsgFlags, UnixAddr, recvmsg, sendmsg};

/// `PACK(a,b,c,d)` from `sprotocol.h`: the four characters read as a little-endian `u32`.
pub const fn pack(tag: [u8; 4]) -> u32 {
    u32::from_le_bytes(tag)
}

pub const Q_OPRD: u32 = pack(*b"OPRD");
pub const Q_OPWR: u32 = pack(*b"OPWR");
pub const Q_READ: u32 = pack(*b"READ");
pub const Q_APND: u32 = pack(*b"APND");
pub const Q_CLOS: u32 = pack(*b"CLOS");
pub const Q_SIZE: u32 = pack(*b"SIZE");
pub const Q_MTIM: u32 = pack(*b"MTIM");
pub const Q_SEEN: u32 = pack(*b"SEEN");
pub const Q_CHLD: u32 = pack(*b"CHLD");

pub const A_DONE: u32 = pack(*b"DONE");
pub const A_PASS: u32 = pack(*b"PASS");
pub const A_SIZE: u32 = pack(*b"SIZE");
pub const A_MTIM: u32 = pack(*b"MTIM");
pub const A_READ: u32 = pack(*b"READ");
pub const A_FORK: u32 = pack(*b"FORK");
pub const A_OPEN: u32 = pack(*b"OPEN");

pub const C_FLSH: u32 = pack(*b"FLSH");

/// Engine → driver greeting (`txp_connect`).
pub const CLIENT_HANDSHAKE: &[u8; 12] = b"TEXPRESSOC01";
/// Driver → engine greeting.
pub const SERVER_HANDSHAKE: &[u8; 12] = b"TEXPRESSOS01";

/// File id used by appends to the engine's standard output.
pub const STDOUT_FID: i32 = -1;

/// The file descriptor on which a `--tex-engine` process finds its channel.
pub const ENGINE_CHANNEL_FD: RawFd = 3;

#[derive(Debug)]
pub enum QueryBody {
    OpenRead { fid: i32, path: String },
    OpenWrite { fid: i32, path: String },
    Read { fid: i32, pos: u32, size: u32 },
    Append { fid: i32, data: Vec<u8> },
    Close { fid: i32 },
    Size { fid: i32 },
    Mtime { fid: i32 },
    Seen { fid: i32, pos: u32 },
    Child { pid: i32, fd: OwnedFd },
}

#[derive(Debug)]
pub struct Query {
    pub time: u32,
    pub body: QueryBody,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    Done,
    Pass,
    Size(u32),
    Mtime(u32),
    Read(Vec<u8>),
    Fork,
    Open(String),
}

impl Answer {
    pub fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Answer::Done => put_u32(out, A_DONE),
            Answer::Pass => put_u32(out, A_PASS),
            Answer::Size(size) => {
                put_u32(out, A_SIZE);
                put_u32(out, *size);
            }
            Answer::Mtime(mtime) => {
                put_u32(out, A_MTIM);
                put_u32(out, *mtime);
            }
            Answer::Read(data) => {
                put_u32(out, A_READ);
                put_bytes(out, data);
            }
            Answer::Fork => put_u32(out, A_FORK),
            Answer::Open(path) => {
                put_u32(out, A_OPEN);
                put_bytes(out, path.as_bytes());
            }
        }
    }
}

pub fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

pub fn put_i32(out: &mut Vec<u8>, value: i32) {
    out.extend_from_slice(&value.to_le_bytes());
}

pub fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    put_u32(out, bytes.len() as u32);
    out.extend_from_slice(bytes);
}

fn broken_pipe(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, message.to_owned())
}

/// Write every byte, retrying on `EINTR`.
pub fn write_all_fd(fd: BorrowedFd<'_>, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        match nix::unistd::write(fd, bytes) {
            Ok(0) => return Err(broken_pipe("channel closed")),
            Ok(n) => bytes = &bytes[n..],
            Err(Errno::EINTR) => {}
            Err(errno) => return Err(errno.into()),
        }
    }
    Ok(())
}

/// Read exactly `buf.len()` bytes without buffering (safe across `fork`), retrying on `EINTR`.
pub fn read_exact_fd(fd: BorrowedFd<'_>, mut buf: &mut [u8]) -> io::Result<()> {
    while !buf.is_empty() {
        match nix::unistd::read(fd, buf) {
            Ok(0) => return Err(broken_pipe("channel closed")),
            Ok(n) => buf = &mut buf[n..],
            Err(Errno::EINTR) => {}
            Err(errno) => return Err(errno.into()),
        }
    }
    Ok(())
}

/// `send_child_fd` (`fork.c:30-56`): `CHLD` + time + pid, with `child_fd` as `SCM_RIGHTS`.
pub fn send_child(fd: BorrowedFd<'_>, time: u32, pid: i32, child_fd: RawFd) -> io::Result<()> {
    let tag = Q_CHLD.to_le_bytes();
    let time = time.to_le_bytes();
    let pid = pid.to_le_bytes();
    let iov = [IoSlice::new(&tag), IoSlice::new(&time), IoSlice::new(&pid)];
    let fds = [child_fd];
    let cmsg = [ControlMessage::ScmRights(&fds)];
    loop {
        match sendmsg::<UnixAddr>(fd.as_raw_fd(), &iov, &cmsg, MsgFlags::empty(), None) {
            Ok(12) => return Ok(()),
            Ok(_) => return Err(io::Error::other("short CHLD message")),
            Err(Errno::EINTR) => {}
            Err(errno) => return Err(errno.into()),
        }
    }
}

pub fn set_cloexec(fd: RawFd) {
    // SAFETY: fcntl on a descriptor we own; failure only leaves the flag unset.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
        }
    }
}

/// The engine's end of a channel: an outgoing buffer flushed before every blocking read, and
/// unbuffered reads, so no byte of one channel can leak into a forked child's new channel.
pub struct EngineChannel {
    fd: RawFd,
    out: Vec<u8>,
}

impl EngineChannel {
    pub fn new(fd: RawFd) -> Self {
        Self {
            fd,
            out: Vec::with_capacity(8192),
        }
    }

    pub fn fd(&self) -> BorrowedFd<'_> {
        // SAFETY: the channel descriptor stays open for the whole engine process.
        unsafe { BorrowedFd::borrow_raw(self.fd) }
    }

    pub fn send_raw(&mut self, bytes: &[u8]) {
        self.out.extend_from_slice(bytes);
    }

    pub fn send_u32(&mut self, value: u32) {
        put_u32(&mut self.out, value);
    }

    pub fn send_i32(&mut self, value: i32) {
        put_i32(&mut self.out, value);
    }

    pub fn send_bytes(&mut self, bytes: &[u8]) {
        put_bytes(&mut self.out, bytes);
    }

    pub fn flush(&mut self) -> io::Result<()> {
        if self.out.is_empty() {
            return Ok(());
        }
        let result = write_all_fd(self.fd(), &self.out);
        self.out.clear();
        result
    }

    pub fn recv_u32(&mut self) -> io::Result<u32> {
        self.flush()?;
        let mut bytes = [0u8; 4];
        read_exact_fd(self.fd(), &mut bytes)?;
        Ok(u32::from_le_bytes(bytes))
    }

    pub fn recv_exact(&mut self, buf: &mut [u8]) -> io::Result<()> {
        self.flush()?;
        read_exact_fd(self.fd(), buf)
    }

    /// `txp_io_recv_tag`: returns the next answer tag and how many `FLSH` asks preceded it.
    pub fn recv_tag(&mut self) -> io::Result<(u32, u32)> {
        let mut flushes = 0;
        loop {
            let tag = self.recv_u32()?;
            if tag != C_FLSH {
                return Ok((tag, flushes));
            }
            flushes += 1;
        }
    }
}

/// The driver's end of one engine process channel.
pub struct DriverChannel {
    fd: OwnedFd,
    input: Vec<u8>,
    start: usize,
    fds: VecDeque<OwnedFd>,
    out: Vec<u8>,
}

impl DriverChannel {
    pub fn new(fd: OwnedFd) -> Self {
        set_cloexec(fd.as_raw_fd());
        Self {
            fd,
            input: Vec::new(),
            start: 0,
            fds: VecDeque::new(),
            out: Vec::new(),
        }
    }

    fn buffered(&self) -> usize {
        self.input.len() - self.start
    }

    fn poll_readable(&self, timeout: Duration) -> bool {
        let millis = timeout.as_millis().min(i32::MAX as u128) as i32;
        let timeout = PollTimeout::try_from(millis).unwrap_or(PollTimeout::MAX);
        let mut fds = [PollFd::new(self.fd.as_fd(), PollFlags::POLLIN)];
        loop {
            match poll(&mut fds, timeout) {
                Ok(n) => return n > 0,
                Err(Errno::EINTR) => {}
                Err(_) => return true,
            }
        }
    }

    /// Receive one chunk, keeping any `SCM_RIGHTS` descriptors for a later `CHLD`.
    fn fill(&mut self) -> io::Result<()> {
        if self.start > 0 && self.start == self.input.len() {
            self.input.clear();
            self.start = 0;
        }
        let mut chunk = [0u8; 16 * 1024];
        let mut cmsg = nix::cmsg_space!([RawFd; 4]);
        loop {
            let mut iov = [IoSliceMut::new(&mut chunk)];
            match recvmsg::<UnixAddr>(
                self.fd.as_raw_fd(),
                &mut iov,
                Some(&mut cmsg),
                MsgFlags::empty(),
            ) {
                Ok(message) => {
                    let bytes = message.bytes;
                    if let Ok(cmsgs) = message.cmsgs() {
                        for cmsg in cmsgs {
                            if let ControlMessageOwned::ScmRights(fds) = cmsg {
                                for fd in fds {
                                    set_cloexec(fd);
                                    // SAFETY: SCM_RIGHTS installs fresh descriptors we now own.
                                    self.fds.push_back(unsafe { OwnedFd::from_raw_fd(fd) });
                                }
                            }
                        }
                    }
                    if bytes == 0 {
                        return Err(broken_pipe("engine closed its channel"));
                    }
                    self.input.extend_from_slice(&chunk[..bytes]);
                    return Ok(());
                }
                Err(Errno::EINTR) => {}
                Err(errno) => return Err(errno.into()),
            }
        }
    }

    fn need(&mut self, count: usize) -> io::Result<()> {
        while self.buffered() < count {
            self.fill()?;
        }
        Ok(())
    }

    fn take_u32(&mut self) -> io::Result<u32> {
        self.need(4)?;
        let bytes: [u8; 4] = self.input[self.start..self.start + 4].try_into().unwrap();
        self.start += 4;
        Ok(u32::from_le_bytes(bytes))
    }

    fn take_i32(&mut self) -> io::Result<i32> {
        Ok(self.take_u32()? as i32)
    }

    fn take_bytes(&mut self) -> io::Result<Vec<u8>> {
        let len = self.take_u32()? as usize;
        self.need(len)?;
        let bytes = self.input[self.start..self.start + len].to_vec();
        self.start += len;
        Ok(bytes)
    }

    fn take_string(&mut self) -> io::Result<String> {
        String::from_utf8(self.take_bytes()?)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "path is not UTF-8"))
    }

    /// `channel_handshake`: wait for the engine greeting and answer it.
    pub fn handshake(&mut self, timeout: Duration) -> io::Result<()> {
        while self.buffered() < CLIENT_HANDSHAKE.len() {
            if !self.poll_readable(timeout) {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "engine handshake timed out",
                ));
            }
            self.fill()?;
        }
        if &self.input[self.start..self.start + 12] != CLIENT_HANDSHAKE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid engine handshake",
            ));
        }
        self.start += 12;
        self.out.extend_from_slice(SERVER_HANDSHAKE);
        self.flush()
    }

    /// `channel_has_pending_query`: bytes are buffered, or the socket becomes readable (data or
    /// EOF) within `timeout`.
    pub fn has_pending(&self, timeout: Duration) -> bool {
        self.buffered() > 0 || self.poll_readable(timeout)
    }

    /// `channel_peek_query`: the tag of the next query, without consuming it.
    pub fn peek_tag(&mut self) -> io::Result<u32> {
        self.need(4)?;
        Ok(u32::from_le_bytes(
            self.input[self.start..self.start + 4].try_into().unwrap(),
        ))
    }

    /// `channel_read_query`.
    pub fn read_query(&mut self) -> io::Result<Query> {
        let tag = self.take_u32()?;
        let time = self.take_u32()?;
        let body = match tag {
            Q_OPRD => QueryBody::OpenRead {
                fid: self.take_i32()?,
                path: self.take_string()?,
            },
            Q_OPWR => QueryBody::OpenWrite {
                fid: self.take_i32()?,
                path: self.take_string()?,
            },
            Q_READ => QueryBody::Read {
                fid: self.take_i32()?,
                pos: self.take_u32()?,
                size: self.take_u32()?,
            },
            Q_APND => QueryBody::Append {
                fid: self.take_i32()?,
                data: self.take_bytes()?,
            },
            Q_CLOS => QueryBody::Close {
                fid: self.take_i32()?,
            },
            Q_SIZE => QueryBody::Size {
                fid: self.take_i32()?,
            },
            Q_MTIM => QueryBody::Mtime {
                fid: self.take_i32()?,
            },
            Q_SEEN => QueryBody::Seen {
                fid: self.take_i32()?,
                pos: self.take_u32()?,
            },
            Q_CHLD => {
                let pid = self.take_i32()?;
                let fd = self.fds.pop_front().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "CHLD without a descriptor")
                })?;
                QueryBody::Child { pid, fd }
            }
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown query tag {:?}", other.to_le_bytes()),
                ));
            }
        };
        Ok(Query { time, body })
    }

    pub fn write_answer(&mut self, answer: &Answer) {
        answer.encode(&mut self.out);
    }

    pub fn write_flush_ask(&mut self) {
        put_u32(&mut self.out, C_FLSH);
    }

    pub fn flush(&mut self) -> io::Result<()> {
        if self.out.is_empty() {
            return Ok(());
        }
        let result = write_all_fd(self.fd.as_fd(), &self.out);
        self.out.clear();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::sys::socket::{AddressFamily, SockFlag, SockType, socketpair};

    fn pair() -> (EngineChannel, OwnedFd, DriverChannel) {
        let (a, b) = socketpair(
            AddressFamily::Unix,
            SockType::Stream,
            None,
            SockFlag::empty(),
        )
        .unwrap();
        let engine = EngineChannel::new(a.as_raw_fd());
        (engine, a, DriverChannel::new(b))
    }

    #[test]
    fn queries_round_trip_and_flush_asks_are_skipped_by_the_engine() {
        let (mut engine, _keep, mut driver) = pair();
        engine.send_u32(Q_OPWR);
        engine.send_u32(7);
        engine.send_i32(2);
        engine.send_bytes(b"mathnote.xdv");
        engine.send_u32(Q_SEEN);
        engine.send_u32(9);
        engine.send_i32(0);
        engine.send_u32(123);
        engine.flush().unwrap();

        assert!(driver.has_pending(Duration::from_millis(100)));
        let query = driver.read_query().unwrap();
        assert_eq!(query.time, 7);
        assert!(
            matches!(query.body, QueryBody::OpenWrite { fid: 2, ref path } if path == "mathnote.xdv")
        );
        assert_eq!(driver.peek_tag().unwrap(), Q_SEEN);
        let query = driver.read_query().unwrap();
        assert!(matches!(query.body, QueryBody::Seen { fid: 0, pos: 123 }));

        driver.write_flush_ask();
        driver.write_answer(&Answer::Read(b"abc".to_vec()));
        driver.flush().unwrap();
        assert_eq!(engine.recv_tag().unwrap(), (A_READ, 1));
        let size = engine.recv_u32().unwrap();
        let mut data = vec![0; size as usize];
        engine.recv_exact(&mut data).unwrap();
        assert_eq!(data, b"abc");
    }

    #[test]
    fn child_descriptors_travel_with_their_chld_query() {
        let (engine, _keep, mut driver) = pair();
        let (x, y) = socketpair(
            AddressFamily::Unix,
            SockType::Stream,
            None,
            SockFlag::empty(),
        )
        .unwrap();
        send_child(engine.fd(), 5, 4242, x.as_raw_fd()).unwrap();
        drop(x);
        let query = driver.read_query().unwrap();
        assert_eq!(query.time, 5);
        let QueryBody::Child { pid, fd } = query.body else {
            panic!("expected CHLD");
        };
        assert_eq!(pid, 4242);
        write_all_fd(fd.as_fd(), b"ping").unwrap();
        let mut got = [0u8; 4];
        read_exact_fd(y.as_fd(), &mut got).unwrap();
        assert_eq!(&got, b"ping");
    }
}
