//! Long-lived helper processes that parse risky formats (PDF, legacy Word).
//!
//! A malformed file can make a parser crash, hang or allocate without bound. Running
//! parsers in separate processes, each with a memory cap and a per-file deadline, keeps
//! that from ever affecting the app. Helpers are reused across files because starting
//! the app binary (which links GTK) for every file would dominate the cost.
//!
//! Protocol over the helper's stdin/stdout, integers little-endian:
//! request `u32 len, path bytes`; response `u8 ok, u32 len, bytes` where bytes are the
//! text when ok, or a [`Failure`] id otherwise.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::extract::{self, ExtractError, Failure, Result, MAX_TEXT_BYTES};

pub const SERVER_ARG: &str = "--extract-server";

/// CPU time one file may take. Measured as CPU rather than wall-clock time, because
/// background helpers only run when the machine is otherwise idle.
const CPU_LIMIT: Duration = Duration::from_secs(45);
/// Backstop for a helper that is stuck without using CPU.
const WALL_LIMIT: Duration = Duration::from_secs(600);
/// Address-space cap per helper; exceeding it makes the allocation fail and the helper exit.
const MEMORY_LIMIT: u64 = 2 << 30;
/// Helpers are replaced after this many files so allocator fragmentation can't accumulate.
const FILES_PER_HELPER: u32 = 200;

pub(crate) struct HelperPool {
    exe: PathBuf,
    /// Background helpers run at idle priority; interactive ones (previews) don't.
    background: bool,
    idle: Mutex<Vec<Helper>>,
}

struct Helper {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    served: u32,
}

impl HelperPool {
    pub(crate) fn new(exe: PathBuf, background: bool) -> HelperPool {
        HelperPool { exe, background, idle: Mutex::new(Vec::new()) }
    }

    pub(crate) fn extract(&self, path: &Path) -> Result<String> {
        let idle = self.idle.lock().unwrap_or_else(|e| e.into_inner()).pop();
        let mut helper = match idle {
            Some(h) => h,
            // Not the file's fault: report it as transient so the file is tried again later.
            None => Helper::spawn(&self.exe, self.background).map_err(|e| ExtractError::transient(e.to_string()))?,
        };
        match helper.request(path) {
            Ok(result) => {
                helper.served += 1;
                if helper.served < FILES_PER_HELPER {
                    self.idle.lock().unwrap_or_else(|e| e.into_inner()).push(helper);
                }
                result
            }
            // The helper is dropped (and killed): its state is unknown after a failed exchange.
            Err(e) if e.kind() == io::ErrorKind::TimedOut => Err(ExtractError::new(Failure::TimedOut, "took too long")),
            Err(e) => Err(ExtractError::new(Failure::Crashed, e.to_string())),
        }
    }

