//! The TeX engine helper process (texpresso-xetex's role), started as
//! `<exe> --tex-engine <cache_dir>` with its driver channel on fd 3.
//!
//! It runs Tectonic's XeTeX through our own [`DriverHooks`] / [`IoProvider`]:
//! - the primary input `mathnote.tex` is read through the driver with `READ`/`SEEN`, and a `FORK`
//!   answer turns the process into a snapshot (texpresso `main.c`, `texpresso_protocol.c`,
//!   `fork.c`);
//! - every output (xdv, synctex.gz, log, aux, stdout) and the status stream `mathnote.status` is
//!   appended to the driver;
//! - every other input is asked from the driver first (it serves outputs written earlier, such
//!   as `mathnote.aux`); bundle files and the format are served locally, read fully into memory on open so no file
//!   offset is shared across `fork`.
//!
//! The process must stay single-threaded because it forks.

use std::cell::RefCell;
use std::fmt::Arguments;
use std::io::{self, Cursor, Read, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime};

use nix::errno::Errno;
use nix::libc;
use nix::sys::socket::{AddressFamily, SockFlag, SockType, socketpair};
use nix::sys::wait::waitpid;
use nix::unistd::{ForkResult, Pid, fork};
use tectonic::TexEngine;
use tectonic::TexOutcome;
use tectonic::io::format_cache::FormatCache;
use tectonic::io::{InputFeatures, InputHandle, InputOrigin, IoProvider, OpenResult, OutputHandle};
use tectonic::status::{MessageKind, StatusBackend};
use tectonic_bridge_core::{CoreBridgeLauncher, DriverHooks};
use tectonic_bundles::Bundle;

use crate::cache;
use crate::protocol::{
    A_DONE, A_FORK, A_MTIM, A_OPEN, A_PASS, A_READ, A_SIZE, C_FLSH, CLIENT_HANDSHAKE,
    ENGINE_CHANNEL_FD, EngineChannel, Q_APND, Q_CLOS, Q_MTIM, Q_OPRD, Q_OPWR, Q_READ, Q_SEEN,
    Q_SIZE, SERVER_HANDSHAKE, STDOUT_FID, read_exact_fd, send_child,
};

/// Name under which the driver serves the primary document.
pub const PRIMARY_INPUT_NAME: &str = "mathnote.tex";
/// Pseudo output carrying `kind\tmessage` status lines.
pub const STATUS_OUTPUT_NAME: &str = "mathnote.status";

/// `BUF_SIZE` of `texpresso_protocol.c`.
const APPEND_BUFFER: usize = 4096;
/// `txp_input::buffer` size (`main.c:137`).
const INPUT_BUFFER: usize = 1024;

/// The driver is gone (EOF / EPIPE): nothing left to do, exit silently.
fn driver_gone() -> ! {
    // SAFETY: `_exit` never returns and skips destructors that could talk to the dead channel.
    unsafe { libc::_exit(0) }
}

/// `txp_client`: the engine side of the protocol.
struct Client {
    chan: EngineChannel,
    generation: u32,
    seen_fid: i32,
    seen_pos: u32,
    append_fid: i32,
    append_buf: Vec<u8>,
    next_fid: i32,
    open_fids: Vec<i32>,
    /// Active engine time. Time blocked on the driver (answers, `waitpid` as a snapshot) is
    /// excluded; this replaces texpresso's patched-in `xetex_tokens` counter.
    active: Duration,
    running_since: Option<Instant>,
}

type Shared = Rc<RefCell<Client>>;

impl Client {
    /// `txp_connect`.
    fn connect(fd: i32) -> Self {
        let mut chan = EngineChannel::new(fd);
        chan.send_raw(CLIENT_HANDSHAKE);
        let mut answer = [0u8; 12];
        if chan.recv_exact(&mut answer).is_err() || &answer != SERVER_HANDSHAKE {
            driver_gone();
        }
        Self {
            chan,
            generation: 0,
            seen_fid: 0,
            seen_pos: 0,
            append_fid: 0,
            append_buf: Vec::with_capacity(APPEND_BUFFER),
            next_fid: 0,
            open_fids: Vec::new(),
            active: Duration::ZERO,
            running_since: Some(Instant::now()),
        }
    }

    fn time(&self) -> u32 {
        let active = self.active + self.running_since.map_or(Duration::ZERO, |t| t.elapsed());
        active.as_millis().min(u128::from(u32::MAX)) as u32
    }

