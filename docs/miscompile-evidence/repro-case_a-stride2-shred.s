; Excerpt from the isolated repro staticlib built for x86_64-unknown-none with
; the kernel's exact profile (opt3/fat-LTO/cgu1/panic=abort) and rustflags
; (crucially -C target-feature=+cmpxchg16b,+sse4.2,+xsave):
;   miscompile-repro: cargo build --release --lib --target x86_64-unknown-none --no-default-features
;
; case_a_snapshot_guard_u32_32 = bug #2's exact shape: 'let snap = *guard' on
; SpinLock<[u32; 32]>. The 128-byte array copy is emitted as 128 single-byte
; loads stored at STRIDE-2 stack offsets (smeared over 256 bytes; only even
; offsets written), while the later element-wise verify reads dense offsets.
; The same build with '-C target-feature=+sse4.2,-soft-float' (or without
; +sse*) emits a correct dense copy. First 30 of 128 shredded stores:
     353:      	jne	0x2f3 <case_a_snapshot_guard_u32_32+0x2f3>
     355:      	movzbl	(%rip), %eax            # 0x35c <case_a_snapshot_guard_u32_32+0x35c>
     35c:      	movb	%al, -0x32(%rbp)
     35f:      	movzbl	(%rip), %eax            # 0x366 <case_a_snapshot_guard_u32_32+0x366>
     366:      	movb	%al, -0x34(%rbp)
     369:      	movzbl	(%rip), %eax            # 0x370 <case_a_snapshot_guard_u32_32+0x370>
     370:      	movb	%al, -0x36(%rbp)
     373:      	movzbl	(%rip), %eax            # 0x37a <case_a_snapshot_guard_u32_32+0x37a>
     37a:      	movb	%al, -0x38(%rbp)
     37d:      	movzbl	(%rip), %eax            # 0x384 <case_a_snapshot_guard_u32_32+0x384>
     384:      	movb	%al, -0x3a(%rbp)
     387:      	movzbl	(%rip), %eax            # 0x38e <case_a_snapshot_guard_u32_32+0x38e>
     38e:      	movb	%al, -0x3c(%rbp)
     391:      	movzbl	(%rip), %eax            # 0x398 <case_a_snapshot_guard_u32_32+0x398>
     398:      	movb	%al, -0x3e(%rbp)
     39b:      	movzbl	(%rip), %eax            # 0x3a2 <case_a_snapshot_guard_u32_32+0x3a2>
     3a2:      	movb	%al, -0x40(%rbp)
     3a5:      	movzbl	(%rip), %eax            # 0x3ac <case_a_snapshot_guard_u32_32+0x3ac>
     3ac:      	movb	%al, -0x42(%rbp)
     3af:      	movzbl	(%rip), %eax            # 0x3b6 <case_a_snapshot_guard_u32_32+0x3b6>
     3b6:      	movb	%al, -0x44(%rbp)
     3b9:      	movzbl	(%rip), %eax            # 0x3c0 <case_a_snapshot_guard_u32_32+0x3c0>
     3c0:      	movb	%al, -0x46(%rbp)
     3c3:      	movzbl	(%rip), %eax            # 0x3ca <case_a_snapshot_guard_u32_32+0x3ca>
     3ca:      	movb	%al, -0x48(%rbp)
     3cd:      	movzbl	(%rip), %eax            # 0x3d4 <case_a_snapshot_guard_u32_32+0x3d4>
     3d4:      	movb	%al, -0x4a(%rbp)
     3d7:      	movzbl	(%rip), %eax            # 0x3de <case_a_snapshot_guard_u32_32+0x3de>
     3de:      	movb	%al, -0x4c(%rbp)
     3e1:      	movzbl	(%rip), %eax            # 0x3e8 <case_a_snapshot_guard_u32_32+0x3e8>
