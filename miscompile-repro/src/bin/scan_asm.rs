//! Scan an llvm-objdump disassembly for the rustc 1.97.0 miscompile
//! signature observed in this kernel: an aggregate copy scalarized into a
//! run of byte loads/stores whose displacements step by 2 (only the
//! even-offset — or only the odd-offset — bytes of the value are moved,
//! the skipped bytes being wrongly treated as padding).
//!
//! Usage: scan_asm <disassembly.s>
//!
//! Reports every run of >= MIN_RUN register-source `movb` stores to the
//! same base register whose displacements form a |step|=2 progression,
//! with the enclosing function name. Constant stores (`movb $0x0, ...`)
//! are ignored — zeroing loops legitimately look like that.

use std::env;
use std::fs;

const MIN_RUN: usize = 3;
/// Max intervening non-matching lines inside one run (the real hit
/// interleaves each store with its paired `movzbl` load).
const MAX_GAP: usize = 3;

#[derive(Debug)]
struct ByteStore {
    line_no: usize,
    addr: String,
    base: String,
    disp: i64,
}

fn parse_disp_base(operand: &str) -> Option<(i64, String)> {
    // "0x86e(%rsp)" | "-0x30(%rcx)" | "0x4(%r15,%r14)" | "(%rax)"
    let open = operand.find('(')?;
    let close = operand.rfind(')')?;
    let disp_str = &operand[..open];
    let base = operand[open + 1..close].to_string();
    let disp = if disp_str.is_empty() {
        0
    } else if let Some(hex) = disp_str.strip_prefix("-0x") {
        -i64::from_str_radix(hex, 16).ok()?
    } else if let Some(hex) = disp_str.strip_prefix("0x") {
        i64::from_str_radix(hex, 16).ok()?
    } else {
        disp_str.parse().ok()?
    };
    Some((disp, base))
}

fn main() {
    let path = env::args().nth(1).expect("usage: scan_asm <disassembly.s>");
    let text = fs::read_to_string(&path).expect("read disassembly");

    let mut current_fn = String::from("<unknown>");
    let mut stores: Vec<ByteStore> = Vec::new();
    let mut findings = 0usize;

    let mut flush = |func: &str, stores: &mut Vec<ByteStore>, findings: &mut usize| {
        // Find maximal appearance-order runs: same base, |disp step| == 2,
        // line gap <= MAX_GAP between consecutive members.
        let mut i = 0;
        while i < stores.len() {
            let mut j = i;
            while j + 1 < stores.len() {
                let a = &stores[j];
                let b = &stores[j + 1];
                let step_ok = (b.disp - a.disp).abs() == 2;
                let near = b.line_no - a.line_no <= MAX_GAP;
                if a.base == b.base && step_ok && near {
                    j += 1;
                } else {
                    break;
                }
            }
            let run = &stores[i..=j];
            if run.len() >= MIN_RUN {
                *findings += 1;
                let offs: Vec<String> =
                    run.iter().map(|s| format!("{:#x}", s.disp)).collect();
                println!(
                    "SUSPECT stride-2 byte-store run ({} stores) in\n  {}\n  base ({}), offsets: {}\n  first at {}\n",
                    run.len(),
                    func,
                    run[0].base,
                    offs.join(" "),
                    run[0].addr
                );
            }
            i = j + 1;
        }
        stores.clear();
    };

    for (line_no, line) in text.lines().enumerate() {
        // Function label: "ffffffff80013d60 <name>:"
        if line.ends_with(">:") {
            if let Some(open) = line.find('<') {
                flush(&current_fn, &mut stores, &mut findings);
                current_fn = line[open + 1..line.len() - 2].to_string();
                continue;
            }
        }
        // Register-source byte store: "addr:   \tmovb\t%al, DEST"
        let Some((addr_part, rest)) = line.split_once(':') else {
            continue;
        };
        if addr_part.trim().chars().any(|c| !c.is_ascii_hexdigit()) {
            continue;
        }
        let rest = rest.trim();
        let Some(operands) = rest
            .strip_prefix("movb\t")
            .or_else(|| rest.strip_prefix("movb "))
        else {
            continue;
        };
        let mut parts = operands.splitn(2, ',');
        let (Some(src), Some(dst)) = (parts.next(), parts.next()) else {
            continue;
        };
        let src = src.trim();
        let dst = dst.trim();
        if !src.starts_with('%') {
            continue; // constant store — zeroing loops, not the signature
        }
        if let Some((disp, base)) = parse_disp_base(dst) {
            stores.push(ByteStore {
                line_no,
                addr: addr_part.trim().to_string(),
                base,
                disp,
            });
        }
    }
    flush(&current_fn, &mut stores, &mut findings);

    println!("{findings} suspect stride-2 byte-store run(s) found.");
    std::process::exit(if findings > 0 { 1 } else { 0 });
}