    fn pause(&mut self) {
        if let Some(since) = self.running_since.take() {
            self.active += since.elapsed();
        }
    }

    fn resume(&mut self) {
        self.running_since = Some(Instant::now());
    }

    fn next_fid(&mut self) -> i32 {
        let fid = self.next_fid;
        self.next_fid += 1;
        fid
    }

    fn flush_channel(&mut self) {
        if self.chan.flush().is_err() {
            driver_gone();
        }
    }

    fn recv_u32(&mut self) -> u32 {
        self.pause();
        let value = self.chan.recv_u32().unwrap_or_else(|_| driver_gone());
        self.resume();
        value
    }

    fn recv_exact(&mut self, buf: &mut [u8]) {
        self.pause();
        if self.chan.recv_exact(buf).is_err() {
            driver_gone();
        }
        self.resume();
    }

    /// `txp_io_recv_tag`: every `FLSH` bumps the generation.
    fn recv_tag(&mut self) -> u32 {
        self.pause();
        let (tag, flushes) = self.chan.recv_tag().unwrap_or_else(|_| driver_gone());
        self.resume();
        self.generation = self.generation.wrapping_add(flushes);
        tag
    }

    fn check_done(&mut self) {
        let tag = self.recv_tag();
        if tag != A_DONE {
            driver_gone();
        }
    }

    fn send_tag_raw(&mut self, tag: u32) {
        let time = self.time();
        self.chan.send_u32(tag);
        self.chan.send_u32(time);
    }

    /// `txp_flush_pending`.
    fn flush_pending(&mut self) {
        if self.seen_pos != 0 {
            self.send_tag_raw(Q_SEEN);
            self.chan.send_i32(self.seen_fid);
            self.chan.send_u32(self.seen_pos);
            self.seen_pos = 0;
        }
        if !self.append_buf.is_empty() {
            self.send_tag_raw(Q_APND);
            self.chan.send_i32(self.append_fid);
            let data = std::mem::take(&mut self.append_buf);
            self.chan.send_bytes(&data);
            self.append_buf = data;
            self.append_buf.clear();
            self.check_done();
        }
    }

    /// `txp_io_send_tag`.
    fn send_tag(&mut self, tag: u32) {
        self.flush_pending();
        self.send_tag_raw(tag);
    }

    /// `txp_seen`.
    fn seen(&mut self, fid: i32, pos: u32) {
        if self.seen_fid != fid {
            self.flush_pending();
            self.seen_fid = fid;
        }
        if self.seen_pos < pos {
            self.seen_pos = pos;
        }
    }

    /// `txp_open`.
    fn open(&mut self, fid: i32, path: &str, write: bool) -> Option<String> {
        self.send_tag(if write { Q_OPWR } else { Q_OPRD });
        self.chan.send_i32(fid);
        self.chan.send_bytes(path.as_bytes());
        match self.recv_tag() {
            A_PASS => None,
            A_OPEN => {
                let len = self.recv_u32() as usize;
                let mut name = vec![0; len];
                self.recv_exact(&mut name);
                self.open_fids.push(fid);
                Some(String::from_utf8_lossy(&name).into_owned())
            }
            _ => driver_gone(),
        }
    }

    /// `txp_read` (`texpresso_protocol.c:257-287`): a `FORK` answer forks, then re-asks.
    fn read(&mut self, fid: i32, pos: u32, buf: &mut [u8]) -> usize {
        loop {
            self.send_tag(Q_READ);
            self.chan.send_i32(fid);
            self.chan.send_u32(pos);
            self.chan.send_u32(buf.len() as u32);
            match self.recv_tag() {
                A_FORK => {
                    self.fork();
                    continue;
                }
                A_READ => {
                    let size = self.recv_u32() as usize;
                    if size > buf.len() {
                        driver_gone();
                    }
                    self.recv_exact(&mut buf[..size]);
                    return size;
                }
                _ => driver_gone(),
            }
        }
    }

    /// `txp_append`.
    fn append(&mut self, fid: i32, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        if self.append_fid == fid && self.append_buf.len() + data.len() <= APPEND_BUFFER {
            self.append_buf.extend_from_slice(data);
            return;
        }
        self.flush_pending();
        if data.len() <= APPEND_BUFFER {
            self.append_fid = fid;
            self.append_buf.extend_from_slice(data);
            return;
        }
        self.send_tag(Q_APND);
        self.chan.send_i32(fid);
        self.chan.send_bytes(data);
        self.check_done();
    }

