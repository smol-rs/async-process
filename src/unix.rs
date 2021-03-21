//! Unix-specific extensions.

use std::ffi::OsStr;
use std::io;
use std::os::unix::process::CommandExt as _;
use std::process::{ChildStderr, ChildStdin, ChildStdout};

use crate::Command;

/// Unix-specific extensions to the [`Command`] builder.
pub trait CommandExt {
    /// Sets the child process's user ID. This translates to a
    /// `setuid` call in the child process. Failure in the `setuid`
    /// call will cause the spawn to fail.
    fn uid(&mut self, id: u32) -> &mut Command;

    /// Similar to `uid`, but sets the group ID of the child process. This has
    /// the same semantics as the `uid` field.
    fn gid(&mut self, id: u32) -> &mut Command;

    /// Schedules a closure to be run just before the `exec` function is
    /// invoked.
    ///
    /// The closure is allowed to return an I/O error whose OS error code will
    /// be communicated back to the parent and returned as an error from when
    /// the spawn was requested.
    ///
    /// Multiple closures can be registered and they will be called in order of
    /// their registration. If a closure returns `Err` then no further closures
    /// will be called and the spawn operation will immediately return with a
    /// failure.
    ///
    /// # Notes and Safety
    ///
    /// This closure will be run in the context of the child process after a
    /// `fork`. This primarily means that any modifications made to memory on
    /// behalf of this closure will **not** be visible to the parent process.
    /// This is often a very constrained environment where normal operations
    /// like `malloc` or acquiring a mutex are not guaranteed to work (due to
    /// other threads perhaps still running when the `fork` was run).
    ///
    /// This also means that all resources such as file descriptors and
    /// memory-mapped regions got duplicated. It is your responsibility to make
    /// sure that the closure does not violate library invariants by making
    /// invalid use of these duplicates.
    ///
    /// When this closure is run, aspects such as the stdio file descriptors and
    /// working directory have successfully been changed, so output to these
    /// locations may not appear where intended.
    unsafe fn pre_exec<F>(&mut self, f: F) -> &mut Command
    where
        F: FnMut() -> io::Result<()> + Send + Sync + 'static;

    /// Performs all the required setup by this `Command`, followed by calling
    /// the `execvp` syscall.
    ///
    /// On success this function will not return, and otherwise it will return
    /// an error indicating why the exec (or another part of the setup of the
    /// `Command`) failed.
    ///
    /// `exec` not returning has the same implications as calling
    /// [`std::process::exit`] – no destructors on the current stack or any other
    /// thread’s stack will be run. Therefore, it is recommended to only call
    /// `exec` at a point where it is fine to not run any destructors. Note,
    /// that the `execvp` syscall independently guarantees that all memory is
    /// freed and all file descriptors with the `CLOEXEC` option (set by default
    /// on all file descriptors opened by the standard library) are closed.
    ///
    /// This function, unlike `spawn`, will **not** `fork` the process to create
    /// a new child. Like spawn, however, the default behavior for the stdio
    /// descriptors will be to inherited from the current process.
    ///
    /// # Notes
    ///
    /// The process may be in a "broken state" if this function returns in
    /// error. For example the working directory, environment variables, signal
    /// handling settings, various user/group information, or aspects of stdio
    /// file descriptors may have changed. If a "transactional spawn" is
    /// required to gracefully handle errors it is recommended to use the
    /// cross-platform `spawn` instead.
    fn exec(&mut self) -> io::Error;

    /// Set executable argument
    ///
    /// Set the first process argument, `argv[0]`, to something other than the
    /// default executable path.
    fn arg0<S>(&mut self, arg: S) -> &mut Command
    where
        S: AsRef<OsStr>;
}

impl CommandExt for Command {
    fn uid(&mut self, id: u32) -> &mut Command {
        self.inner.uid(id);
        self
    }

    fn gid(&mut self, id: u32) -> &mut Command {
        self.inner.gid(id);
        self
    }

    unsafe fn pre_exec<F>(&mut self, f: F) -> &mut Command
    where
        F: FnMut() -> io::Result<()> + Send + Sync + 'static,
    {
        self.inner.pre_exec(f);
        self
    }

    fn exec(&mut self) -> io::Error {
        self.inner.exec()
    }

    fn arg0<S>(&mut self, arg: S) -> &mut Command
    where
        S: AsRef<OsStr>,
    {
        self.inner.arg0(arg);
        self
    }
}

/// Moves `Fd` out of nonblocking mode.
pub fn set_blocking<T: Blocking>(stdout: T) -> io::Result<T> {
    unsafe {
        let res = libc::fcntl(stdout.as_raw_fd(), libc::F_GETFL);
        let errno = libc::fcntl(
            stdout.as_raw_fd(),
            libc::F_SETFL,
            !(!res | libc::O_NONBLOCK),
        );

        // Unix-like systems when errno is_minus_one then return last_os_error.
        if errno == -1 {
            return Err(io::Error::last_os_error());
        }
    }

    Ok(stdout)
}

/// This is a sealed trait that allows types with the specified range of constraints to use `set_bloking`.
pub trait Blocking: secret_jar::SuperBlocking {}

/// Implement Blocking to `ChildStd*`, because getting the internal `ChildStd*` via `into_inner`
/// may require restoring the file descriptor to blocking mode.
impl Blocking for ChildStdin {}
impl Blocking for ChildStdout {}
impl Blocking for ChildStderr {}

mod secret_jar {
    use std::os::unix::io::AsRawFd;
    use std::process::{ChildStderr, ChildStdin, ChildStdout};

    pub trait SuperBlocking: AsRawFd {}

    impl SuperBlocking for ChildStdin {}
    impl SuperBlocking for ChildStdout {}
    impl SuperBlocking for ChildStderr {}
}

mod test {

    #[test]
    fn test_into_inner() {
        futures_lite::future::block_on(async {
            use crate::unix::set_blocking;
            use crate::Command;

            use std::io::{Read, Result};
            use std::process::Stdio;
            use std::str::from_utf8;

            let mut ls_child = Command::new("cat")
                .arg("Cargo.toml")
                .stdout(Stdio::piped())
                .spawn()?;

            let stdio: Stdio = ls_child.stdout.take().unwrap().into_inner().await?.into();

            let mut echo_child = Command::new("grep")
                .arg("async")
                .stdin(stdio)
                .stdout(Stdio::piped())
                .spawn()?;

            let mut buf = vec![];
            let mut stdout = echo_child.stdout.take().unwrap().into_inner().await?;
            stdout = set_blocking(stdout)?;

            stdout.read_to_end(&mut buf)?;
            dbg!(from_utf8(&buf).unwrap_or(""));

            Result::Ok(())
        })
        .unwrap();
    }
}
