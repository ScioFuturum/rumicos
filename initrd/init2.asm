; initrd/init2.asm — second userspace process bundled in the initrd CPIO,
; launched via SYS_EXECVE("/init2.elf", ...) from init.asm rather than as a
; separately-spawned process.
;
; Exercises three checkpoints' worth of userspace-observable behavior over
; the shared serial line, using only the syscalls this kernel implements
; (there is still no wait()/pipe(), so fork's parent and child cannot
; rendezvous; each just reports what IT observes):
;   0. the original execve smoke test
;   1. anonymous mmap: demand-zero, write/read-back, munmap
;   2. fork() + copy-on-write: a shared .data byte, inherited then
;      independently overwritten by parent ('P') and child ('C')
;   3. file-backed MAP_SHARED mmap: open a real ramfs file, mmap it
;      shared, verify the INITIAL content came from the actual file (not
;      a stray zero page), write a marker THROUGH the mapping, munmap
;      (which must trigger writeback), then re-open the SAME file FRESH
;      and sys_read it directly — bypassing the now-discarded mapping
;      entirely — to prove the write landed in the real file, not just
;      in memory that vanished at munmap.
;
; Every check below compares against a concrete expected value with
; cmp/jne rather than merely "did we reach the next line without
; crashing", though the crash-absence signal is also meaningful here: a
; broken #PF handler, a broken VMA walk, a broken fork_cow(), or a broken
; page cache would each manifest as a kernel panic, a triple fault, or
; this process dying with SIGSEGV instead of the printed lines below ever
; appearing on the serial console.
;
; Build:
;   nasm -f elf64 -o init2.o init2.asm
;   ld -static -Ttext-segment=0x400000 -e _start --build-id=none -o init2.elf init2.o
;   printf 'init2.elf\ntestfile.txt\n' | cpio -o --format=newc > initrd.cpio
;
; Or use scripts/elf2rust.py to regenerate the embedded bytes.

bits 64
section .text
global _start

