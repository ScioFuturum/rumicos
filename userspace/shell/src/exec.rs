//! Pipeline execution: the syscall half of the shell.
//!
//! Only ever consumes an already-validated [`Pipeline`] from
//! [`crate::parse`], so there is no parsing logic here — just the fork /
//! dup2 / exec / wait choreography, whose *ordering* is the whole game.

use crate::parse::{Pipeline, RedirectKind, Stage, MAX_PIPELINE_STAGES};
use liblow::fmt::write_all;
use liblow::{
    O_CREAT, O_RDONLY, O_TRUNC, O_WRONLY, STDERR, STDIN, STDOUT, close, dup2, execve, exit, fork,
    pipe, wait4, wexitstatus, wifexited,
};

/// POSIX's "command not found" exit status.
const EXIT_NOT_FOUND: i32 = 127;
/// Exit status for a child that could not set up its redirections.
const EXIT_REDIRECT_FAIL: i32 = 1;

/// One pipe's two fds.
#[derive(Clone, Copy)]
struct PipeFds {
    read: i32,
    write: i32,
}

/// Run a whole pipeline to completion. Returns the LAST stage's exit
/// status, the conventional value of `$?` for a pipeline.
pub fn execute_pipeline(p: &Pipeline<'_>) -> i32 {
    let n = p.stage_count;
    if n == 0 {
        return 0;
    }

    // ── 1. Create every pipe UP FRONT, before forking anything ──────────
    // A pipeline of n stages needs n-1 pipes; pipes[i] connects stage i's
    // stdout to stage i+1's stdin.
    let mut pipes = [PipeFds { read: -1, write: -1 }; MAX_PIPELINE_STAGES - 1];
    let pipe_count = n - 1;
    for (i, slot) in pipes.iter_mut().enumerate().take(pipe_count) {
        match pipe() {
            Ok((r, w)) => *slot = PipeFds { read: r, write: w },
            Err(_) => {
                let _ = write_all(STDERR, b"shell: pipe() failed\n");
                // Roll back the pipes already created, or they leak for the
                // life of the shell.
                for done in pipes.iter().take(i) {
                    close(done.read);
                    close(done.write);
                }
                return 1;
            }
        }
    }

    // ── 2. Fork EVERY stage before waiting for any of them ──────────────
    // Forking stage i and then wait4()-ing it before stage i+1 exists would
    // deadlock the moment stage i writes more than one pipe buffer (4 KiB)
    // with no reader yet draining the other end. Build the whole topology
    // first, then reap.
    let mut pids = [0i64; MAX_PIPELINE_STAGES];
    let mut forked = 0usize;

    for i in 0..n {
        let pid = fork();
        if pid < 0 {
            let _ = write_all(STDERR, b"shell: fork() failed\n");
            break;
        }
        if pid == 0 {
            // ── CHILD ──
            child_setup_and_exec(&p.stages[i], i, n, &pipes, pipe_count);
            // child_setup_and_exec never returns.
        }
        pids[forked] = pid;
        forked += 1;
    }

    // ── 3. The PARENT drops every pipe fd it holds ──────────────────────
    // This is mandatory, not tidiness: the shell is not part of the data
    // flow, but as long as it holds any write end open, the reader on that
    // pipe never sees EOF and the pipeline hangs forever.
    for pf in pipes.iter().take(pipe_count) {
        close(pf.read);
        close(pf.write);
    }

    // ── 4. Reap exactly as many children as we forked ───────────────────
    // wait4(-1) takes whichever exits next; we know the count, so no
    // bookkeeping of which pid is which is needed except for the last
    // stage, whose status is the pipeline's status.
    let last_pid = if forked == n { pids[n - 1] } else { -1 };
    let mut last_status = 0i32;
    for _ in 0..forked {
        let mut status = 0i32;
        let reaped = wait4(-1, &mut status);
        if reaped < 0 {
            break;
        }
        if reaped == last_pid {
            last_status = status;
        }
    }

    if wifexited(last_status) {
        wexitstatus(last_status)
    } else {
        1
    }
}