    pub(crate) fn release_idle(&self) {
        self.idle.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

impl Helper {
    fn spawn(exe: &Path, background: bool) -> io::Result<Helper> {
        let mut cmd = Command::new(exe);
        cmd.arg(SERVER_ARG).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        // SAFETY: only async-signal-safe system calls run between fork and exec.
        unsafe {
            cmd.pre_exec(move || {
                let limit = libc::rlimit { rlim_cur: MEMORY_LIMIT, rlim_max: MEMORY_LIMIT };
                libc::setrlimit(libc::RLIMIT_AS, &limit);
                // Tied to the spawning thread, which is why pools live on long-lived threads
                // (or are released after use).
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                if background {
                    crate::priority::lower_current_thread();
                }
                Ok(())
            });
        }
        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        Ok(Helper { child, stdin, stdout, served: 0 })
    }

    /// Returns the helper's answer, or an I/O error if the helper failed or missed the deadline.
    fn request(&mut self, path: &Path) -> io::Result<Result<String>> {
        let bytes = path.as_os_str().as_bytes();
        let mut request = Vec::with_capacity(bytes.len() + 4);
        request.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        request.extend_from_slice(bytes);
        self.stdin.write_all(&request)?;
        self.stdin.flush()?;

        let limits = Limits { pid: self.child.id(), cpu_start: cpu_time(self.child.id()), wall_end: Instant::now() + WALL_LIMIT };
        let mut header = [0u8; 5];
        read_within(&mut self.stdout, &mut header, &limits)?;
        let len = u32::from_le_bytes([header[1], header[2], header[3], header[4]]) as usize;
        if len > MAX_TEXT_BYTES + 64 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "oversized response"));
        }
        let mut payload = vec![0u8; len];
        read_within(&mut self.stdout, &mut payload, &limits)?;
        let text = String::from_utf8_lossy(&payload).into_owned();
        Ok(if header[0] == 1 {
            Ok(text)
        } else {
            Err(ExtractError::new(Failure::from_id(&text), "reported by helper"))
        })
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Limits {
    pid: u32,
    cpu_start: Option<Duration>,
    wall_end: Instant,
}

impl Limits {
    fn exceeded(&self) -> bool {
        let cpu_used = match (cpu_time(self.pid), self.cpu_start) {
            (Some(now), Some(start)) => now.saturating_sub(start),
            _ => Duration::ZERO,
        };
        cpu_used > CPU_LIMIT || Instant::now() > self.wall_end
    }
}

/// User plus system CPU time used so far by process `pid`.
fn cpu_time(pid: u32) -> Option<Duration> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Fields after the parenthesized command name; utime and stime are fields 14 and 15.
    let fields: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    let ticks: u64 = fields.get(11)?.parse::<u64>().ok()? + fields.get(12)?.parse::<u64>().ok()?;
    // SAFETY: sysconf has no preconditions.
    let per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64;
    Some(Duration::from_millis(ticks * 1000 / per_second))
}

/// Fills `buf` from `source`, failing with `TimedOut` once the helper exceeds its limits.
fn read_within(source: &mut ChildStdout, buf: &mut [u8], limits: &Limits) -> io::Result<()> {
    let mut filled = 0;
    while filled < buf.len() {
        if limits.exceeded() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        let mut pollfd = libc::pollfd { fd: source.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: `pollfd` is a valid, exclusively borrowed pollfd array of length 1.
        let ready = unsafe { libc::poll(&mut pollfd, 1, 500) };
        if ready < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if ready == 0 {
            continue;
        }
        match source.read(&mut buf[filled..])? {
            0 => return Err(io::ErrorKind::UnexpectedEof.into()),
            n => filled += n,
        }
    }
    Ok(())
}

/// The helper's main loop; returns the process exit code.
pub fn serve() -> i32 {
    // Libraries sometimes print; that must not corrupt the protocol. Keep the real
    // stdout for responses and point fd 1 at stderr (which is discarded).
    // SAFETY: plain descriptor duplication at startup, before any other thread exists.
    let out_fd = unsafe {
        let fd = libc::dup(1);
        libc::dup2(2, 1);
        fd
    };
    if out_fd < 0 {
        return 3;
    }
    // SAFETY: `out_fd` is a fresh descriptor owned by nothing else.
    let mut out = unsafe { File::from_raw_fd(out_fd) };
    let mut input = io::stdin().lock();
    loop {
        let mut len = [0u8; 4];
        if input.read_exact(&mut len).is_err() {
            return 0; // The app closed the pipe: time to go.
        }
        let mut path = vec![0u8; u32::from_le_bytes(len) as usize];
        if input.read_exact(&mut path).is_err() {
            return 0;
        }
        let path = Path::new(std::ffi::OsStr::from_bytes(&path));
        let result = match extract::kind_for(path) {
            Some(kind) => extract::extract_guarded(path, kind),
            None => Err(ExtractError::new(Failure::Unreadable, "unsupported")),
        };
        let (ok, payload) = match &result {
            Ok(text) => (1u8, text.as_bytes()),
            Err(e) => (0u8, e.failure.id().as_bytes()),
        };
        let mut response = Vec::with_capacity(payload.len() + 5);
        response.push(ok);
        response.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        response.extend_from_slice(payload);
        if out.write_all(&response).and_then(|_| out.flush()).is_err() {
            return 0;
        }
    }
}
