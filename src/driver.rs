//! `TexDriver`: incremental typesetting of one primary document over snapshot engine processes.
//!
//! The driver runs `<engine_exe> --tex-engine <cache_dir>` (see [`crate::engine`]) and answers its
//! queries over [`crate::protocol`]. Engine processes form a stack: the root and the `fork()`
//! snapshots it and its descendants take while reading the document. Every observation of the
//! document is recorded in a *trace*; an edit rolls back to the newest snapshot that has not seen
//! the changed bytes, places *fences* just before the change so the resumed engine snapshots
//! again right there, and truncates every output to the snapshot's mark.

use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use nix::libc;
use nix::sys::signal::{Signal, kill};
use nix::sys::socket::{AddressFamily, SockFlag, SockType, socketpair};
use nix::unistd::Pid;

use crate::engine::{PRIMARY_INPUT_NAME, STATUS_OUTPUT_NAME};
use crate::protocol::{
    Answer, DriverChannel, ENGINE_CHANNEL_FD, Q_SEEN, Query, QueryBody, STDOUT_FID, set_cloexec,
};

/// Most engine processes (snapshots plus the running one) kept at once.
const MAX_PROCESS: usize = 32;
const MAX_FENCES: usize = 16;
/// How long `engine_step` waits for a pending query.
const ENGINE_POLL: Duration = Duration::from_millis(10);
/// Silence after which a process that has consumed changed bytes is presumed stuck (see
/// [`TexDriver::process_pending_messages`]).
const STUCK_AFTER: Duration = Duration::from_secs(1);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);

const XDV_NAME: &str = "mathnote.xdv";
const SYNCTEX_NAME: &str = "mathnote.synctex.gz";
const LOG_NAME: &str = "mathnote.log";
const STDOUT_NAME: &str = "stdout";

/// `entry->seen` before any observation.
const SEEN_UNSET: i64 = -1;
/// `INT_MAX`: a lookup that failed.
const SEEN_MISSING: i64 = i32::MAX as i64;

