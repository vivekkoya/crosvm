// Copyright 2017 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Small system utility modules for usage by other modules.

#[cfg(target_os = "android")]
mod android;
#[cfg(target_os = "android")]
use android as target_os;
#[cfg(target_os = "linux")]
#[allow(clippy::module_inception)]
mod linux;
#[cfg(target_os = "linux")]
use linux as target_os;
use log::warn;
#[macro_use]
pub mod ioctl;
#[macro_use]
pub mod syslog;
mod acpi_event;
mod capabilities;
mod descriptor;
mod event;
mod file;
mod file_traits;
mod get_filesystem_type;
mod mmap;
mod net;
mod netlink;
mod notifiers;
pub mod panic_handler;
pub mod platform_timer_resolution;
mod poll;
mod priority;
pub mod process;
mod sched;
mod shm;
pub mod signal;
mod signalfd;
mod terminal;
mod timer;
pub mod vsock;
mod write_zeroes;

use std::fs::remove_file;
use std::fs::File;
use std::fs::OpenOptions;
use std::mem;
use std::mem::MaybeUninit;
use std::ops::Deref;
use std::os::unix::io::FromRawFd;
use std::os::unix::io::RawFd;
use std::os::unix::net::UnixDatagram;
use std::os::unix::net::UnixListener;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::ptr;
use std::time::Duration;

pub use acpi_event::*;
pub use capabilities::drop_capabilities;
pub use descriptor::*;
pub use event::EventExt;
pub(crate) use event::PlatformEvent;
pub use file::find_next_data;
pub use file::FileDataIterator;
pub(crate) use file_traits::lib::*;
pub use get_filesystem_type::*;
pub use ioctl::*;
use libc::c_int;
use libc::c_long;
use libc::fcntl;
use libc::pipe2;
use libc::syscall;
use libc::waitpid;
use libc::SYS_getpid;
use libc::SYS_getppid;
use libc::SYS_gettid;
use libc::EINVAL;
use libc::O_CLOEXEC;
use libc::SIGKILL;
use libc::WNOHANG;
pub use mmap::*;
pub(in crate::sys) use net::sendmsg_nosignal as sendmsg;
pub(in crate::sys) use net::sockaddr_un;
pub(in crate::sys) use net::sockaddrv4_to_lib_c;
pub(in crate::sys) use net::sockaddrv6_to_lib_c;
pub use netlink::*;
use once_cell::sync::OnceCell;
pub use poll::EventContext;
pub use priority::*;
pub use sched::*;
pub use shm::MemfdSeals;
pub use shm::SharedMemoryLinux;
pub use signal::*;
pub use signalfd::Error as SignalFdError;
pub use signalfd::*;
pub use terminal::*;
pub use timer::*;
pub(crate) use write_zeroes::file_punch_hole;
pub(crate) use write_zeroes::file_write_zeroes_at;

use crate::descriptor::FromRawDescriptor;
use crate::descriptor::SafeDescriptor;
pub use crate::errno::Error;
pub use crate::errno::Result;
pub use crate::errno::*;
use crate::number_of_logical_cores;
use crate::round_up_to_page_size;
pub use crate::sys::unix::descriptor::*;
use crate::syscall;
use crate::AsRawDescriptor;
use crate::Pid;

/// Re-export libc types that are part of the API.
pub type Uid = libc::uid_t;
pub type Gid = libc::gid_t;
pub type Mode = libc::mode_t;

/// This bypasses `libc`'s caching `getpid(2)` wrapper which can be invalid if a raw clone was used
/// elsewhere.
#[inline(always)]
pub fn getpid() -> Pid {
    // SAFETY:
    // Safe because this syscall can never fail and we give it a valid syscall number.
    unsafe { syscall(SYS_getpid as c_long) as Pid }
}

/// Safe wrapper for the geppid Linux systemcall.
#[inline(always)]
pub fn getppid() -> Pid {
    // SAFETY:
    // Safe because this syscall can never fail and we give it a valid syscall number.
    unsafe { syscall(SYS_getppid as c_long) as Pid }
}

/// Safe wrapper for the gettid Linux systemcall.
pub fn gettid() -> Pid {
    // SAFETY:
    // Calling the gettid() sycall is always safe.
    unsafe { syscall(SYS_gettid as c_long) as Pid }
}

