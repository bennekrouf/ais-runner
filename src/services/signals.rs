//! Turn the signals that end a process without unwinding into an ordinary
//! callback on an ordinary thread.
//!
//! Without this, Ctrl-C in a terminal, a `kill`, a logout or a CI job
//! cancelling its step takes the process out between one instruction and the
//! next: patched files stay patched, spawned emulators are reparented to init,
//! and `local.settings.json` keeps whatever a scenario put in it.
//!
//! Two things make this awkward, and both are why the callback runs on a
//! thread instead of in the handler:
//!
//! * A signal handler may call almost nothing. Allocating, taking a mutex and
//!   touching the filesystem — everything a teardown does — are all forbidden
//!   there. So the handler writes one byte to a pipe and returns; a normal
//!   thread blocked on the read side does the actual work.
//! * A *disposition* set with `sigaction` is process-wide, but a signal *mask*
//!   is per-thread and inherited only by threads created afterwards. Masking
//!   plus `sigwait` therefore silently does nothing when a runtime has already
//!   started its workers — which, under `#[tokio::main]`, it always has.

#[cfg(unix)]
mod imp {
    use std::sync::atomic::{AtomicI32, Ordering};

    static SIGNALLED: AtomicI32 = AtomicI32::new(0);
    static WRITE_FD: AtomicI32 = AtomicI32::new(-1);

    const SIGNALS: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

    extern "C" fn handler(sig: libc::c_int) {
        SIGNALLED.store(sig, Ordering::SeqCst);
        let fd = WRITE_FD.load(Ordering::SeqCst);
        if fd >= 0 {
            // `write` is on the async-signal-safe list. One byte, and the
            // result is deliberately dropped: a full pipe means the waiter has
            // already been woken.
            unsafe { libc::write(fd, b"x".as_ptr() as *const libc::c_void, 1) };
        }
    }

    unsafe fn set_disposition(h: libc::sighandler_t) {
        for sig in SIGNALS {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = h;
            libc::sigemptyset(&mut sa.sa_mask);
            // Restart interrupted syscalls: nothing in the process should have
            // to grow EINTR handling because we installed a handler.
            sa.sa_flags = libc::SA_RESTART;
            libc::sigaction(sig, &sa, std::ptr::null_mut());
        }
    }

    pub fn on_termination(reraise: bool, on_signal: impl FnOnce() + Send + 'static) {
        let mut fds = [0 as libc::c_int; 2];
        unsafe {
            if libc::pipe(fds.as_mut_ptr()) != 0 {
                return;
            }
            WRITE_FD.store(fds[1], Ordering::SeqCst);
            set_disposition(handler as *const () as usize);
        }
        let read_fd = fds[0];
        std::thread::spawn(move || {
            let mut byte = [0u8; 1];
            // Blocks with no cost until the handler writes. A short read still
            // means a signal arrived; anything else means the pipe broke, and
            // there is nothing useful left to wait for.
            let n = unsafe { libc::read(read_fd, byte.as_mut_ptr() as *mut libc::c_void, 1) };
            if n <= 0 {
                return;
            }
            // From here the default disposition is back, so a second signal
            // ends the process whatever the callback is in the middle of. The
            // user asking twice is an instruction, not a retry.
            unsafe { set_disposition(libc::SIG_DFL) };
            on_signal();
            if reraise {
                // Exit status still reads "killed by signal N", as a shell and
                // a CI runner both expect.
                unsafe { libc::raise(SIGNALLED.load(Ordering::SeqCst)) };
            }
        });
    }
}

#[cfg(not(unix))]
mod imp {
    pub fn on_termination(_reraise: bool, _on_signal: impl FnOnce() + Send + 'static) {}
}

/// Run `on_signal` on a normal thread the first time SIGINT, SIGTERM or SIGHUP
/// arrives. A second signal takes the default disposition and ends the process.
///
/// With `reraise`, the process re-raises the signal once `on_signal` returns,
/// so the exit status still reports the signal rather than a clean exit. Pass
/// `false` when the callback's job is to make the program stop on its own —
/// cancelling a run that then unwinds through its own teardown.
///
/// No-op off Unix.
pub fn on_termination(reraise: bool, on_signal: impl FnOnce() + Send + 'static) {
    imp::on_termination(reraise, on_signal)
}
