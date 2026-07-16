; Excerpt from the SHIPPED kernel ELF (target/x86_64-unknown-none/release/kernel,
; built 2026-07-14, rustc 1.97.0 2d8144b78, QEMU CI 16/16 green) —
; kernel_proc::fork::sys_fork, disassembled with llvm-objdump -d --demangle.
;
; PART 1: parent Process +0x20..0x2f is 'name: [u8; 16]'. The copy into a
; stack temp reads/writes ONLY the 8 EVEN bytes; odd bytes are never touched
; anywhere in the function (verified by exhaustive grep, see docs/miscompile-audit.md).
ffffffff80015967:      	movq	%r13, %rbx
ffffffff8001596a:      	leaq	(%rax,%r14), %r13
ffffffff8001596e:      	movzbl	0x2e(%rbx), %eax
ffffffff80015972:      	movb	%al, 0x86e(%rsp)
ffffffff80015979:      	movzbl	0x2c(%rbx), %eax
ffffffff8001597d:      	movb	%al, 0x86c(%rsp)
ffffffff80015984:      	movzbl	0x2a(%rbx), %eax
ffffffff80015988:      	movb	%al, 0x86a(%rsp)
ffffffff8001598f:      	movzbl	0x28(%rbx), %eax
ffffffff80015993:      	movb	%al, 0x868(%rsp)
ffffffff8001599a:      	movzbl	0x26(%rbx), %eax
ffffffff8001599e:      	movb	%al, 0x866(%rsp)
ffffffff800159a5:      	movzbl	0x24(%rbx), %eax
ffffffff800159a9:      	movb	%al, 0x864(%rsp)
ffffffff800159b0:      	movzbl	0x20(%rbx), %eax
ffffffff800159b4:      	movzbl	0x22(%rbx), %ecx
ffffffff800159b8:      	movb	%cl, 0x862(%rsp)
ffffffff800159bf:      	movb	%al, 0x860(%rsp)
ffffffff800159c6:      	leaq	0x1ba8(%rsp), %rdi
ffffffff800159ce:      	leaq	0x13a0(%rsp), %rsi
ffffffff800159d6:      	movl	$0x800, %edx    # imm = 0x800

; PART 2: the child Process placement-write. Every other field gets correct
; movl/movq/memcpy stores — but name @0x20..0x2f again receives only the
; 8 even bytes. The child frame is NOT pre-zeroed: odd name bytes = garbage.
ffffffff80015ab9:      	movq	$0x0, 0xc0(%rsp)
ffffffff80015ac5:      	movl	0x20(%rsp), %eax
ffffffff80015ac9:      	movl	%eax, (%r15,%r14)
ffffffff80015acd:      	movb	$0x0, 0x4(%r15,%r14)
ffffffff80015ad3:      	movl	$0x0, 0x8(%r15,%r14)
ffffffff80015adc:      	movq	0x28(%rsp), %rax
ffffffff80015ae1:      	movq	%rax, 0x10(%r15,%r14)
ffffffff80015ae6:      	movq	0x38(%rsp), %rax
ffffffff80015aeb:      	movq	%rax, 0x18(%r15,%r14)
ffffffff80015af0:      	movzbl	0x86e(%rsp), %eax
ffffffff80015af8:      	movb	%al, 0x2e(%r15,%r14)
ffffffff80015afd:      	movzbl	0x86c(%rsp), %eax
ffffffff80015b05:      	movb	%al, 0x2c(%r15,%r14)
ffffffff80015b0a:      	movzbl	0x86a(%rsp), %eax
ffffffff80015b12:      	movb	%al, 0x2a(%r15,%r14)
ffffffff80015b17:      	movzbl	0x868(%rsp), %eax
ffffffff80015b1f:      	movb	%al, 0x28(%r15,%r14)
ffffffff80015b24:      	movzbl	0x866(%rsp), %eax
ffffffff80015b2c:      	movb	%al, 0x26(%r15,%r14)
ffffffff80015b31:      	movzbl	0x864(%rsp), %eax
ffffffff80015b39:      	movb	%al, 0x24(%r15,%r14)
ffffffff80015b3e:      	movzbl	0x860(%rsp), %eax
ffffffff80015b46:      	movzbl	0x862(%rsp), %ecx
ffffffff80015b4e:      	movb	%cl, 0x22(%r15,%r14)
ffffffff80015b53:      	movb	%al, 0x20(%r15,%r14)
ffffffff80015b58:      	movq	0x80(%rsp), %rax
ffffffff80015b60:      	movq	%rax, 0x30(%r15,%r14)
ffffffff80015b65:      	movq	0x78(%rsp), %rax
ffffffff80015b6a:      	movq	%rax, 0x38(%r15,%r14)
ffffffff80015b6f:      	movq	0x70(%rsp), %rax
ffffffff80015b74:      	movq	%rax, 0x40(%r15,%r14)