/// Safe wrapper for `geteuid(2)`.
#[inline(always)]
pub fn geteuid() -> Uid {
    // SAFETY:
    // trivially safe
    unsafe { libc::geteuid() }
}

/// Safe wrapper for `getegid(2)`.
#[inline(always)]
pub fn getegid() -> Gid {
    // SAFETY:
    // trivially safe
    unsafe { libc::getegid() }
}

/// The operation to perform with `flock`.
pub enum FlockOperation {
    LockShared,
    LockExclusive,
    Unlock,
}

/// Safe wrapper for flock(2) with the operation `op` and optionally `nonblocking`. The lock will be
/// dropped automatically when `file` is dropped.
#[inline(always)]
pub fn flock<F: AsRawDescriptor>(file: &F, op: FlockOperation, nonblocking: bool) -> Result<()> {
    let mut operation = match op {
        FlockOperation::LockShared => libc::LOCK_SH,
        FlockOperation::LockExclusive => libc::LOCK_EX,
        FlockOperation::Unlock => libc::LOCK_UN,
    };

    if nonblocking {
        operation |= libc::LOCK_NB;
    }

    // SAFETY:
    // Safe since we pass in a valid fd and flock operation, and check the return value.
    syscall!(unsafe { libc::flock(file.as_raw_descriptor(), operation) }).map(|_| ())
}

/// The operation to perform with `fallocate`.
pub enum FallocateMode {
    PunchHole,
    ZeroRange,
    Allocate,
}

impl From<FallocateMode> for i32 {
    fn from(value: FallocateMode) -> Self {
        match value {
            FallocateMode::Allocate => libc::FALLOC_FL_KEEP_SIZE,
            FallocateMode::PunchHole => libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
            FallocateMode::ZeroRange => libc::FALLOC_FL_ZERO_RANGE | libc::FALLOC_FL_KEEP_SIZE,
        }
    }
}

impl From<FallocateMode> for u32 {
    fn from(value: FallocateMode) -> Self {
        Into::<i32>::into(value) as u32
    }
}

/// Safe wrapper for `fallocate()`.
pub fn fallocate<F: AsRawDescriptor>(
    file: &F,
    mode: FallocateMode,
    offset: u64,
    len: u64,
) -> Result<()> {
    let offset = if offset > libc::off64_t::max_value() as u64 {
        return Err(Error::new(libc::EINVAL));
    } else {
        offset as libc::off64_t
    };

    let len = if len > libc::off64_t::max_value() as u64 {
        return Err(Error::new(libc::EINVAL));
    } else {
        len as libc::off64_t
    };

    // SAFETY:
    // Safe since we pass in a valid fd and fallocate mode, validate offset and len,
    // and check the return value.
    syscall!(unsafe { libc::fallocate64(file.as_raw_descriptor(), mode.into(), offset, len) })
        .map(|_| ())
}

/// Safe wrapper for `fstat()`.
pub fn fstat<F: AsRawDescriptor>(f: &F) -> Result<libc::stat64> {
    let mut st = MaybeUninit::<libc::stat64>::zeroed();

    // SAFETY:
    // Safe because the kernel will only write data in `st` and we check the return
    // value.
    syscall!(unsafe { libc::fstat64(f.as_raw_descriptor(), st.as_mut_ptr()) })?;

    // SAFETY:
    // Safe because the kernel guarantees that the struct is now fully initialized.
    Ok(unsafe { st.assume_init() })
}

/// Checks whether a file is a block device fie or not.
pub fn is_block_file<F: AsRawDescriptor>(file: &F) -> Result<bool> {
    let stat = fstat(file)?;
    Ok((stat.st_mode & libc::S_IFMT) == libc::S_IFBLK)
}

const BLOCK_IO_TYPE: u32 = 0x12;
ioctl_io_nr!(BLKDISCARD, BLOCK_IO_TYPE, 119);

/// Discards the given range of a block file.
pub fn discard_block<F: AsRawDescriptor>(file: &F, offset: u64, len: u64) -> Result<()> {
    let range: [u64; 2] = [offset, len];
    // SAFETY:
    // Safe because
    // - we check the return value.
    // - ioctl(BLKDISCARD) does not hold the descriptor after the call.
    // - ioctl(BLKDISCARD) does not break the file descriptor.
    // - ioctl(BLKDISCARD) does not modify the given range.
    syscall!(unsafe { libc::ioctl(file.as_raw_descriptor(), BLKDISCARD(), &range) }).map(|_| ())
}

