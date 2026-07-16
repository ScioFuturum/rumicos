//! Host runner for the miscompile repro matrix. Exit code = number of
//! cases that observed corruption (0 = every aggregate copy compiled
//! correctly in THIS build; see the caveats printed below and in
//! docs/miscompile-audit.md — a clean host run does NOT clear the
//! x86_64-unknown-none build).

use miscompile_repro::CASES;

fn main() {
    println!(
        "miscompile-repro  |  rustc profile: opt-level={} (see Cargo.toml)  |  target: {}",
        option_env!("REPRO_PROFILE").unwrap_or("release(kernel-mirror)"),
        std::env::consts::ARCH,
    );
    println!("seeds: 5 per case; a case FAILS if any seed observes corrupted elements\n");

    let seeds = [1u32, 0x1234_5678, 0xDEAD_BEEF, 0x0BAD_F00D, u32::MAX];
    let mut failing_cases = 0;

    for (name, f) in CASES {
        let mut worst = 0u64;
        for &s in &seeds {
            worst = worst.max(f(s));
        }
        if worst == 0 {
            println!("PASS       {name}");
        } else {
            println!("CORRUPTED  {name}  ({worst} bad elements, worst seed)");
            failing_cases += 1;
        }
    }

    println!(
        "\n{failing_cases} of {} cases corrupted on this host build.",
        CASES.len()
    );
    println!(
        "NOTE: host target ({}) and x86_64-unknown-none take different LLVM\n\
         lowering paths; a clean host run narrows but does not clear the kernel\n\
         build. The .s files emitted by run-matrix are the target-side evidence.",
        std::env::consts::OS
    );
    std::process::exit(failing_cases);
}