pub struct DriverConfig {
    pub engine_exe: PathBuf,
    pub cache_dir: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineState {
    Idle,
    Running,
    Finished,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TexMessage {
    pub error: bool,
    pub message: String,
    pub tex_line: Option<usize>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DriverStats {
    pub roots_spawned: usize,
    pub forks: usize,
    pub rollbacks: usize,
    pub live_processes: usize,
}

/// One engine process on the snapshot stack.
struct Process {
    pid: i32,
    /// `None` once the process has exited or was closed.
    chan: Option<DriverChannel>,
    trace_len: usize,
    snap: Mark,
    killed: bool,
    /// When the driver last received a message from this process.
    last_heard: Instant,
}

/// One observation of an entry: `seen` is the entry's position *before* this observation.
#[derive(Debug, Clone, Copy)]
struct TraceEntry {
    entry: usize,
    seen: i64,
    time: u32,
}

/// A position at which the next read of an entry is answered with `FORK`.
#[derive(Debug, Clone, Copy)]
struct Fence {
    entry: usize,
    position: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutState {
    Unopened,
    Open,
    Closed,
}

/// The primary input, or a named output buffer.
struct Entry {
    input: bool,
    seen: i64,
    /// Identity of `data` (0 = never opened); a reopen starts a new buffer.
    buffer: u64,
    data: Vec<u8>,
    state: OutState,
}

/// Snapshot of the output log: the file table and every output's buffer, length and open
/// state.
#[derive(Clone, Default)]
struct Mark {
    cells: HashMap<i32, usize>,
    outputs: Vec<(u64, usize, OutState)>,
}

/// One change transaction: opened, fed the changed offsets, then closed.
#[derive(Clone, Copy)]
struct Transaction {
    trace_len: usize,
    offset: i64,
    flush: bool,
}

/// A change the top process may already have consumed without reporting it yet: its next
/// message decides (see [`TexDriver::process_pending_messages`]).
#[derive(Clone, Copy)]
struct Verification {
    pid: i32,
    entry: usize,
    changed: i64,
}

/// A protocol violation: the offending process is dropped.
struct Violation;

pub struct TexDriver {
    config: DriverConfig,
    document: Option<Vec<u8>>,
    entries: Vec<Entry>,
    names: HashMap<String, usize>,
    cells: HashMap<i32, usize>,
    retired: HashMap<u64, Vec<u8>>,
    next_buffer: u64,
    processes: Vec<Process>,
    roots: Vec<Child>,
    trace: Vec<TraceEntry>,
    fences: [Fence; MAX_FENCES],
    fence_pos: i32,
    transaction: Option<Transaction>,
    verification: Option<Verification>,
    xdv_truncation: Option<usize>,
    spawn_error: Option<String>,
    stats: DriverStats,
}

const PRIMARY: usize = 0;

impl TexDriver {
    pub fn new(config: DriverConfig) -> Self {
        let mut driver = Self {
            config,
            document: None,
            entries: Vec::new(),
            names: HashMap::new(),
            cells: HashMap::new(),
            retired: HashMap::new(),
            next_buffer: 1,
            processes: Vec::new(),
            roots: Vec::new(),
            trace: Vec::new(),
            fences: [Fence {
                entry: PRIMARY,
                position: 0,
            }; MAX_FENCES],
            fence_pos: -1,
            transaction: None,
            verification: None,
            xdv_truncation: None,
            spawn_error: None,
            stats: DriverStats::default(),
        };
        let primary = driver.entry_for(PRIMARY_INPUT_NAME, true);
        debug_assert_eq!(primary, PRIMARY);
        driver
    }

    /// Replace the primary input. The first changed byte drives the change transaction;
    /// identical bytes are a no-op.
    pub fn set_document(&mut self, source: &[u8]) {
        let Some(old) = self.document.as_deref() else {
            self.document = Some(source.to_vec());
            return;
        };
        let len = old.len().min(source.len());
        let changed = old
            .iter()
            .zip(source)
            .position(|(a, b)| a != b)
            .unwrap_or(len);
        if changed == len && old.len() == source.len() {
            return;
        }

        // Open a change transaction for the primary input and roll back as far as needed.
        self.document = Some(source.to_vec());
        // An unverified earlier change still counts: roll back to the lowest changed offset.
        let changed = match self.take_verification() {
            Some(pending) => pending.changed.min(changed as i64),
            None => changed as i64,
        };
        self.apply_change(PRIMARY, changed);
    }

    /// One change transaction: open it, add the change, close it, place fences, and roll the
    /// processes back.
    fn apply_change(&mut self, entry: usize, changed: i64) {
        self.rollback_begin();
        self.rollback_add_change(entry, changed);
        if let Some((reverted, offset)) = self.rollback_end() {
            let trace = self.compute_fences(reverted, offset);
            self.rollback_processes(reverted, trace);
        }
    }

    /// A deferred change whose process then stays silent for [`STUCK_AFTER`] kills that
    /// process: it may be stuck in a loop, having consumed the changed bytes, and would otherwise
    /// never settle the change.
    fn expire_verification(&mut self) {
        let Some(pending) = self.verification else {
            return;
        };
        let Some(top) = self.processes.last() else {
            self.verification = None;
            return;
        };
        if top.pid != pending.pid || top.chan.is_none() {
            self.verification = None;
            return;
        }
        if top.last_heard.elapsed() < STUCK_AFTER {
            return;
        }
        self.verification = None;
        // Same steps as the kill in `process_pending_messages`, inside a change transaction.
        self.rollback_begin();
        let top = self.top();
        self.close_process(top);
        self.processes[top].killed = true;
        if let Some(transaction) = self.transaction.as_mut() {
            transaction.flush = true;
        }
        self.rollback_add_change(pending.entry, pending.changed);
        if let Some((reverted, offset)) = self.rollback_end() {
            let trace = self.compute_fences(reverted, offset);
            self.rollback_processes(reverted, trace);
        }
    }

    /// The pending verification, if it still belongs to the live top process.
    fn take_verification(&mut self) -> Option<Verification> {
        let pending = self.verification.take()?;
        let top = self.processes.last()?;
        (top.pid == pending.pid && top.chan.is_some()).then_some(pending)
    }

    /// Serve engine queries for up to `budget`; true if outputs or state changed.
    pub fn step(&mut self, budget: Duration) -> bool {
        let deadline = Instant::now() + budget;
        let mut changed = false;
        self.reap();
        while self.document.is_some() {
            if self.processes.is_empty() {
                // engine_step(restart_if_needed = true) → prepare_process.
                self.prepare_process();
                changed = true;
            }
            self.expire_verification();
            let poll = deadline
                .saturating_duration_since(Instant::now())
                .min(ENGINE_POLL);
            if self.engine_step(poll) {
                changed = true;
            } else if self.state() != EngineState::Running {
                break;
            }
            if Instant::now() >= deadline {
                break;
            }
        }
        changed
    }

    pub fn state(&self) -> EngineState {
        if self.document.is_none() {
            return EngineState::Idle;
        }
        match self.processes.last() {
            None => EngineState::Running,
            Some(process) if process.chan.is_some() => EngineState::Running,
            Some(_) => EngineState::Finished,
        }
    }

    pub fn xdv(&self) -> &[u8] {
        self.output(XDV_NAME).map_or(&[], |entry| &entry.data)
    }

    pub fn take_xdv_truncation(&mut self) -> Option<usize> {
        self.xdv_truncation.take()
    }

    pub fn synctex_gz(&self) -> Option<&[u8]> {
        self.output(SYNCTEX_NAME)
            .filter(|entry| entry.state == OutState::Closed)
            .map(|entry| entry.data.as_slice())
    }

    pub fn messages(&self) -> Vec<TexMessage> {
        let mut messages = Vec::new();
        if let Some(error) = &self.spawn_error {
            messages.push(TexMessage {
                error: true,
                message: error.clone(),
                tex_line: None,
            });
        }
        let status = self
            .output(STATUS_OUTPUT_NAME)
            .map(|entry| String::from_utf8_lossy(&entry.data).into_owned())
            .unwrap_or_default();
        for line in status.lines() {
            let Some((kind, message)) = line.split_once('\t') else {
                continue;
            };
            let error = match kind {
                "error" => true,
                "warning" => false,
                _ => continue,
            };
            let message = unescape_status(message);
            let tex_line = extract_tex_line(&message);
            messages.push(TexMessage {
                error,
                message,
                tex_line,
            });
        }

        let log = self
            .output(LOG_NAME)
            .map(|entry| String::from_utf8_lossy(&entry.data).into_owned())
            .unwrap_or_default();
        let mut log_errors = log_error_pairs(&log).into_iter();
        let mut any_error = false;
        for message in messages.iter_mut().filter(|message| message.error) {
            any_error = true;
            if message.tex_line.is_none() {
                message.tex_line = log_errors.next().and_then(|(_, line)| line);
            }
        }
        if !any_error {
            messages.extend(log_errors.map(|(message, tex_line)| TexMessage {
                error: true,
                message,
                tex_line,
            }));
        }
        messages
    }

    pub fn stats(&self) -> DriverStats {
        DriverStats {
            live_processes: self.processes.iter().filter(|p| p.chan.is_some()).count(),
            ..self.stats
        }
    }

    // Output log ----------------------------------------------------------------------------

    fn output(&self, name: &str) -> Option<&Entry> {
        self.names.get(name).map(|&index| &self.entries[index])
    }

    /// Index of the entry for `path`, created on first use.
    fn entry_for(&mut self, path: &str, input: bool) -> usize {
        if let Some(&index) = self.names.get(path) {
            return index;
        }
        self.entries.push(Entry {
            input,
            seen: SEEN_UNSET,
            buffer: 0,
            data: Vec::new(),
            state: OutState::Unopened,
        });
        self.names.insert(path.to_owned(), self.entries.len() - 1);
        self.entries.len() - 1
    }

    /// Mark every output's current state.
    fn snapshot(&self) -> Mark {
        Mark {
            cells: self.cells.clone(),
            outputs: self
                .entries
                .iter()
                .map(|entry| (entry.buffer, entry.data.len(), entry.state))
                .collect(),
        }
    }

    /// Roll outputs back to `mark`: marks nest, so buffers created after `mark` are unreferenced
    /// and dropped.
    fn log_rollback(&mut self, mark: &Mark) {
        self.cells = mark.cells.clone();
        let xdv = self.names.get(XDV_NAME).copied();
        for (index, entry) in self.entries.iter_mut().enumerate() {
            let (buffer, len, state) =
                mark.outputs
                    .get(index)
                    .copied()
                    .unwrap_or((0, 0, OutState::Unopened));
            let old_len = entry.data.len();
            let old_buffer = entry.buffer;
            if entry.buffer != buffer {
                entry.data = self.retired.remove(&buffer).unwrap_or_default();
                entry.buffer = buffer;
            }
            entry.data.truncate(len);
            entry.state = state;
            if Some(index) == xdv && (old_buffer != buffer || entry.data.len() < old_len) {
                let len = entry.data.len();
                self.xdv_truncation = Some(self.xdv_truncation.map_or(len, |t| t.min(len)));
            }
        }
    }

    fn open_output(&mut self, entry: usize) {
        let buffer = self.next_buffer;
        self.next_buffer += 1;
        let is_xdv = self.names.get(XDV_NAME) == Some(&entry);
        let entry = &mut self.entries[entry];
        if entry.buffer != 0 {
            let old = std::mem::take(&mut entry.data);
            self.retired.insert(entry.buffer, old);
            if is_xdv {
                self.xdv_truncation = Some(0);
            }
        }
        entry.buffer = buffer;
        entry.data.clear();
        entry.state = OutState::Open;
    }

    // Processes -------------------------------------------------------------------------------

    fn top(&self) -> usize {
        self.processes.len() - 1
    }

    /// Start the root engine process when the stack is empty.
    fn prepare_process(&mut self) {
        if !self.processes.is_empty() {
            return;
        }
        self.log_rollback(&Mark::default());
        self.retired.clear();
        self.spawn_error = None;
        self.stats.roots_spawned += 1;
        let (pid, chan) = match self.spawn_root() {
            Ok(spawned) => spawned,
            Err(error) => {
                self.spawn_error = Some(format!("could not start the TeX engine: {error}"));
                (0, None)
            }
        };
        self.processes.push(Process {
            pid,
            chan,
            trace_len: 0,
            snap: Mark::default(),
            killed: false,
            last_heard: Instant::now(),
        });
    }

    fn spawn_root(&mut self) -> std::io::Result<(i32, Option<DriverChannel>)> {
        let (driver_end, engine_end) = socketpair(
            AddressFamily::Unix,
            SockType::Stream,
            None,
            SockFlag::empty(),
        )?;
        // Engines started later must not inherit the driver's end.
        set_cloexec(driver_end.as_raw_fd());
        set_cloexec(engine_end.as_raw_fd());
        let engine_fd = engine_end.as_raw_fd();
        let mut command = Command::new(&self.config.engine_exe);
        command
            .arg("--tex-engine")
            .arg(&self.config.cache_dir)
            // macOS: allow Objective-C initialisation in forked snapshots.
            .env("OBJC_DISABLE_INITIALIZE_FORK_SAFETY", "YES")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // SAFETY: only async-signal-safe libc calls between fork and exec.
        unsafe {
            command.pre_exec(move || {
                if engine_fd == ENGINE_CHANNEL_FD {
                    let flags = libc::fcntl(engine_fd, libc::F_GETFD);
                    libc::fcntl(engine_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
                } else if libc::dup2(engine_fd, ENGINE_CHANNEL_FD) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn()?;
        drop(engine_end);
        let pid = child.id() as i32;
        self.roots.push(child);
        let mut chan = DriverChannel::new(driver_end);
        match chan.handshake(HANDSHAKE_TIMEOUT) {
            Ok(()) => Ok((pid, Some(chan))),
            Err(error) => {
                let _ = kill(Pid::from_raw(pid), Signal::SIGTERM);
                Err(error)
            }
        }
    }

    /// Stop a process: SIGTERM and close its channel.
    fn close_process(&mut self, index: usize) {
        let process = &mut self.processes[index];
        if process.chan.take().is_some() {
            let _ = kill(Pid::from_raw(process.pid), Signal::SIGTERM);
        }
    }

    /// Pop the top process and roll outputs back to the parent snapshot's mark, or to empty.
    fn pop_process(&mut self) {
        let top = self.top();
        if self
            .verification
            .is_some_and(|pending| pending.pid == self.processes[top].pid)
        {
            self.verification = None;
        }
        self.close_process(top);
        self.processes.pop();
        let mark = self
            .processes
            .last()
            .map(|process| process.snap.clone())
            .unwrap_or_default();
        self.log_rollback(&mark);
        if self.processes.is_empty() {
            self.retired.clear();
        }
    }

    /// Thin out a full stack, keeping exponentially spaced snapshots.
    fn decimate_processes(&mut self) {
        let count = self.processes.len();
        let mut keep = [false; MAX_PROCESS];
        let mut target = 32usize;
        for (i, process) in self.processes.iter().enumerate() {
            if process.trace_len >= target {
                keep[i] = true;
                target *= 2;
            }
        }

        let mut target = self.processes[count - 1].trace_len as i64;
        let mut delta = 32i64;
        for i in (0..count).rev() {
            let trace_len = self.processes[i].trace_len as i64;
            if trace_len <= target {
                keep[i] = true;
                delta *= 2;
                target -= delta;
            } else if keep[i] {
                delta *= 2;
                target = trace_len - delta;
            }
        }

        for (i, kept) in keep.iter().enumerate().take(count) {
            if !kept {
                self.close_process(i);
            }
        }
        let mut index = 0;
        self.processes.retain(|_| {
            let kept = keep[index];
            index += 1;
            kept
        });
    }

    /// Reap exited roots (forked snapshots are reaped by their engine parents).
    fn reap(&mut self) {
        self.roots
            .retain_mut(|child| matches!(child.try_wait(), Ok(None)));
    }

    // Trace -----------------------------------------------------------------------------------

    /// Record that the top process has seen `entry` up to `seen`.
    fn record_seen(&mut self, entry: usize, seen: i64, time: u32) {
        let count = self.processes.len();
        let trace_len = self.processes[count - 1].trace_len;
        if trace_len > 0
            && self.trace[trace_len - 1].entry == entry
            && (count <= 1 || self.processes[count - 2].trace_len != trace_len)
        {
            self.trace[trace_len - 1].time = time;
            self.entries[entry].seen = seen;
            return;
        }
        self.trace.truncate(trace_len);
        self.trace.push(TraceEntry {
            entry,
            seen: self.entries[entry].seen,
            time,
        });
        self.entries[entry].seen = seen;
        self.processes[count - 1].trace_len += 1;
    }

    fn revert_trace(&mut self, index: usize) {
        let entry = self.trace[index];
        self.entries[entry.entry].seen = entry.seen;
    }

    /// Whether page data follows the XDV preamble.
    fn xdv_output_started(&self) -> bool {
        let xdv = self.xdv();
        xdv.len() > 15 && xdv.len() > 15 + usize::from(xdv[14])
    }

    /// Whether to answer the next read with `FORK`: once 500 ms of engine time have passed since
    /// the last snapshot, and never while fences are pending.
    fn need_snapshot(&self, time: u32) -> bool {
        if self.fence_pos != -1 {
            return false;
        }
        let process = self.top();
        let last_time = if process > 0 {
            let previous = self.processes[process - 1].trace_len;
            if self.processes[process].trace_len == previous {
                return false;
            }
            previous.checked_sub(1).map_or(0, |i| self.trace[i].time)
        } else {
            // macOS cannot load system fonts after fork: delay the first snapshot until output
            // has started, hoping every font is loaded by then.
            if cfg!(target_os = "macos") && !self.xdv_output_started() {
                return false;
            }
            0
        };
        u64::from(time) > 500 + u64::from(last_time)
    }

    /// The bytes served for `entry`: the document for the primary input, else what the engine
    /// wrote.
    fn entry_data(&self, entry: usize) -> &[u8] {
        if entry == PRIMARY {
            self.document.as_deref().unwrap_or_default()
        } else {
            &self.entries[entry].data
        }
    }

    fn cell(&self, fid: i32) -> Result<usize, Violation> {
        self.cells.get(&fid).copied().ok_or(Violation)
    }

    /// Answer one engine query.
    fn answer_query(&mut self, query: Query) -> Result<Option<Answer>, Violation> {
        let time = query.time;
        let answer = match query.body {
            QueryBody::OpenRead { fid, path } => {
                if self.cells.contains_key(&fid) {
                    return Err(Violation);
                }
                let entry = self.entry_for(&path, false);
                let available = if entry == PRIMARY {
                    self.document.is_some()
                } else {
                    self.entries[entry].state != OutState::Unopened
                };
                if !available {
                    // Missing file: record the observation, fail the lookup.
                    self.record_seen(entry, SEEN_MISSING, time);
                    return Ok(Some(Answer::Pass));
                }
                self.cells.insert(fid, entry);
                if self.entries[entry].seen < 0 {
                    self.record_seen(entry, 0, time);
                }
                Answer::Open(path)
            }
            QueryBody::OpenWrite { fid, path } => {
                if self.cells.contains_key(&fid) {
                    return Err(Violation);
                }
                let entry = self.entry_for(&path, false);
                if self.entries[entry].input {
                    return Err(Violation);
                }
                self.cells.insert(fid, entry);
                if self.entries[entry].seen < 0 {
                    self.record_seen(entry, 0, time);
                }
                self.open_output(entry);
                Answer::Open(path)
            }
            QueryBody::Read { fid, pos, size } => {
                let entry = self.cell(fid)?;
                let data_len = self.entry_data(entry).len() as i64;
                let pos = i64::from(pos);
                if pos > data_len {
                    return Err(Violation);
                }
                let mut n = i64::from(size).min(data_len - pos);
                let mut fork = false;
                if self.fence_pos >= 0 {
                    let fence = self.fences[self.fence_pos as usize];
                    if fence.entry == entry && fence.position < pos + n {
                        n = fence.position - pos;
                        if n < 0 {
                            return Err(Violation);
                        }
                        fork = n == 0;
                    }
                }
                if fork {
                    self.fence_pos -= 1;
                    Answer::Fork
                } else if self.need_snapshot(time) {
                    Answer::Fork
                } else {
                    Answer::Read(self.entry_data(entry)[pos as usize..(pos + n) as usize].to_vec())
                }
            }
            QueryBody::Append { fid, data } => {
                let entry = if fid == STDOUT_FID {
                    let entry = self.entry_for(STDOUT_NAME, false);
                    if self.entries[entry].state != OutState::Open {
                        self.open_output(entry);
                    }
                    entry
                } else {
                    self.cell(fid)?
                };
                if self.entries[entry].input || self.entries[entry].state != OutState::Open {
                    return Err(Violation);
                }
                self.entries[entry].data.extend_from_slice(&data);
                Answer::Done
            }
            QueryBody::Close { fid } => {
                let entry = self.cells.remove(&fid).ok_or(Violation)?;
                if !self.entries[entry].input {
                    self.entries[entry].state = OutState::Closed;
                }
                Answer::Done
            }
            QueryBody::Size { fid } => {
                let entry = self.cell(fid)?;
                Answer::Size(self.entry_data(entry).len() as u32)
            }
            QueryBody::Mtime { fid } => {
                self.cell(fid)?;
                Answer::Mtime(0)
            }
            QueryBody::Seen { fid, pos } => {
                let entry = self.cell(fid)?;
                if i64::from(pos) > self.entries[entry].seen {
                    self.record_seen(entry, i64::from(pos), time);
                }
                return Ok(None);
            }
            QueryBody::Child { pid, fd } => {
                if self.processes.len() == MAX_PROCESS {
                    self.decimate_processes();
                }
                let snap = self.snapshot();
                let parent = self.top();
                self.processes[parent].snap = snap;
                let trace_len = self.processes[parent].trace_len;
                self.processes.push(Process {
                    pid,
                    chan: Some(DriverChannel::new(fd)),
                    trace_len,
                    snap: Mark::default(),
                    killed: false,
                    last_heard: Instant::now(),
                });
                self.stats.forks += 1;
                Answer::Done
            }
        };
        Ok(Some(answer))
    }

    /// Answer one query of the process at `index` and flush the answer to its asker.
    fn serve_query(&mut self, index: usize, query: Query) {
        let asker = self.processes[index].pid;
        match self.answer_query(query) {
            Ok(Some(answer)) => {
                if let Some(process) = self.processes.iter_mut().find(|p| p.pid == asker)
                    && let Some(chan) = process.chan.as_mut()
                {
                    chan.write_answer(&answer);
                    if chan.flush().is_err() {
                        process.chan = None;
                    }
                }
            }
            Ok(None) => {}
            Err(Violation) => {
                if let Some(position) = self.processes.iter().position(|p| p.pid == asker) {
                    self.close_process(position);
                }
            }
        }
    }

    /// Serve the top process's next query, waiting up to `poll` for it.
    fn engine_step(&mut self, poll: Duration) -> bool {
        let Some(process) = self.processes.last_mut() else {
            return false;
        };
        let Some(chan) = process.chan.as_mut() else {
            return false;
        };
        if !chan.has_pending(poll) {
            return false;
        }
        match chan.read_query() {
            Ok(query) => {
                process.last_heard = Instant::now();
                let top = self.top();
                // The first message after a deferred change settles it. The engine flushes a
                // coalesced SEEN before any other query, so a SEEN of the changed entry comes
                // first if stale bytes were consumed; anything else means none were.
                let pending = self.take_verification();
                let settles = pending.filter(|pending| {
                    matches!(query.body, QueryBody::Seen { fid, .. }
                        if self.cells.get(&fid) == Some(&pending.entry))
                });
                self.serve_query(top, query);
                if let Some(pending) = settles
                    && self.entries[pending.entry].seen >= pending.changed
                {
                    self.apply_change(pending.entry, pending.changed);
                }
            }
            // EOF: the process is gone; nothing to signal.
            Err(_) => {
                let top = self.top();
                self.processes[top].chan = None;
            }
        }
        true
    }

    // Rollback --------------------------------------------------------------------------------

    /// Pop every process that saw past trace position `trace`, then rewind the trace.
    fn rollback_processes(&mut self, reverted: usize, trace: i64) {
        self.stats.rollbacks += 1;
        while self
            .processes
            .last()
            .is_some_and(|process| process.trace_len as i64 > trace)
        {
            self.pop_process();
        }
        // A dead top is a resume point only if it finished by itself after tracing reads.
        while self.processes.last().is_some_and(|process| {
            process.chan.is_none() && (process.killed || process.trace_len == 0)
        }) {
            self.pop_process();
        }
        let trace_len = self.processes.last().map_or(0, |process| process.trace_len);
        let mut reverted = reverted;
        while reverted > trace_len {
            reverted -= 1;
            self.revert_trace(reverted);
        }
        self.reap();
    }

    fn possible_fence(&self, entry: &TraceEntry) -> bool {
        entry.seen != SEEN_MISSING && entry.seen != SEEN_UNSET && self.entries[entry.entry].input
    }

    /// Place fences before the change at `offset`; returns the trace position processes roll
    /// back to (may be -1).
    fn compute_fences(&mut self, trace: usize, offset: i64) -> i64 {
        self.fence_pos = -1;
        if trace == 0 {
            return 0;
        }
        if self.processes[self.top()].trace_len <= trace {
            return trace as i64;
        }
        self.fence_pos = 0;

        let mut offset = (offset - 64) & !63;
        if offset < self.trace[trace].seen {
            offset = self.trace[trace].seen;
        }
        if offset == -1 {
            offset = 0;
        }
        self.fences[0] = Fence {
            entry: self.trace[trace].entry,
            position: offset,
        };

        let mut delta = 50i64;
        let mut time = i64::from(self.trace[trace].time) - 10;
        let target_trace = self
            .processes
            .iter()
            .rev()
            .find(|process| process.trace_len <= trace)
            .map_or(-1, |process| process.trace_len as i64);
        let mut trace = trace as i64;
        while trace > target_trace && self.fence_pos < (MAX_FENCES - 1) as i32 {
            let entry = self.trace[trace as usize];
            if i64::from(entry.time) <= time && self.possible_fence(&entry) {
                self.fence_pos += 1;
                self.fences[self.fence_pos as usize] = Fence {
                    entry: entry.entry,
                    position: entry.seen.max(0),
                };
                time -= delta;
                delta *= 2;
            }
            trace -= 1;
        }
        trace
    }

    /// Open a change transaction at the top process's trace length.
    fn rollback_begin(&mut self) {
        self.transaction = self.processes.last().map(|process| Transaction {
            trace_len: process.trace_len,
            offset: -1,
            flush: false,
        });
    }

    /// Bring the top process's view up to date before a change at `changed`; false if it may
    /// have observed more than recorded.
    ///
    /// A top process that sends nothing may have consumed changed bytes without reporting them.
    /// Bundle files are read inside the engine, so a root busy with the preamble can stay silent
    /// for a long time, and killing it on every keystroke would restart it forever. Instead the
    /// change is *deferred*: `FLSH` is sent (the engine discards its buffered bytes and re-reads
    /// from its true position) and the process's next message settles it, because the engine
    /// always flushes a pending `SEEN` before any other query (see `engine_step`). Only a process
    /// silent for [`STUCK_AFTER`] is killed, since it is likely stuck in an infinite loop.
    fn process_pending_messages(&mut self, entry: usize, changed: i64) -> bool {
        let Some(transaction) = self.transaction else {
            return true;
        };
        if transaction.flush {
            return true;
        }
        let top = self.top();
        if self.processes[top].chan.is_none() {
            return true;
        }
        // A process that has not opened anything yet cannot have seen stale contents, and is
        // likely still loading its format: don't apply the stuck-worker heuristic to it.
        if self.processes[top].trace_len == 0 {
            return false;
        }

        let mut nothing_seen = true;
        let pending = self.processes[top]
            .chan
            .as_ref()
            .is_some_and(|chan| chan.has_pending(Duration::ZERO));
        if !pending {
            if self.processes[top].last_heard.elapsed() >= STUCK_AFTER {
                // Possibly stuck in an endless computation; resume from the previous snapshot.
                self.close_process(top);
                self.processes[top].killed = true;
            } else {
                self.verification = Some(Verification {
                    pid: self.processes[top].pid,
                    entry,
                    changed,
                });
            }
        } else {
            // Drain pending SEENs to update our view of the process.
            while let Some(chan) = self.processes[top].chan.as_mut() {
                match chan.peek_tag() {
                    Ok(Q_SEEN) => match chan.read_query() {
                        Ok(query) => {
                            self.processes[top].last_heard = Instant::now();
                            self.serve_query(top, query);
                            nothing_seen = false;
                        }
                        Err(_) => {
                            self.processes[top].chan = None;
                            break;
                        }
                    },
                    Ok(_) => break,
                    Err(_) => {
                        self.processes[top].chan = None;
                        break;
                    }
                }
                if !self.processes[top]
                    .chan
                    .as_ref()
                    .is_some_and(|chan| chan.has_pending(Duration::ZERO))
                {
                    break;
                }
            }
        }
        if let Some(transaction) = self.transaction.as_mut() {
            transaction.flush = true;
        }
        nothing_seen
    }

    /// Record a change at `changed` in `entry`: rewind the trace to before the first observation
    /// that saw it.
    fn rollback_add_change(&mut self, entry: usize, changed: i64) {
        let Some(transaction) = self.transaction else {
            return;
        };
        let mut trace_len = transaction.trace_len;
        if self.entries[entry].seen < changed && trace_len == self.processes[self.top()].trace_len {
            // A pending message might update the entry's seen position.
            if self.process_pending_messages(entry, changed) {
                return;
            }
            trace_len = self.processes[self.top()].trace_len;
            if let Some(transaction) = self.transaction.as_mut() {
                transaction.trace_len = trace_len;
            }
        }
        if self.entries[entry].seen < changed {
            return;
        }
        while self.entries[entry].seen >= changed && trace_len > 0 {
            trace_len -= 1;
            self.revert_trace(trace_len);
        }
        debug_assert!(self.trace.get(trace_len).is_none_or(|t| t.entry == entry));
        if let Some(transaction) = self.transaction.as_mut() {
            transaction.trace_len = trace_len;
            transaction.offset = changed;
        }
    }

    /// Close the transaction: `Some((trace, offset))` when processes must roll back.
    fn rollback_end(&mut self) -> Option<(usize, i64)> {
        let transaction = self.transaction.take()?;
        let top = self.top();
        let mut trace_len = transaction.trace_len;
        let mut offset = transaction.offset;
        if trace_len == self.processes[top].trace_len {
            if !transaction.flush {
                return None;
            }
            if let Some(chan) = self.processes[top].chan.as_mut() {
                // The process has not seen the change: drop its buffered bytes.
                chan.write_flush_ask();
                if chan.flush().is_err() {
                    self.processes[top].chan = None;
                }
                return None;
            }
            if trace_len > 0 {
                trace_len -= 1;
                self.revert_trace(trace_len);
            }
            if trace_len > 0 {
                offset = self.trace[trace_len].seen;
            }
        }
        Some((trace_len, offset))
    }
}

impl Drop for TexDriver {
    fn drop(&mut self) {
        for index in 0..self.processes.len() {
            self.close_process(index);
        }
        for child in &mut self.roots {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn unescape_status(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    let mut chars = message.chars();
    while let Some(character) = chars.next() {
        if character != '\\' {
            out.push(character);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// `! message` lines of a TeX log with the `l.<n>` context line that follows them.
fn log_error_pairs(log: &str) -> Vec<(String, Option<usize>)> {
    let mut pairs: Vec<(String, Option<usize>)> = Vec::new();
    for line in log.lines() {
        if let Some(message) = line.strip_prefix("! ") {
            pairs.push((message.trim().to_owned(), None));
        } else if let Some(rest) = line.strip_prefix("l.")
            && let Some(last) = pairs.last_mut()
            && last.1.is_none()
        {
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            last.1 = digits.parse().ok();
        }
    }
    pairs
}

/// A `mathnote.tex:<n>:` prefix, else an `l.<n>` context line (copy of the worker's logic).
fn extract_tex_line(log: &str) -> Option<usize> {
    let mut remainder = log;
    while let Some(index) = remainder.find("mathnote.tex:") {
        remainder = &remainder[index + "mathnote.tex:".len()..];
        let digits: String = remainder
            .chars()
            .take_while(|character| character.is_ascii_digit())
            .collect();
        if let Ok(line) = digits.parse() {
            return Some(line);
        }
    }

    for marker in ["\nl.", " l."] {
        let mut remainder = log;
        while let Some(index) = remainder.find(marker) {
            remainder = &remainder[index + marker.len()..];
            let digits: String = remainder
                .chars()
                .take_while(|character| character.is_ascii_digit())
                .collect();
            if let Ok(line) = digits.parse() {
                return Some(line);
            }
        }
    }
    None
}
