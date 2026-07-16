; init/init.asm — Rumicos minimal userspace init process
;
; Build:
;   nasm -f elf64 -o init.o init.asm
;   ld -static -Ttext=0x400000 -e _start --build-id=none -o init.elf init.o
;   objdump -d init.elf         # disassemble to verify
;   python3 ../scripts/elf2rust.py init.elf INIT_ELF > ../kernel/src/init.rs
;
; The binary:
;   1. SYS_WRITE(1, &msg, 14)   -> prints "Hello Rumicos\n" to serial stdout
;   2. SYS_EXECVE("/init2.elf", argv, envp)
;        -> replaces this image with /init2.elf from the initrd.
;           This is the "point of no return": on success, execve never
;           returns here at all — init2.elf's _start runs next instead.
;   3. SYS_EXIT(1)              -> only reached if execve failed (rax<0)

bits 64
section .text
global _start

_start:
    ; SYS_WRITE(fd=1, buf=&msg, len=14)
    mov     edi, 1                  ; arg1: fd = stdout
    lea     rsi, [rel msg]          ; arg2: buf
    mov     edx, 14                 ; arg3: len("Hello Rumicos\n") = 14
    mov     eax, 1                  ; rax  = SYS_WRITE
    syscall

    ; SYS_EXECVE(path="/init2.elf", argv={"/init2.elf", NULL}, envp={NULL})
    lea     rdi, [rel path]         ; arg1: path
    lea     rsi, [rel argv]         ; arg2: argv
    lea     rdx, [rel envp]         ; arg3: envp
    mov     eax, 59                 ; rax  = SYS_EXECVE
    syscall

    ; Only reached if execve failed (process is unchanged; rax = -errno).
    mov     edi, 1                  ; exit code 1 signals exec failure
    mov     eax, 60                 ; rax  = SYS_EXIT
    syscall

    ; should never reach here
    jmp     $

section .rodata
msg:    db  "Hello Rumicos", 10     ; 13 + LF = 14 bytes
path:   db  "/init2.elf", 0
argv:   dq  path, 0                 ; argv[] = { &path, NULL }
envp:   dq  0                       ; envp[] = { NULL }