    /// `txp_close`.
    fn close(&mut self, fid: i32) {
        if let Some(index) = self.open_fids.iter().position(|&open| open == fid) {
            self.open_fids.swap_remove(index);
        } else {
            return;
        }
        self.send_tag(Q_CLOS);
        self.chan.send_i32(fid);
        self.check_done();
    }

    fn size(&mut self, fid: i32) -> u32 {
        self.send_tag(Q_SIZE);
        self.chan.send_i32(fid);
        if self.recv_tag() != A_SIZE {
            driver_gone();
        }
        self.recv_u32()
    }

    #[allow(dead_code)]
    fn mtime(&mut self, fid: i32) -> u32 {
        self.send_tag(Q_MTIM);
        self.chan.send_i32(fid);
        if self.recv_tag() != A_MTIM {
            driver_gone();
        }
        self.recv_u32()
    }

    /// `txp_fork` + `texpresso_fork_with_channel` (`fork.c:58-124`).
    fn fork(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.flush_pending();
        self.flush_channel();
        let time = self.time();

        let (parent_end, child_end) = socketpair(
            AddressFamily::Unix,
            SockType::Stream,
            None,
            SockFlag::empty(),
        )
        .unwrap_or_else(|_| driver_gone());

        // SAFETY: the engine process is single-threaded.
        match unsafe { fork() } {
            Err(_) => driver_gone(),
            Ok(ForkResult::Child) => {
                // Replace the channel with the new socket, release the temporaries.
                // SAFETY: plain descriptor duplication onto the channel number.
                if unsafe { libc::dup2(child_end.as_raw_fd(), ENGINE_CHANNEL_FD) } == -1 {
                    driver_gone();
                }
                drop(parent_end);
                drop(child_end);
            }
            Ok(ForkResult::Parent { child }) => {
                if send_child(self.chan.fd(), time, child.as_raw(), parent_end.as_raw_fd()).is_err()
                {
                    driver_gone();
                }
                drop(child_end);
                // Wait for the driver's acknowledgement, ignoring flushes (buffers are empty).
                self.pause();
                loop {
                    let mut answer = [0u8; 4];
                    if read_exact_fd(self.chan.fd(), &mut answer).is_err() {
                        driver_gone();
                    }
                    let tag = u32::from_le_bytes(answer);
                    if tag == C_FLSH {
                        continue;
                    }
                    if tag != A_DONE {
                        driver_gone();
                    }
                    break;
                }
                // Release the driver's end before waiting, so the child sees the driver exit.
                drop(parent_end);
                wait_child(child);
                self.resume();
            }
        }
    }

    /// End of run: flush, close every output, then close the channel before exiting. The driver
    /// treats end of channel as the end of the run, and closing it explicitly lets it see that
    /// immediately instead of after the kernel has torn down this process's address space.
    fn finish(&mut self) -> ! {
        self.flush_pending();
        while let Some(&fid) = self.open_fids.last() {
            self.close(fid);
        }
        self.flush_channel();
        // SAFETY: the channel is not used again; `_exit` follows.
        unsafe { libc::close(ENGINE_CHANNEL_FD) };
        driver_gone()
    }
}

fn wait_child(child: Pid) {
    loop {
        match waitpid(child, None) {
            Err(Errno::EINTR) => {}
            _ => return,
        }
    }
}

/// `txp_input` (`main.c:370-560`): a file served by the driver (the primary document, or an
/// output the engine wrote earlier, such as `mathnote.aux`).
struct TxpInput {
    client: Shared,
    id: i32,
    file_size: Option<u32>,
    file_pos: u32,
    generation: u32,
    buf_pos: usize,
    buf_len: usize,
    buffer: [u8; INPUT_BUFFER],
}