/// A trait used to abstract types that provide a process id that can be operated on.
pub trait AsRawPid {
    fn as_raw_pid(&self) -> Pid;
}

impl AsRawPid for Pid {
    fn as_raw_pid(&self) -> Pid {
        *self
    }
}

impl AsRawPid for std::process::Child {
    fn as_raw_pid(&self) -> Pid {
        self.id() as Pid
    }
}

/// A safe wrapper around waitpid.
///
/// On success if a process was reaped, it will be returned as the first value.
/// The second returned value is the ExitStatus from the libc::waitpid() call.
///
/// Note: this can block if libc::WNOHANG is not set and EINTR is not handled internally.
pub fn wait_for_pid<A: AsRawPid>(pid: A, options: c_int) -> Result<(Option<Pid>, ExitStatus)> {
    let pid = pid.as_raw_pid();
    let mut status: c_int = 1;
    // SAFETY:
    // Safe because status is owned and the error is checked.
    let ret = unsafe { libc::waitpid(pid, &mut status, options) };
    if ret < 0 {
        return errno_result();
    }
    Ok((
        if ret == 0 { None } else { Some(ret) },
        ExitStatus::from_raw(status),
    ))
}

/// Reaps a child process that has terminated.
///
/// Returns `Ok(pid)` where `pid` is the process that was reaped or `Ok(0)` if none of the children
/// have terminated. An `Error` is with `errno == ECHILD` if there are no children left to reap.
///
/// # Examples
///
/// Reaps all child processes until there are no terminated children to reap.
///
/// ```
/// fn reap_children() {
///     loop {
///         match base::linux::reap_child() {
///             Ok(0) => println!("no children ready to reap"),
///             Ok(pid) => {
///                 println!("reaped {}", pid);
///                 continue
///             },
///             Err(e) if e.errno() == libc::ECHILD => println!("no children left"),
///             Err(e) => println!("error reaping children: {}", e),
///         }
///         break
///     }
/// }
/// ```
pub fn reap_child() -> Result<Pid> {
    // SAFETY:
    // Safe because we pass in no memory, prevent blocking with WNOHANG, and check for error.
    let ret = unsafe { waitpid(-1, ptr::null_mut(), WNOHANG) };
    if ret == -1 {
        errno_result()
    } else {
        Ok(ret)
    }
}

/// Kill all processes in the current process group.
///
/// On success, this kills all processes in the current process group, including the current
/// process, meaning this will not return. This is equivalent to a call to `kill(0, SIGKILL)`.
pub fn kill_process_group() -> Result<()> {
    // SAFETY: Safe because pid is 'self group' and return value doesn't matter.
    unsafe { kill(0, SIGKILL) }?;
    // Kill succeeded, so this process never reaches here.
    unreachable!();
}

/// Spawns a pipe pair where the first pipe is the read end and the second pipe is the write end.
///
/// If `close_on_exec` is true, the `O_CLOEXEC` flag will be set during pipe creation.
pub fn pipe(close_on_exec: bool) -> Result<(File, File)> {
    let flags = if close_on_exec { O_CLOEXEC } else { 0 };
    let mut pipe_fds = [-1; 2];
    // SAFETY:
    // Safe because pipe2 will only write 2 element array of i32 to the given pointer, and we check
    // for error.
    let ret = unsafe { pipe2(&mut pipe_fds[0], flags) };
    if ret == -1 {
        errno_result()
    } else {
        // SAFETY:
        // Safe because both fds must be valid for pipe2 to have returned sucessfully and we have
        // exclusive ownership of them.
        Ok(unsafe {
            (
                File::from_raw_fd(pipe_fds[0]),
                File::from_raw_fd(pipe_fds[1]),
            )
        })
    }
}

/// Sets the pipe signified with fd to `size`.
///
/// Returns the new size of the pipe or an error if the OS fails to set the pipe size.
pub fn set_pipe_size(fd: RawFd, size: usize) -> Result<usize> {
    // SAFETY:
    // Safe because fcntl with the `F_SETPIPE_SZ` arg doesn't touch memory.
    syscall!(unsafe { fcntl(fd, libc::F_SETPIPE_SZ, size as c_int) }).map(|ret| ret as usize)
}