/// Wire up one stage's fds and exec it. Never returns: on any failure it
/// prints to stderr and exits, because a child that returned would carry on
/// running the shell's own code as a duplicate REPL.
fn child_setup_and_exec(
    stage: &Stage<'_>,
    i: usize,
    n: usize,
    pipes: &[PipeFds; MAX_PIPELINE_STAGES - 1],
    pipe_count: usize,
) -> ! {
    // a. stdin from the previous stage's pipe (unless this is stage 0).
    if i > 0 && dup2(pipes[i - 1].read, STDIN) < 0 {
        let _ = write_all(STDERR, b"shell: dup2(stdin) failed\n");
        exit(EXIT_REDIRECT_FAIL)
    }
    // b. stdout into the next stage's pipe (unless this is the last stage).
    // `i + 1 < n` rather than `i < n - 1`: the latter underflows to a huge
    // value when n == 0 (usize), which would wrongly take this branch.
    if i + 1 < n && dup2(pipes[i].write, STDOUT) < 0 {
        let _ = write_all(STDERR, b"shell: dup2(stdout) failed\n");
        exit(EXIT_REDIRECT_FAIL)
    }

    // c. An explicit `<` OVERRIDES the incoming pipe: it is applied after
    //    the dup2 above, so the file wins. Same for `>` below. (A stage
    //    with both a pipe and a redirect is unusual but well-defined here.)
    if let Some(path) = stage.redirect_in {
        let fd = liblow::open(path, O_RDONLY);
        if fd < 0 {
            // Real shells report the failure and still let the pipeline's
            // reap cycle run; the stage simply never execs. This child was
            // already forked, so exiting here keeps the parent's wait4
            // accounting exact.
            let _ = write_all(STDERR, b"shell: cannot open ");
            let _ = write_all(STDERR, path.as_bytes());
            let _ = write_all(STDERR, b"\n");
            exit(EXIT_REDIRECT_FAIL)
        }
        if dup2(fd as i32, STDIN) < 0 {
            let _ = write_all(STDERR, b"shell: dup2(< file) failed\n");
            exit(EXIT_REDIRECT_FAIL)
        }
        close(fd as i32);
    }

    if let Some((path, kind)) = stage.redirect_out {
        // O_TRUNC on `>` so re-running a command does not leave a longer
        // previous file's tail behind. `>>` asks for append; see the
        // known-limitations note — ramfs has no O_APPEND, so the shell
        // emulates it by seeking via the fd offset, which it cannot do
        // either, hence append currently truncates like `>`.
        let flags = match kind {
            RedirectKind::OutAppend => O_WRONLY | O_CREAT,
            _ => O_WRONLY | O_CREAT | O_TRUNC,
        };
        let fd = liblow::open(path, flags);
        if fd < 0 {
            let _ = write_all(STDERR, b"shell: cannot create ");
            let _ = write_all(STDERR, path.as_bytes());
            let _ = write_all(STDERR, b"\n");
            exit(EXIT_REDIRECT_FAIL)
        }
        if dup2(fd as i32, STDOUT) < 0 {
            let _ = write_all(STDERR, b"shell: dup2(> file) failed\n");
            exit(EXIT_REDIRECT_FAIL)
        }
        close(fd as i32);
    }

    // d. Close EVERY original pipe fd — all of them, both ends, not just the
    //    ones this stage did not use. The ones it did use have already been
    //    dup2'd onto 0/1, so the originals are redundant; the ones it did
    //    not use are someone else's plumbing. Leaving any of them open here
    //    means some other stage's reader never sees EOF, because THIS
    //    process still counts as a writer.
    for pf in pipes.iter().take(pipe_count) {
        close(pf.read);
        close(pf.write);
    }

    // e. Replace the image. There is no PATH search: argv[0] must be an
    //    absolute VFS path such as /bin/echo.
    let argv = stage.args();
    if argv.is_empty() {
        exit(EXIT_NOT_FOUND)
    }
    execve(argv[0], argv);

    // execve only returns on failure (the kernel's phase-1 error contract
    // leaves this image intact).
    let _ = write_all(STDERR, b"shell: command not found: ");
    let _ = write_all(STDERR, argv[0].as_bytes());
    let _ = write_all(STDERR, b"\n");
    exit(EXIT_NOT_FOUND)
}