impl Read for TxpInput {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let mut client = self.client.borrow_mut();
        // A new generation (fork, FLSH) discards buffered bytes.
        if self.generation != client.generation {
            self.generation = client.generation;
            self.file_pos += self.buf_pos as u32;
            self.buf_pos = 0;
            self.buf_len = 0;
        }
        if self.buf_pos >= self.buf_len {
            self.file_pos += self.buf_len as u32;
            self.buf_pos = 0;
            self.buf_len = client.read(self.id, self.file_pos, &mut self.buffer);
            // The READ may have forked (generation bump); the fresh buffer is valid.
            self.generation = client.generation;
            if self.buf_len == 0 {
                return Ok(0);
            }
        }
        let count = out.len().min(self.buf_len - self.buf_pos);
        out[..count].copy_from_slice(&self.buffer[self.buf_pos..self.buf_pos + count]);
        self.buf_pos += count;
        client.seen(self.id, self.file_pos + self.buf_pos as u32);
        Ok(count)
    }
}

impl InputFeatures for TxpInput {
    fn get_size(&mut self) -> tectonic::Result<usize> {
        if self.file_size.is_none() {
            self.file_size = Some(self.client.borrow_mut().size(self.id));
        }
        Ok(self.file_size.unwrap_or(0) as usize)
    }

    fn try_seek(&mut self, pos: SeekFrom) -> tectonic::Result<u64> {
        match pos {
            SeekFrom::Start(0) => {
                // `ttstub_input_seek` with ofs 0: buf_pos would be -file_pos, never > 0.
                self.file_pos = 0;
                self.buf_pos = 0;
                self.buf_len = 0;
                Ok(0)
            }
            SeekFrom::Current(0) => Ok(u64::from(self.file_pos) + self.buf_pos as u64),
            _ => Err(io::Error::other("primary input only rewinds to 0").into()),
        }
    }

    fn get_unix_mtime(&mut self) -> tectonic::Result<Option<i64>> {
        Ok(None)
    }
}

impl Drop for TxpInput {
    fn drop(&mut self) {
        self.client.borrow_mut().close(self.id);
    }
}

/// An output whose writes become buffered `APND` messages; dropping it sends `CLOS`.
struct TxpOutput {
    client: Shared,
    fid: i32,
}

impl Write for TxpOutput {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.client.borrow_mut().append(self.fid, buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for TxpOutput {
    fn drop(&mut self) {
        if self.fid != STDOUT_FID {
            self.client.borrow_mut().close(self.fid);
        }
    }
}

fn read_fully(mut handle: InputHandle) -> OpenResult<InputHandle> {
    let name = handle.name().to_owned();
    let origin = handle.origin();
    let mut data = Vec::new();
    match handle.read_to_end(&mut data) {
        Ok(_) => OpenResult::Ok(InputHandle::new(name, Cursor::new(data), origin)),
        Err(error) => OpenResult::Err(error.into()),
    }
}

struct EngineIo {
    client: Shared,
    bundle: Box<dyn Bundle>,
    formats: FormatCache,
}

impl IoProvider for EngineIo {
    fn output_open_name(&mut self, name: &str) -> OpenResult<OutputHandle> {
        let mut client = self.client.borrow_mut();
        let fid = client.next_fid();
        match client.open(fid, name, true) {
            Some(_) => OpenResult::Ok(OutputHandle::new(
                name,
                TxpOutput {
                    client: Rc::clone(&self.client),
                    fid,
                },
            )),
            None => OpenResult::NotAvailable,
        }
    }

    fn output_open_stdout(&mut self) -> OpenResult<OutputHandle> {
        OpenResult::Ok(OutputHandle::new(
            "stdout",
            TxpOutput {
                client: Rc::clone(&self.client),
                fid: STDOUT_FID,
            },
        ))
    }

    /// Ask the driver first (`main.c:226-249`), then the bundle.
    fn input_open_name(
        &mut self,
        name: &str,
        status: &mut dyn StatusBackend,
    ) -> OpenResult<InputHandle> {
        if let Some(handle) = self.open_driver_input(name) {
            return OpenResult::Ok(handle);
        }
        match self.bundle.input_open_name(name, status) {
            OpenResult::Ok(handle) => read_fully(handle),
            other => other,
        }
    }

    fn input_open_primary(&mut self, _status: &mut dyn StatusBackend) -> OpenResult<InputHandle> {
        match self.open_driver_input(PRIMARY_INPUT_NAME) {
            Some(handle) => OpenResult::Ok(handle),
            None => OpenResult::NotAvailable,
        }
    }

    fn input_open_format(
        &mut self,
        name: &str,
        status: &mut dyn StatusBackend,
    ) -> OpenResult<InputHandle> {
        match self.formats.input_open_format(name, status) {
            OpenResult::Ok(handle) => read_fully(handle),
            other => other,
        }
    }
}

impl EngineIo {
    fn open_driver_input(&mut self, name: &str) -> Option<InputHandle> {
        let mut client = self.client.borrow_mut();
        let id = client.next_fid();
        client.open(id, name, false)?;
        let generation = client.generation;
        Some(InputHandle::new(
            name,
            TxpInput {
                client: Rc::clone(&self.client),
                id,
                file_size: None,
                file_pos: 0,
                generation,
                buf_pos: 0,
                buf_len: 0,
                buffer: [0; INPUT_BUFFER],
            },
            InputOrigin::Other,
        ))
    }
}

struct EngineHooks {
    io: EngineIo,
}

impl DriverHooks for EngineHooks {
    fn io(&mut self) -> &mut dyn IoProvider {
        &mut self.io
    }
}

/// Status reports become `kind\tmessage` lines of [`STATUS_OUTPUT_NAME`].
struct EngineStatus {
    client: Shared,
    fid: i32,
}

/// Escape `\`, newline and tab so each message stays one line.
pub(crate) fn escape_status(message: &str) -> String {
    let mut escaped = String::with_capacity(message.len());
    for character in message.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\t' => escaped.push_str("\\t"),
            '\r' => {}
            other => escaped.push(other),
        }
    }
    escaped
}