/// Test-only function used to create a pipe that is full. The pipe is created, has its size set to
/// the minimum and then has that much data written to it. Use `new_pipe_full` to test handling of
/// blocking `write` calls in unit tests.
pub fn new_pipe_full() -> Result<(File, File)> {
    use std::io::Write;

    let (rx, mut tx) = pipe(true)?;
    // The smallest allowed size of a pipe is the system page size on linux.
    let page_size = set_pipe_size(tx.as_raw_descriptor(), round_up_to_page_size(1))?;

    // Fill the pipe with page_size zeros so the next write call will block.
    let buf = vec![0u8; page_size];
    tx.write_all(&buf)?;

    Ok((rx, tx))
}

/// Used to attempt to clean up a named pipe after it is no longer used.
pub struct UnlinkUnixDatagram(pub UnixDatagram);
impl AsRef<UnixDatagram> for UnlinkUnixDatagram {
    fn as_ref(&self) -> &UnixDatagram {
        &self.0
    }
}
impl Drop for UnlinkUnixDatagram {
    fn drop(&mut self) {
        if let Ok(addr) = self.0.local_addr() {
            if let Some(path) = addr.as_pathname() {
                if let Err(e) = remove_file(path) {
                    warn!("failed to remove control socket file: {}", e);
                }
            }
        }
    }
}

/// Used to attempt to clean up a named pipe after it is no longer used.
pub struct UnlinkUnixListener(pub UnixListener);

impl AsRef<UnixListener> for UnlinkUnixListener {
    fn as_ref(&self) -> &UnixListener {
        &self.0
    }
}

impl Deref for UnlinkUnixListener {
    type Target = UnixListener;

    fn deref(&self) -> &UnixListener {
        &self.0
    }
}

impl Drop for UnlinkUnixListener {
    fn drop(&mut self) {
        if let Ok(addr) = self.0.local_addr() {
            if let Some(path) = addr.as_pathname() {
                if let Err(e) = remove_file(path) {
                    warn!("failed to remove control socket file: {}", e);
                }
            }
        }
    }
}

/// Verifies that |raw_descriptor| is actually owned by this process and duplicates it
/// to ensure that we have a unique handle to it.
pub fn validate_raw_descriptor(raw_descriptor: RawDescriptor) -> Result<RawDescriptor> {
    validate_raw_fd(raw_descriptor)
}

/// Verifies that |raw_fd| is actually owned by this process and duplicates it to ensure that
/// we have a unique handle to it.
pub fn validate_raw_fd(raw_fd: RawFd) -> Result<RawFd> {
    // Checking that close-on-exec isn't set helps filter out FDs that were opened by
    // crosvm as all crosvm FDs are close on exec.
    // SAFETY:
    // Safe because this doesn't modify any memory and we check the return value.
    let flags = unsafe { libc::fcntl(raw_fd, libc::F_GETFD) };
    if flags < 0 || (flags & libc::FD_CLOEXEC) != 0 {
        return Err(Error::new(libc::EBADF));
    }

    // SAFETY:
    // Duplicate the fd to ensure that we don't accidentally close an fd previously
    // opened by another subsystem.  Safe because this doesn't modify any memory and
    // we check the return value.
    let dup_fd = unsafe { libc::fcntl(raw_fd, libc::F_DUPFD_CLOEXEC, 0) };
    if dup_fd < 0 {
        return Err(Error::last());
    }
    Ok(dup_fd as RawFd)
}

