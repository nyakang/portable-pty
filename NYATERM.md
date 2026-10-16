# NyaTerm portable-pty patch series

Base: crates.io `portable-pty 0.9.0`, imported unchanged as the first commit
on branch `nyaterm`. This fork exists because the application bundles Microsoft's
ConPTY runtime on Windows while retaining the system ConPTY as a fallback.

The Windows patch adds an absolute-path configuration API, validates matching
`OpenConsole.exe` hosts, and selects bundled or system functions per PTY. A
failed bundled load or creation falls back to the system functions. Each PTY
keeps its selected backend for resize and close. The status API reports active
backend counts, the last successful backend, and whether fallback occurred.

Validation: `cargo test` on Windows for the injected creation-failure unit
test. For the package-backed cases, prepare the pinned ConPTY package, set
`NYATERM_CONPTY_TEST_DLL` to its architecture-specific `conpty.dll`, and run
`cargo test --test conpty_backend -- --ignored` on Windows. The integration
test checks bundled selection, missing or corrupt DLL fallback, missing host
fallback, and active-count cleanup after close. The consuming NyaTerm workspace
also runs its transport and release-package checks.

When updating the base release, compare `src/win/psuedocon.rs` and
`src/win/mod.rs` against the new upstream source and rerun these Windows tests.

## Fresh Windows environment expansion

Carry NyaTerm Tauri 1ef75d1bb as a separate patch on top of the ConPTY changes.
Merge system/user registry values before expanding references, preserving
case-insensitive keys, PATH concatenation, unknown references and bounded
cycle handling. Environment values are never traced. Add lossless iter_full_env
so native shell environment probes can start from the same refreshed snapshot
instead of overwriting it with stale process variables.

Windows validation: cargo test (12 unit tests and 1 doctest passed); the packaged
ConPTY integration test remains ignored without NYATERM_CONPTY_TEST_DLL.

Clippy --all-targets on Rust 1.97 reports an existing clippy::uninit_vec error
in src/win/procthreadattr.rs (unchanged by this series). The check with that
single baseline lint allowed passes, with existing upstream warnings.

## 2026-10-17 fallible process lifecycle

Share Windows process handles across killers instead of infallible duplication.
Propagate poisoned locks, query, wait, duplication and termination errors; fix
TerminateProcess success detection. Waiter threads use fallible spawning and own
their handle until completion.

Windows validation: cargo test --offline passed 15 unit tests and one doctest,
including restricted process handles and injected poisoned locks. The prepared
ConPTY package test is ignored without NYATERM_CONPTY_TEST_DLL.

## 2026-10-17 cancellable PTY writes

Keep take_writer compatible and add take_cancellable_writer with an explicit
stop handle. Windows registers the writing thread for CancelSynchronousIo;
callers repeat cancellation until completion to cover the native-entry race.
Unix uses nonblocking writes and short poll waits. Stopping prevents future
writes, and writer destruction never performs blocking EOT or flush writes.
ConPTY lock failures return errors.

Windows validation: cargo test --offline passed 15 unit tests and one doctest;
a full pipe with a blocked 4 MiB write is cancelled within one second. Unix
execution and prepared package-backed ConPTY validation remain pending.

## 2026-10-17 idempotent cleanup and initialization errors

Treat an already exited Windows process, including an exit racing termination,
as successful cleanup. Preserve query and termination errors for live processes.
Load system ConPTY fallibly and return initialization/status-lock failures instead
of panicking during connection creation. Remaining ConPTY expects describe
invariants established by successful construction.

Windows validation: cargo test --offline passed 16 unit tests and one doctest,
including successful termination and repeated kill after exit. The prepared
ConPTY package integration test remains ignored without NYATERM_CONPTY_TEST_DLL.
