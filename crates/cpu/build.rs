use std::env;
use std::fmt::Write;
use std::fs;
use std::path::PathBuf;

const ERROR_CODE_VECTORS: &[usize] = &[8, 10, 11, 12, 13, 14, 17, 21, 29, 30];

fn main() {
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set by Cargo"));
    let asm_path = out_dir.join("idt_stubs.S");
    fs::write(asm_path, generate_idt_stubs()).expect("write generated IDT stubs");
}

fn generate_idt_stubs() -> String {
    let mut asm = String::with_capacity(48 * 1024);
    writeln!(asm, ".section .text.interrupt_stubs,\"ax\",@progbits").unwrap();
    writeln!(asm, ".global __interrupt_common").unwrap();
    writeln!(asm, ".type __interrupt_common, @function").unwrap();
    writeln!(asm, ".align 16").unwrap();
    writeln!(asm, "__interrupt_common:").unwrap();
    emit_endbr64(&mut asm);
    writeln!(asm, "    cld").unwrap();
    writeln!(asm, "    push r15").unwrap();
    writeln!(asm, "    push r14").unwrap();
    writeln!(asm, "    push r13").unwrap();
    writeln!(asm, "    push r12").unwrap();
    writeln!(asm, "    push r11").unwrap();
    writeln!(asm, "    push r10").unwrap();
    writeln!(asm, "    push r9").unwrap();
    writeln!(asm, "    push r8").unwrap();
    writeln!(asm, "    push rdx").unwrap();
    writeln!(asm, "    push rcx").unwrap();
    writeln!(asm, "    push rbx").unwrap();
    writeln!(asm, "    push rax").unwrap();
    writeln!(asm, "    push rbp").unwrap();
    writeln!(asm, "    push rsi").unwrap();
    writeln!(asm, "    push rdi").unwrap();
    writeln!(asm, "    mov rdi, rsp").unwrap();
    writeln!(asm, "    mov rsi, qword ptr [rsp + 120]").unwrap();
    writeln!(asm, "    xor eax, eax").unwrap();
    writeln!(asm, "    test qword ptr [rsp + 144], 3").unwrap();
    writeln!(asm, "    jz 1f").unwrap();
    writeln!(asm, "    swapgs").unwrap();
    writeln!(asm, "    mov eax, 1").unwrap();
    writeln!(asm, "1:").unwrap();
    writeln!(asm, "    sub rsp, 8").unwrap();
    writeln!(asm, "    mov qword ptr [rsp], rax").unwrap();
    writeln!(asm, "    call rust_interrupt_dispatch").unwrap();
    writeln!(asm, "    mov rax, qword ptr [rsp]").unwrap();
    writeln!(asm, "    add rsp, 8").unwrap();
    writeln!(asm, "    test rax, rax").unwrap();
    writeln!(asm, "    jz 2f").unwrap();
    writeln!(asm, "    swapgs").unwrap();
    writeln!(asm, "2:").unwrap();
    writeln!(asm, "    pop rdi").unwrap();
    writeln!(asm, "    pop rsi").unwrap();
    writeln!(asm, "    pop rbp").unwrap();
    writeln!(asm, "    pop rax").unwrap();
    writeln!(asm, "    pop rbx").unwrap();
    writeln!(asm, "    pop rcx").unwrap();
    writeln!(asm, "    pop rdx").unwrap();
    writeln!(asm, "    pop r8").unwrap();
    writeln!(asm, "    pop r9").unwrap();
    writeln!(asm, "    pop r10").unwrap();
    writeln!(asm, "    pop r11").unwrap();
    writeln!(asm, "    pop r12").unwrap();
    writeln!(asm, "    pop r13").unwrap();
    writeln!(asm, "    pop r14").unwrap();
    writeln!(asm, "    pop r15").unwrap();
    writeln!(asm, "    add rsp, 16").unwrap();
    writeln!(asm, "    iretq").unwrap();
    writeln!(asm, ".size __interrupt_common, . - __interrupt_common").unwrap();

    for vector in 0..256 {
        writeln!(asm, ".global __interrupt_stub_{vector}").unwrap();
        writeln!(asm, ".type __interrupt_stub_{vector}, @function").unwrap();
        writeln!(asm, ".align 16").unwrap();
        writeln!(asm, "__interrupt_stub_{vector}:").unwrap();
        emit_endbr64(&mut asm);
        if ERROR_CODE_VECTORS.contains(&vector) {
            writeln!(asm, "    push {vector}").unwrap();
        } else {
            writeln!(asm, "    push 0").unwrap();
            writeln!(asm, "    push {vector}").unwrap();
        }
        writeln!(asm, "    jmp __interrupt_common").unwrap();
        writeln!(
            asm,
            ".size __interrupt_stub_{vector}, . - __interrupt_stub_{vector}"
        )
        .unwrap();
    }

    writeln!(asm, ".section .rodata.interrupt_stub_table,\"a\",@progbits").unwrap();
    writeln!(asm, ".global __interrupt_stub_table").unwrap();
    writeln!(asm, ".align 8").unwrap();
    writeln!(asm, "__interrupt_stub_table:").unwrap();
    for vector in 0..256 {
        writeln!(asm, "    .quad __interrupt_stub_{vector}").unwrap();
    }

    asm
}

fn emit_endbr64(asm: &mut String) {
    writeln!(asm, "    .byte 0xf3, 0x0f, 0x1e, 0xfa").unwrap();
}
