//! Windows-specific extensions.

use std::os::windows::io::{AsRawHandle, RawHandle};
use std::os::windows::process::CommandExt as _;

use crate::{Child, Command};

/// Windows-specific extensions to the [`Command`] builder.
///
/// This trait is sealed: it cannot be implemented outside `async-process`.
/// This is so that future additional methods are not breaking changes.
pub trait CommandExt: crate::sealed::Sealed {
    /// Sets the [process creation flags][1] to be passed to `CreateProcess`.
    ///
    /// These will always be ORed with `CREATE_UNICODE_ENVIRONMENT`.
    ///
    /// [1]: https://docs.microsoft.com/en-us/windows/win32/procthread/process-creation-flags
    fn creation_flags(&mut self, flags: u32) -> &mut Command;
}

impl crate::sealed::Sealed for Command {}
impl CommandExt for Command {
    fn creation_flags(&mut self, flags: u32) -> &mut Command {
        self.inner.creation_flags(flags);
        self
    }
}

impl AsRawHandle for Child {
    fn as_raw_handle(&self) -> RawHandle {
        self.child.lock().unwrap().get_mut().as_raw_handle()
    }
}
