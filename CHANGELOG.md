# Version 1.2.0

- Implement `AsRawHandle` on `Child` on `Windows` (#17)

# Version 1.1.0

- Add `into_stdio` method to `ChildStdin`, `ChildStdout`, and `ChildStderr`. (#13)

# Version 1.0.2

- Use `kill_on_drop` only when the last reference to `ChildGuard` is dropped.

# Version 1.0.1

- Update `futures-lite`.

# Version 1.0.0

- Update dependencies and stabilize.

# Version 0.1.3

- Update dependencies.

# Version 0.1.2

- Add Unix and Windows extensions.
- Add `Command::reap_on_drop()` option.
- Add `Command::kill_on_drop()` option.

# Version 0.1.1

- Initial version