_start:
    ; ── 0. unchanged smoke test: SYS_WRITE(fd=1, buf=&msg, len=14) ──────
    mov  edi, 1
    lea  rsi, [rel msg]
    mov  edx, 14
    mov  eax, 1             ; SYS_WRITE
    syscall

    ; ── 1. anonymous mmap + demand-zero + write/read-back ───────────────
    ; sys_mmap(addr_hint=0, length=4096, prot=PROT_READ|PROT_WRITE,
    ;          flags=MAP_PRIVATE|MAP_ANONYMOUS, fd=-1, offset=0)
    ; Linux syscall ABI: arg4 goes in r10 (not rcx — rcx is clobbered by
    ; the SYSCALL instruction itself, holding the return RIP), arg5 (fd)
    ; in r8, arg6 (offset) in r9. The kernel's SYSCALL trampoline now
    ; forwards a genuine 6th argument (see kernel-cpu's syscall_entry and
    ; proc_syscall_handler's SYS_MMAP arm) — the old `(offset << 32) | fd`
    ; packing in r8 is gone. CRITICAL: r9 must be explicitly zeroed; a
    ; fresh process's r9 is whatever the kernel last left there, and a
    ; garbage offset fails sys_mmap's page-alignment check with EINVAL
    ; even for an anonymous mapping that never uses the offset.
    xor  rdi, rdi            ; addr_hint = 0
    mov  rsi, 4096           ; length
    mov  rdx, 3              ; PROT_READ(1) | PROT_WRITE(2)
    mov  r10, 0x22           ; MAP_PRIVATE(0x02) | MAP_ANONYMOUS(0x20)
    mov  r8d, -1             ; fd = -1 (32-bit mov zero-extends into r8)
    xor  r9d, r9d            ; offset = 0
    mov  eax, 9              ; SYS_MMAP
    syscall                  ; rax = mapped base address, or a negative errno
    mov  r15, rax            ; keep the mapped base in r15 across syscalls
                              ; (SYSCALL only clobbers rcx/r11, so r15
                              ; survives every syscall below untouched)

    ; First touch is a READ, before this process ever writes the page —
    ; this must demand-fault a freshly zeroed frame in, not a not-present
    ; #PF that falls through to SIGSEGV. A non-zero byte here would mean
    ; either the zero-fill step was skipped or the VMA lookup found the
    ; wrong page.
    movzx eax, byte [r15]
    test al, al
    jnz  .mmap_zero_fail
    mov  edi, 1
    lea  rsi, [rel mmap_zero_ok_msg]
    mov  edx, mmap_zero_ok_len
    mov  eax, 1
    syscall
    jmp  .mmap_write_check
.mmap_zero_fail:
    mov  edi, 1
    lea  rsi, [rel mmap_zero_fail_msg]
    mov  edx, mmap_zero_fail_len
    mov  eax, 1
    syscall

.mmap_write_check:
    ; Now write a marker and read it back. The PTE installed by the
    ; demand-zero fault above must be Writable (translated from PROT_WRITE)
    ; or this instruction itself SIGSEGVs the process — silence here would
    ; be exactly as informative as the printed FAIL line.
    mov  byte [r15], 0xAB
    movzx eax, byte [r15]
    cmp  al, 0xAB
    jne  .mmap_write_fail
    mov  edi, 1
    lea  rsi, [rel mmap_write_ok_msg]
    mov  edx, mmap_write_ok_len
    mov  eax, 1
    syscall
    jmp  .mmap_done
.mmap_write_fail:
    mov  edi, 1
    lea  rsi, [rel mmap_write_fail_msg]
    mov  edx, mmap_write_fail_len
    mov  eax, 1
    syscall

.mmap_done:
    ; sys_munmap(addr, length) — exercise the teardown path too.
    mov  rdi, r15
    mov  rsi, 4096
    mov  eax, 11             ; SYS_MUNMAP
    syscall

    ; ── 2. file-backed MAP_SHARED mmap: open, mmap, write-through,
    ;       munmap, re-open+read to prove the write reached the file ────
    ;
    ; sys_open("/testfile.txt", 13, O_RDWR)
    mov  eax, 2                    ; SYS_OPEN
    lea  rdi, [rel testfile_path]
    mov  esi, testfile_path_len
    mov  edx, 2                     ; O_RDWR
    syscall
    mov  r14, rax                   ; save fd (or a negative errno)
    cmp  r14, 0
    jl   .open_fail

    ; sys_mmap(0, 4096, PROT_READ|PROT_WRITE, MAP_SHARED, fd, offset=0)
    ; fd in r8, offset explicitly zeroed in r9 — same 6-argument ABI note
    ; as the anonymous mmap above.
    xor  rdi, rdi
    mov  rsi, 4096
    mov  rdx, 3                     ; PROT_READ|PROT_WRITE
    mov  r10, 0x01                  ; MAP_SHARED
    mov  r8, r14
    xor  r9d, r9d                   ; offset = 0
    mov  eax, 9                     ; SYS_MMAP
    syscall
    mov  r13, rax                   ; mapped base (or negative errno)
    cmp  r13, 0
    jl   .file_mmap_fail

    ; Initial-content check: the very first byte read through this
    ; mapping must be 'P' (0x50) — the real first byte of testfile.txt's
    ; "PAGECACHE-TEST-0\n" content, demand-filled by get_or_fill from the
    ; actual file via the page cache. A stray zero (or garbage) here
    ; would mean the file-backed demand-fill path silently fell back to
    ; anonymous zero-fill, or read from the wrong offset/vnode.
    movzx eax, byte [r13]
    cmp  al, 'P'
    jne  .file_initial_content_fail
    mov  edi, 1
    lea  rsi, [rel file_initial_ok_msg]
    mov  edx, file_initial_ok_len
    mov  eax, 1
    syscall
    jmp  .file_write_through
.file_initial_content_fail:
    mov  edi, 1
    lea  rsi, [rel file_initial_fail_msg]
    mov  edx, file_initial_fail_len
    mov  eax, 1
    syscall

.file_write_through:
    ; Overwrite the first byte THROUGH the shared mapping. Because this
    ; is MAP_SHARED (not MAP_PRIVATE), this must go straight to the
    ; page-cache frame with no copy-on-write — resolve_file_fault already
    ; mapped this page writable+dirty-marked on the very first fault
    ; (there is no separate "upgrade to writable" fault to speak of for a
    ; MAP_SHARED page whose VMA permits writes).
    mov  byte [r13], 'X'

    ; sys_munmap(addr, 4096) — for a MAP_SHARED VMA this must trigger
    ; writeback_vnode() BEFORE the PTE is torn down (see
    ; AddressSpace::munmap's Backing::File{shared:true} branch).
    mov  rdi, r13
    mov  rsi, 4096
    mov  eax, 11                    ; SYS_MUNMAP
    syscall

    mov  edi, r14d
    mov  eax, 3                     ; SYS_CLOSE
    syscall

    ; Re-open the SAME file FRESH and sys_read it directly — this
    ; deliberately does NOT reuse the just-unmapped VMA or the page
    ; cache's in-memory frame from this process's point of view; it goes
    ; through an entirely new fd and a plain read() syscall, so a match
    ; here can only mean the write in .file_write_through actually landed
    ; in the underlying ramfs file's own storage, not merely in a mapping
    ; that's now gone.
    mov  eax, 2                     ; SYS_OPEN
    lea  rdi, [rel testfile_path]
    mov  esi, testfile_path_len
    xor  edx, edx                   ; O_RDONLY
    syscall
    mov  r14, rax
    cmp  r14, 0
    jl   .reopen_fail

    mov  edi, r14d
    lea  rsi, [rel reread_buf]
    mov  edx, 17
    mov  eax, 0                     ; SYS_READ
    syscall

    movzx eax, byte [rel reread_buf]
    cmp  al, 'X'
    jne  .file_writeback_fail
    mov  edi, 1
    lea  rsi, [rel file_writeback_ok_msg]
    mov  edx, file_writeback_ok_len
    mov  eax, 1
    syscall
    jmp  .file_mmap_section_done
.file_writeback_fail:
    mov  edi, 1
    lea  rsi, [rel file_writeback_fail_msg]
    mov  edx, file_writeback_fail_len
    mov  eax, 1
    syscall
    jmp  .file_mmap_section_done

.open_fail:
    mov  edi, 1
    lea  rsi, [rel file_open_fail_msg]
    mov  edx, file_open_fail_len
    mov  eax, 1
    syscall
    jmp  .file_mmap_section_done
.file_mmap_fail:
    mov  edi, 1
    lea  rsi, [rel file_mmap_fail_msg]
    mov  edx, file_mmap_fail_len
    mov  eax, 1
    syscall
    jmp  .file_mmap_section_done
.reopen_fail:
    mov  edi, 1
    lea  rsi, [rel file_reopen_fail_msg]
    mov  edx, file_reopen_fail_len
    mov  eax, 1
    syscall

.file_mmap_section_done:
    ; Close the re-opened testfile fd (r14, still open from the writeback
    ; re-read) so later fd allocations start from a clean, predictable
    ; number. Without this the pipe() below would get fds 4/5 instead of
    ; 3/4, and the dup2-onto-fd-5 check would collide with the pipe's own
    ; write end. Closing a negative/stale r14 on an error path is a
    ; harmless EBADF.
    mov  edi, r14d
    mov  eax, 3                     ; SYS_CLOSE
    syscall

    ; ── 4. signals: sigaction + self-kill + handler + sigreturn ──────────
    ; Runs in the single (pre-fork) process so its output appears exactly
    ; once. Registers a SIGUSR1 handler, then self-signals with
    ; kill(getpid(), SIGUSR1); the pending signal is delivered on the
    ; return from the kill() syscall itself (the kernel's syscall-return
    ; signal-delivery hook), the handler prints and returns through the
    ; sigreturn trampoline, and execution resumes right after kill().
    ;
    ; sigaction(SIGUSR1, &sig_act, NULL) — sig_act holds the handler addr.
    mov  edi, 10                    ; signum = SIGUSR1
    lea  rsi, [rel sig_act]         ; act: qword = handler address
    xor  edx, edx                   ; oldact = NULL
    mov  eax, 13                    ; SYS_SIGACTION
    syscall

    ; getpid() → rax
    mov  eax, 39                    ; SYS_GETPID
    syscall
    mov  rdi, rax                   ; kill() arg0 = our own pid

    ; Fix A regression sentinels: load the SysV callee-saved registers with
    ; known values right before the signal-triggering syscall. The handler
    ; (sigusr1_handler) zeroes rbx/r12; the kernel must restore them from
    ; the saved context on sigreturn, so they must read back unchanged here.
    mov  rbx, 0xDEADBEEFCAFE1234
    mov  r12, 0x1111222233334444

    ; kill(pid, SIGUSR1). On return, the kernel delivers SIGUSR1: the
    ; handler below runs, prints, and sigreturns back to the instruction
    ; right after this syscall.
    mov  esi, 10                    ; signum = SIGUSR1
    mov  eax, 62                    ; SYS_KILL
    syscall

    ; ── resumed here by sigreturn after the handler returned ─────────────
    mov  edi, 1
    lea  rsi, [rel sig_returned_ok_msg]
    mov  edx, sig_returned_ok_len
    mov  eax, 1
    syscall

    ; Verify the callee-saved sentinels survived signal delivery. rax/rdi/
    ; rsi/rdx are caller-saved and freely reloaded; rbx/r12 must still hold
    ; the sentinels the handler tried to destroy.
    mov  rax, 0xDEADBEEFCAFE1234
    cmp  rbx, rax
    jne  .signal_clobber_fail
    mov  rax, 0x1111222233334444
    cmp  r12, rax
    jne  .signal_clobber_fail
    mov  edi, 1
    lea  rsi, [rel sig_callee_ok_msg]
    mov  edx, sig_callee_ok_len
    mov  eax, 1
    syscall
    jmp  .signal_callee_done
.signal_clobber_fail:
    mov  edi, 1
    lea  rsi, [rel sig_callee_fail_msg]
    mov  edx, sig_callee_fail_len
    mov  eax, 1
    syscall
.signal_callee_done:

    mov  edi, 1
    lea  rsi, [rel sig_all_ok_msg]
    mov  edx, sig_all_ok_len
    mov  eax, 1
    syscall

.signal_section_done:
    ; ── 6. pipe() + read/write + dup2 ─────────────────────────────────────
    ; Runs in the single pre-fork process so its output appears once.
    ; pipe(pipe_fds): pipe_fds[0]=read end, pipe_fds[1]=write end.
    lea  rdi, [rel pipe_fds]
    mov  eax, 22                    ; SYS_PIPE
    syscall
    test rax, rax
    jnz  .pipe_fail                 ; nonzero return = error

    ; write "ping" (4 bytes) to the write end
    mov  edi, [rel pipe_fds + 4]    ; write fd
    lea  rsi, [rel ping_msg]
    mov  edx, 4
    mov  eax, 1                     ; SYS_WRITE
    syscall
    cmp  rax, 4
    jne  .pipe_fail

    ; read 4 bytes back from the read end
    mov  edi, [rel pipe_fds]        ; read fd
    lea  rsi, [rel pipe_rbuf]
    mov  edx, 4
    mov  eax, 0                     ; SYS_READ
    syscall
    cmp  rax, 4
    jne  .pipe_fail
    ; the 4 bytes read back must equal "ping"
    mov  eax, dword [rel pipe_rbuf]
    cmp  eax, dword [rel ping_msg]
    jne  .pipe_fail
    mov  edi, 1
    lea  rsi, [rel pipe_ok_msg]
    mov  edx, pipe_ok_len
    mov  eax, 1
    syscall

    ; dup2 the read end onto fd 5, then prove fd 5 IS the read end by
    ; writing a fresh token and reading it back through fd 5.
    mov  edi, [rel pipe_fds]        ; oldfd = read end
    mov  esi, 5                     ; newfd = 5
    mov  eax, 33                    ; SYS_DUP2
    syscall
    cmp  rax, 5                     ; dup2 returns newfd on success
    jne  .dup_fail

    mov  edi, [rel pipe_fds + 4]    ; write "pong" to the write end
    lea  rsi, [rel pong_msg]
    mov  edx, 4
    mov  eax, 1                     ; SYS_WRITE
    syscall
    cmp  rax, 4
    jne  .dup_fail

    mov  edi, 5                     ; read through the dup'd fd 5
    lea  rsi, [rel pipe_rbuf]
    mov  edx, 4
    mov  eax, 0                     ; SYS_READ
    syscall
    cmp  rax, 4
    jne  .dup_fail
    mov  eax, dword [rel pipe_rbuf]
    cmp  eax, dword [rel pong_msg]
    jne  .dup_fail
    mov  edi, 1
    lea  rsi, [rel dup_ok_msg]
    mov  edx, dup_ok_len
    mov  eax, 1
    syscall
    jmp  .pipe_done
.pipe_fail:
    mov  edi, 1
    lea  rsi, [rel pipe_fail_msg]
    mov  edx, pipe_fail_len
    mov  eax, 1
    syscall
    jmp  .pipe_done
.dup_fail:
    mov  edi, 1
    lea  rsi, [rel dup_fail_msg]
    mov  edx, dup_fail_len
    mov  eax, 1
    syscall
.pipe_done:

    ; ── 5. wait4: fork a child, block until it exits, reap it ─────────────
    ; The child prints and exit(42)s; the parent blocks in wait4 until the
    ; child's exit wakes it, reaps the zombie, and checks the decoded exit
    ; status. r12 (callee-saved, preserved across the blocking wait4 by the
    ; syscall trampoline + context switch) holds the child pid.
    mov  eax, 57                    ; SYS_FORK
    syscall
    test rax, rax
    jz   .wait_child

    ; PARENT
    mov  r12, rax                   ; remember child pid
    mov  rdi, r12                   ; wait4 arg0 = child pid
    lea  rsi, [rel wait_status_buf] ; arg1 = &status
    xor  edx, edx                   ; arg2 = options = 0 (blocking)
    xor  r10, r10                   ; arg3 = rusage = NULL
    mov  eax, 61                    ; SYS_WAIT4
    syscall
    cmp  rax, r12                   ; wait4 must return the reaped child pid
    jne  .wait_fail
    mov  eax, dword [rel wait_status_buf]
    test eax, 0x7f                  ; WIFEXITED: low 7 bits must be 0
    jnz  .wait_fail
    shr  eax, 8
    and  eax, 0xff                  ; WEXITSTATUS
    cmp  eax, 42
    jne  .wait_fail
    mov  edi, 1
    lea  rsi, [rel wait_ok_msg]
    mov  edx, wait_ok_len
    mov  eax, 1
    syscall
    jmp  .wait_done
.wait_fail:
    mov  edi, 1
    lea  rsi, [rel wait_fail_msg]
    mov  edx, wait_fail_len
    mov  eax, 1
    syscall
    jmp  .wait_done
.wait_child:
    mov  edi, 1
    lea  rsi, [rel wait_child_msg]
    mov  edx, wait_child_len
    mov  eax, 1
    syscall

    ; ── fork name-copy check (miscompile regression, docs/miscompile-audit.md)
    ; This runs in the FORKED CHILD, whose Process.name was produced by the
    ; `name: parent.name` copy that the rustc 1.97.0 aggregate-copy
    ; miscompile silently shredded (odd bytes garbage) before the
    ; -soft-float flag fix. prctl(PR_GET_NAME) is the first-ever reader of
    ; that field; a full 16-byte compare against the known parent name
    ; "init" (set once in Process::create and never changed by execve) is
    ; what would catch the shredding. Runs before exit(42), so it prints
    ; before the parent's wait4 returns — deterministic ordering.
    mov  edi, 16                    ; PR_GET_NAME
    lea  rsi, [rel name_buf]        ; 16-byte destination
    mov  eax, 157                   ; SYS_PRCTL
    syscall
    ; byte-compare name_buf[0..16] against expected_name[0..16].
    ; Addresses loaded into base registers first: x86-64 cannot combine a
    ; RIP-relative operand with an index register in one memory reference.
    lea  r8, [rel name_buf]
    lea  r9, [rel expected_name]
    xor  ecx, ecx
.name_cmp_loop:
    mov  r10b, [r8 + rcx]
    mov  r11b, [r9 + rcx]
    cmp  r10b, r11b
    jne  .name_fail
    inc  ecx
    cmp  ecx, 16
    jne  .name_cmp_loop
    mov  edi, 1
    lea  rsi, [rel name_ok_msg]
    mov  edx, name_ok_len
    mov  eax, 1
    syscall
    jmp  .name_done
.name_fail:
    mov  edi, 1
    lea  rsi, [rel name_fail_msg]
    mov  edx, name_fail_len
    mov  eax, 1
    syscall
.name_done:

    mov  edi, 42                    ; exit code 42
    mov  eax, 60                    ; SYS_EXIT
    syscall
    ; unreachable
.wait_done:
    ; ── 3. fork() + copy-on-write ────────────────────────────────────────
    ; Seed a byte in this process's own .data — every present USER page
    ; (this one included) becomes CoW-shared with the child the instant
    ; fork() returns, so this exact byte is what the inherited-content
    ; check below verifies survived the share.
    mov  byte [rel shared_byte], 0x42

    mov  eax, 57              ; SYS_FORK
    syscall                   ; parent: rax = child pid; child: rax = 0
    cmp  rax, 0
    je   .child_path

; ───────────────────────── parent path ──────────────────────────────────
.parent_path:
    ; Inherited-content check: the page fork_cow() shared with the child
    ; must still read back exactly what was written before the fork — if
    ; resolve_cow_fault's copy step ever read from the wrong source frame,
    ; or fork_cow shared the wrong PTE, this is where it would show up as
    ; 0x00 (a stray fresh zero frame) or garbage instead of 0x42.
    movzx eax, byte [rel shared_byte]
    cmp  al, 0x42
    jne  .parent_inherited_fail
    mov  edi, 1
    lea  rsi, [rel parent_inherited_ok_msg]
    mov  edx, parent_inherited_ok_len
    mov  eax, 1
    syscall
    jmp  .parent_write
.parent_inherited_fail:
    mov  edi, 1
    lea  rsi, [rel parent_inherited_fail_msg]
    mov  edx, parent_inherited_fail_len
    mov  eax, 1
    syscall

.parent_write:
    ; The CoW write itself: this instruction is what actually drives the
    ; #PF → resolve_cow_fault path (PF_PRESENT|PF_WRITE on a PTE_COW page).
    mov  byte [rel shared_byte], 'P'
    movzx eax, byte [rel shared_byte]
    cmp  al, 'P'
    jne  .parent_write_fail
    mov  edi, 1
    lea  rsi, [rel parent_write_ok_msg]
    mov  edx, parent_write_ok_len
    mov  eax, 1
    syscall
    jmp  .parent_exit
.parent_write_fail:
    mov  edi, 1
    lea  rsi, [rel parent_write_fail_msg]
    mov  edx, parent_write_fail_len
    mov  eax, 1
    syscall

.parent_exit:
    ; Reap the CoW-test child before becoming the shell: the shell reaps
    ; its own pipeline stages with wait4(-1), and a zombie left over from
    ; THIS fork test would be miscounted as one of them (off-by-one in
    ; every pipeline's reap cycle). wait4 also orders the serial output —
    ; every child line lands before the shell's first line.
    mov  rdi, -1              ; any child
    xor  esi, esi             ; status = NULL (sys_wait4 skips the write)
    xor  edx, edx             ; options = 0
    xor  r10d, r10d           ; rusage = NULL
    mov  eax, 61              ; SYS_WAIT4
    syscall

    ; Hand the boot over to the userspace shell (/bin/shell from initrd).
    ; On success this never returns; the shell's self-test lines continue
    ; the expected-boot transcript.
    lea  rdi, [rel shell_path]
    lea  rsi, [rel shell_argv]
    xor  edx, edx             ; envp = NULL
    mov  eax, 59              ; SYS_EXECVE
    syscall

    ; Only reached if execve failed (phase-1 error: image intact).
    mov  edi, 1
    lea  rsi, [rel shell_fail_msg]
    mov  edx, shell_fail_len
    mov  eax, 1               ; SYS_WRITE
    syscall
    mov  edi, 1
    mov  eax, 60              ; SYS_EXIT(1)
    syscall
    jmp  $                    ; unreachable

; ───────────────────────── child path ───────────────────────────────────
.child_path:
    ; Same inherited-content check, independently, on the child's own copy
    ; of the page (which may be the original frame or a freshly copied one
    ; depending on fault order — both must read 0x42 here either way).
    movzx eax, byte [rel shared_byte]
    cmp  al, 0x42
    jne  .child_inherited_fail
    mov  edi, 1
    lea  rsi, [rel child_inherited_ok_msg]
    mov  edx, child_inherited_ok_len
    mov  eax, 1
    syscall
    jmp  .child_write
.child_inherited_fail:
    mov  edi, 1
    lea  rsi, [rel child_inherited_fail_msg]
    mov  edx, child_inherited_fail_len
    mov  eax, 1
    syscall

.child_write:
    mov  byte [rel shared_byte], 'C'
    movzx eax, byte [rel shared_byte]
    cmp  al, 'C'
    jne  .child_write_fail
    mov  edi, 1
    lea  rsi, [rel child_write_ok_msg]
    mov  edx, child_write_ok_len
    mov  eax, 1
    syscall
    jmp  .child_exit
.child_write_fail:
    mov  edi, 1
    lea  rsi, [rel child_write_fail_msg]
    mov  edx, child_write_fail_len
    mov  eax, 1
    syscall

.child_exit:
    xor  edi, edi
    mov  eax, 60              ; SYS_EXIT
    syscall
    jmp  $                    ; unreachable

; ───────────────────────── SIGUSR1 handler ───────────────────────────────
; Reached only by the kernel setting RIP here on signal delivery (rdi =
; signum, SysV arg0). Kept outside _start's local-label scope so the
; `.signal_section_done` label above stays in _start's scope. Prints a
; fixed message and returns; the `ret` pops the sigreturn-trampoline
; address the kernel pushed, which executes SYS_SIGRETURN to restore the
; interrupted context.
global sigusr1_handler
sigusr1_handler:
    ; Deliberately DESTROY the callee-saved sentinels the interrupted code
    ; left in rbx/r12. If they still read correctly after sigreturn, that
    ; proves the kernel restored them from the saved context — not merely
    ; that the handler happened not to touch them.
    xor  ebx, ebx
    xor  r12d, r12d
    mov  edi, 1
    lea  rsi, [rel sig_received_msg]
    mov  edx, sig_received_len
    mov  eax, 1              ; SYS_WRITE
    syscall
    ret                     ; → sigreturn trampoline

section .rodata
msg: db "execve works!", 10   ; 13 chars + LF = 14 bytes

mmap_zero_ok_msg:        db "mmap: demand-zero page reads 0 before any write - OK", 10
mmap_zero_ok_len         equ $ - mmap_zero_ok_msg
mmap_zero_fail_msg:      db "mmap: demand-zero page was NOT zero - FAIL", 10
mmap_zero_fail_len       equ $ - mmap_zero_fail_msg
mmap_write_ok_msg:       db "mmap: write 0xAB then read back 0xAB - OK", 10
mmap_write_ok_len        equ $ - mmap_write_ok_msg
mmap_write_fail_msg:     db "mmap: write/read-back mismatch - FAIL", 10
mmap_write_fail_len      equ $ - mmap_write_fail_msg

testfile_path:           db "/testfile.txt"
testfile_path_len        equ $ - testfile_path

file_open_fail_msg:        db "file mmap: open(/testfile.txt) failed - FAIL", 10
file_open_fail_len         equ $ - file_open_fail_msg
file_mmap_fail_msg:        db "file mmap: MAP_SHARED mmap() failed - FAIL", 10
file_mmap_fail_len         equ $ - file_mmap_fail_msg
file_reopen_fail_msg:      db "file mmap: re-open after munmap failed - FAIL", 10
file_reopen_fail_len       equ $ - file_reopen_fail_msg
file_initial_ok_msg:       db "file mmap: initial content is 'P' from real file - OK", 10
file_initial_ok_len        equ $ - file_initial_ok_msg
file_initial_fail_msg:     db "file mmap: initial content NOT from real file - FAIL", 10
file_initial_fail_len      equ $ - file_initial_fail_msg
file_writeback_ok_msg:     db "file mmap: munmap writeback landed in file ('X') - OK", 10
file_writeback_ok_len      equ $ - file_writeback_ok_msg
file_writeback_fail_msg:   db "file mmap: write did NOT reach the file - FAIL", 10
file_writeback_fail_len    equ $ - file_writeback_fail_msg

parent_inherited_ok_msg:   db "parent: shared_byte inherited as 0x42 - OK", 10
parent_inherited_ok_len    equ $ - parent_inherited_ok_msg
parent_inherited_fail_msg: db "parent: shared_byte NOT 0x42 after fork - FAIL", 10
parent_inherited_fail_len  equ $ - parent_inherited_fail_msg
parent_write_ok_msg:       db "parent: CoW write resolved, shared_byte='P' - OK", 10
parent_write_ok_len        equ $ - parent_write_ok_msg
parent_write_fail_msg:     db "parent: CoW write did not stick - FAIL", 10
parent_write_fail_len      equ $ - parent_write_fail_msg

child_inherited_ok_msg:    db "child: shared_byte inherited as 0x42 - OK", 10
child_inherited_ok_len     equ $ - child_inherited_ok_msg
child_inherited_fail_msg:  db "child: shared_byte NOT 0x42 after fork - FAIL", 10
child_inherited_fail_len   equ $ - child_inherited_fail_msg
child_write_ok_msg:        db "child: CoW write resolved, shared_byte='C' - OK", 10
child_write_ok_len         equ $ - child_write_ok_msg
child_write_fail_msg:      db "child: CoW write did not stick - FAIL", 10
child_write_fail_len       equ $ - child_write_fail_msg

sig_received_msg:          db "signal: SIGUSR1 received", 10
sig_received_len           equ $ - sig_received_msg
sig_returned_ok_msg:       db "signal: handler returned OK", 10
sig_returned_ok_len        equ $ - sig_returned_ok_msg
sig_callee_ok_msg:         db "signal: callee-saved registers preserved - OK", 10
sig_callee_ok_len          equ $ - sig_callee_ok_msg
sig_callee_fail_msg:       db "signal: callee-saved CLOBBERED - FAIL", 10
sig_callee_fail_len        equ $ - sig_callee_fail_msg
sig_all_ok_msg:            db "signals: all OK", 10
sig_all_ok_len             equ $ - sig_all_ok_msg

wait_child_msg:            db "wait: child running", 10
wait_child_len             equ $ - wait_child_msg
wait_ok_msg:               db "wait: child exited with code 42 - OK", 10
wait_ok_len                equ $ - wait_ok_msg
wait_fail_msg:             db "wait: FAIL", 10
wait_fail_len              equ $ - wait_fail_msg

shell_fail_msg:            db "init2: execve(/bin/shell) failed - FAIL", 10
shell_fail_len             equ $ - shell_fail_msg

name_ok_msg:               db "fork: child name intact after copy - OK", 10
name_ok_len                equ $ - name_ok_msg
name_fail_msg:             db "fork: child name SHREDDED by miscompile - FAIL", 10
name_fail_len              equ $ - name_fail_msg

pipe_ok_msg:               db "pipe: write/read through pipe - OK", 10
pipe_ok_len                equ $ - pipe_ok_msg
pipe_fail_msg:             db "pipe: write/read through pipe - FAIL", 10
pipe_fail_len              equ $ - pipe_fail_msg
dup_ok_msg:                db "pipe: dup2 aliasing - OK", 10
dup_ok_len                 equ $ - dup_ok_msg
dup_fail_msg:              db "pipe: dup2 aliasing - FAIL", 10
dup_fail_len               equ $ - dup_fail_msg
; 4-byte tokens written through the pipe (no trailing LF — compared as
; raw dwords against the read-back bytes).
ping_msg:                  db "ping"
pong_msg:                  db "pong"
; The process name is "init" (set in Process::create; execve does not
; change it), zero-padded to the full 16-byte Process.name field.
expected_name:             db "init", 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0

section .data
; Lives in this process's USER data page, so it becomes CoW-shared between
; parent and child the moment fork() returns. Pre-fork value is 0x42;
; each side then stamps its own ASCII marker over it independently.
shared_byte: db 0x42

; wait4 writes the child's encoded exit status here.
align 4
wait_status_buf: dd 0

; sigaction() user ABI (see kernel crate::syscall): a single qword holding
; the disposition — 0 = SIG_DFL, 1 = SIG_IGN, else the handler address.
; ld resolves sigusr1_handler to its absolute link-time VA (static link).
align 8
sig_act: dq sigusr1_handler

; Scratch buffer for the re-read-after-writeback check. Zero-initialized
; explicitly here (rather than left to a separate .bss section) to avoid
; introducing a THIRD PT_LOAD segment with p_memsz > p_filesz — this
; kernel's ELF loader supports multiple segments (proven already by the
; existing two: RX text+rodata, RW data), but a real bss-zero-fill segment
; was never exercised by any prior checkpoint's demo, so keeping this
; explicitly-initialized avoids depending on untested behavior for what is
; ultimately just a 32-byte scratch buffer.
reread_buf: times 32 db 0

; prctl(PR_GET_NAME) writes the caller's 16-byte process name here. Lives
; in .data (writable); the forked child that reads it triggers an ordinary
; CoW fault on first write, same as shared_byte above.
align 16
name_buf: times 16 db 0

; execve(/bin/shell) hand-off. The path is NUL-terminated and shell_argv is
; a NULL-terminated array of pointers, exactly what sys_execve reads. ld
; resolves shell_path to its absolute link-time VA (static, non-PIE link).
shell_path: db "/bin/shell", 0
align 8
shell_argv: dq shell_path, 0

; pipe() writes the two fd numbers here: [0]=read end, [1]=write end.
align 8
pipe_fds: dd 0, 0
; Scratch for reading bytes back out of the pipe.
align 4
pipe_rbuf: times 8 db 0