/// Utility function that returns true if the given FD is readable without blocking.
///
/// On an error, such as an invalid or incompatible FD, this will return false, which can not be
/// distinguished from a non-ready to read FD.
pub fn poll_in<F: AsRawDescriptor>(fd: &F) -> bool {
    let mut fds = libc::pollfd {
        fd: fd.as_raw_descriptor(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY:
    // Safe because we give a valid pointer to a list (of 1) FD and check the return value.
    let ret = unsafe { libc::poll(&mut fds, 1, 0) };
    // An error probably indicates an invalid FD, or an FD that can't be polled. Returning false in
    // that case is probably correct as such an FD is unlikely to be readable, although there are
    // probably corner cases in which that is wrong.
    if ret == -1 {
        return false;
    }
    fds.revents & libc::POLLIN != 0
}

/// Return a timespec filed with the specified Duration `duration`.
#[allow(clippy::useless_conversion)]
pub fn duration_to_timespec(duration: Duration) -> libc::timespec {
    // nsec always fits in i32 because subsec_nanos is defined to be less than one billion.
    let nsec = duration.subsec_nanos() as i32;
    libc::timespec {
        tv_sec: duration.as_secs() as libc::time_t,
        tv_nsec: nsec.into(),
    }
}

/// Return the maximum Duration that can be used with libc::timespec.
pub fn max_timeout() -> Duration {
    Duration::new(libc::time_t::max_value() as u64, 999999999)
}

/// If the given path is of the form /proc/self/fd/N for some N, returns `Ok(Some(N))`. Otherwise
/// returns `Ok(None)`.
pub fn safe_descriptor_from_path<P: AsRef<Path>>(path: P) -> Result<Option<SafeDescriptor>> {
    let path = path.as_ref();
    if path.parent() == Some(Path::new("/proc/self/fd")) {
        let raw_descriptor = path
            .file_name()
            .and_then(|fd_osstr| fd_osstr.to_str())
            .and_then(|fd_str| fd_str.parse::<RawFd>().ok())
            .ok_or_else(|| Error::new(EINVAL))?;
        let validated_fd = validate_raw_fd(raw_descriptor)?;
        Ok(Some(
            // SAFETY:
            // Safe because nothing else has access to validated_fd after this call.
            unsafe { SafeDescriptor::from_raw_descriptor(validated_fd) },
        ))
    } else {
        Ok(None)
    }
}

/// Open the file with the given path, or if it is of the form `/proc/self/fd/N` then just use the
/// file descriptor.
///
/// Note that this will not work properly if the same `/proc/self/fd/N` path is used twice in
/// different places, as the metadata (including the offset) will be shared between both file
/// descriptors.
pub fn open_file_or_duplicate<P: AsRef<Path>>(path: P, options: &OpenOptions) -> Result<File> {
    let path = path.as_ref();
    // Special case '/proc/self/fd/*' paths. The FD is already open, just use it.
    Ok(if let Some(fd) = safe_descriptor_from_path(path)? {
        fd.into()
    } else {
        options.open(path)?
    })
}

/// Get the max number of open files allowed by the environment.
pub fn max_open_files() -> Result<u64> {
    let mut buf = mem::MaybeUninit::<libc::rlimit64>::zeroed();

    // SAFETY:
    // Safe because this will only modify `buf` and we check the return value.
    let res = unsafe { libc::prlimit64(0, libc::RLIMIT_NOFILE, ptr::null(), buf.as_mut_ptr()) };
    if res == 0 {
        // SAFETY:
        // Safe because the kernel guarantees that the struct is fully initialized.
        let limit = unsafe { buf.assume_init() };
        Ok(limit.rlim_max)
    } else {
        errno_result()
    }
}

/// Moves the requested PID/TID to a particular cgroup
///
pub fn move_to_cgroup(cgroup_path: PathBuf, id_to_write: Pid, cgroup_file: &str) -> Result<()> {
    use std::io::Write;

    let gpu_cgroup_file = cgroup_path.join(cgroup_file);
    let mut f = File::create(gpu_cgroup_file)?;
    f.write_all(id_to_write.to_string().as_bytes())?;
    Ok(())
}

pub fn move_task_to_cgroup(cgroup_path: PathBuf, thread_id: Pid) -> Result<()> {
    move_to_cgroup(cgroup_path, thread_id, "tasks")
}

pub fn move_proc_to_cgroup(cgroup_path: PathBuf, process_id: Pid) -> Result<()> {
    move_to_cgroup(cgroup_path, process_id, "cgroup.procs")
}

/// Queries the property of a specified CPU sysfs node.
fn parse_sysfs_cpu_info_vec(cpu_id: usize, property: &str) -> Result<Vec<u32>> {
    let path = format!("/sys/devices/system/cpu/cpu{cpu_id}/{property}");
    let res: Result<Vec<_>> = std::fs::read_to_string(path)?
        .split_whitespace()
        .map(|x| x.parse().map_err(|_| Error::new(libc::EINVAL)))
        .collect();
    res
}

/// Returns a list of supported frequencies in kHz for a given logical core.
pub fn logical_core_frequencies_khz(cpu_id: usize) -> Result<Vec<u32>> {
    parse_sysfs_cpu_info_vec(cpu_id, "cpufreq/scaling_available_frequencies")
}

fn parse_sysfs_cpu_info(cpu_id: usize, property: &str) -> Result<u32> {
    let path = format!("/sys/devices/system/cpu/cpu{cpu_id}/{property}");
    std::fs::read_to_string(path)?
        .trim()
        .parse()
        .map_err(|_| Error::new(libc::EINVAL))
}

/// Returns the capacity (measure of performance) of a given logical core.
pub fn logical_core_capacity(cpu_id: usize) -> Result<u32> {
    static CPU_MAX_FREQS: OnceCell<Vec<u32>> = OnceCell::new();

    let cpu_capacity = parse_sysfs_cpu_info(cpu_id, "cpu_capacity")?;

    // Collect and cache the maximum frequencies of all cores. We need to know
    // the largest maximum frequency between all cores to reverse normalization,
    // so collect all the values once on the first call to this function.
    let cpu_max_freqs = CPU_MAX_FREQS.get_or_try_init(|| {
        (0..number_of_logical_cores()?)
            .map(logical_core_max_freq_khz)
            .collect()
    });

    if let Ok(cpu_max_freqs) = cpu_max_freqs {
        let largest_max_freq = cpu_max_freqs.iter().max().ok_or(Error::new(EINVAL))?;
        let cpu_max_freq = cpu_max_freqs.get(cpu_id).ok_or(Error::new(EINVAL))?;
        (cpu_capacity * largest_max_freq)
            .checked_div(*cpu_max_freq)
            .ok_or(Error::new(EINVAL))
    } else {
        // cpu-freq is not enabled. Fall back to using the normalized capacity.
        Ok(cpu_capacity)
    }
}

/// Returns the cluster ID of a given logical core.
pub fn logical_core_cluster_id(cpu_id: usize) -> Result<u32> {
    parse_sysfs_cpu_info(cpu_id, "topology/physical_package_id")
}

/// Returns the maximum frequency (in kHz) of a given logical core.
fn logical_core_max_freq_khz(cpu_id: usize) -> Result<u32> {
    parse_sysfs_cpu_info(cpu_id, "cpufreq/cpuinfo_max_freq")
}

#[repr(C)]
pub struct sched_attr {
    pub size: u32,

    pub sched_policy: u32,
    pub sched_flags: u64,
    pub sched_nice: i32,

    pub sched_priority: u32,

    pub sched_runtime: u64,
    pub sched_deadline: u64,
    pub sched_period: u64,

    pub sched_util_min: u32,
    pub sched_util_max: u32,
}

impl sched_attr {
    pub fn default() -> Self {
        Self {
            size: std::mem::size_of::<sched_attr>() as u32,
            sched_policy: 0,
            sched_flags: 0,
            sched_nice: 0,
            sched_priority: 0,
            sched_runtime: 0,
            sched_deadline: 0,
            sched_period: 0,
            sched_util_min: 0,
            sched_util_max: 0,
        }
    }
}

pub fn sched_setattr(pid: Pid, attr: &mut sched_attr, flags: u32) -> Result<()> {
    // SAFETY: Safe becuase all the args are valid and the return valud is checked.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_sched_setattr,
            pid as usize,
            attr as *mut sched_attr as usize,
            flags as usize,
        )
    };

    if ret < 0 {
        return Err(Error::last());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::os::fd::AsRawFd;

    use super::*;
    use crate::unix::add_fd_flags;

    #[test]
    fn pipe_size_and_fill() {
        let (_rx, mut tx) = new_pipe_full().expect("Failed to pipe");

        // To  check that setting the size worked, set the descriptor to non blocking and check that
        // write returns an error.
        add_fd_flags(tx.as_raw_fd(), libc::O_NONBLOCK).expect("Failed to set tx non blocking");
        tx.write(&[0u8; 8])
            .expect_err("Write after fill didn't fail");
    }
}