impl EngineStatus {
    fn line(&mut self, kind: &str, message: &str) {
        let line = format!("{kind}\t{}\n", escape_status(message));
        self.client.borrow_mut().append(self.fid, line.as_bytes());
    }
}

impl StatusBackend for EngineStatus {
    fn report(&mut self, kind: MessageKind, args: Arguments<'_>, err: Option<&tectonic::Error>) {
        let kind = match kind {
            MessageKind::Note => "note",
            MessageKind::Warning => "warning",
            MessageKind::Error => "error",
        };
        let mut message = args.to_string();
        if let Some(error) = err {
            message.push_str(": ");
            message.push_str(&format!("{error:#}"));
        }
        self.line(kind, &message);
    }

    fn dump_error_logs(&mut self, output: &[u8]) {
        self.line("note", &String::from_utf8_lossy(output));
    }
}

fn run_tex(
    client: &Shared,
    status: &mut EngineStatus,
    cache_dir: &Path,
) -> Result<TexOutcome, String> {
    let bundle = cache::open_bundle(cache_dir, true).map_err(|error| error.to_string())?;
    let formats = FormatCache::new(cache::bundle_digest(), cache_dir.join("formats"));
    let mut hooks = EngineHooks {
        io: EngineIo {
            client: Rc::clone(client),
            bundle,
            formats,
        },
    };
    let mut launcher = CoreBridgeLauncher::new(&mut hooks, status);
    let outcome = TexEngine::default()
        .halt_on_error_mode(false)
        .synctex(true)
        .build_date(SystemTime::UNIX_EPOCH)
        .process(&mut launcher, "latex", PRIMARY_INPUT_NAME)
        .map_err(|error| format!("{error:#}"));
    // The process exits right after this run. Freeing the bundle's file index (about 135k
    // entries) would cost 25-35 ms per edit, and in a forked snapshot child every heap page it
    // touches is also copied first.
    std::mem::forget(hooks);
    outcome
}

/// Entry point of `<exe> --tex-engine <cache_dir>`; never returns.
pub fn run_engine_process(cache_dir: &Path) -> ! {
    let client: Shared = Rc::new(RefCell::new(Client::connect(ENGINE_CHANNEL_FD)));
    let fid = client.borrow_mut().next_fid();
    if client
        .borrow_mut()
        .open(fid, STATUS_OUTPUT_NAME, true)
        .is_none()
    {
        client.borrow_mut().finish();
    }
    let mut status = EngineStatus {
        client: Rc::clone(&client),
        fid,
    };
    let outcome = match run_tex(&client, &mut status, cache_dir) {
        Ok(TexOutcome::Spotless) => "spotless",
        Ok(TexOutcome::Warnings) => "warnings",
        Ok(TexOutcome::Errors) => "errors",
        Err(message) => {
            status.line("error", &message);
            "aborted"
        }
    };
    status.line("outcome", outcome);
    drop(status);
    let mut client = client.borrow_mut();
    client.finish()
}
