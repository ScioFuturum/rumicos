#![cfg_attr(target_os = "none", no_std)]
#![cfg_attr(target_os = "none", no_main)]

#[cfg(not(target_os = "none"))]
use std::env;
#[cfg(not(target_os = "none"))]
use std::ffi::OsString;
#[cfg(not(target_os = "none"))]
use std::path::{Path, PathBuf};
#[cfg(not(target_os = "none"))]
use std::process::{Command, ExitCode};

#[cfg(not(target_os = "none"))]
fn main() -> ExitCode {
    let mut args = env::args_os();
    let _exe = args.next();
    let command = args.next().unwrap_or_else(|| OsString::from("help"));

    let result = match command.to_string_lossy().as_ref() {
        "build" => cargo_kernel(false),
        "release" => cargo_kernel(true),
        "check" => cargo_check(),
        "bench" => bench(),
        "pgo" => pgo(),
        "qemu" => qemu(args.collect()),
        "qemu-test" => qemu_test(),
        "qemu-test-keyboard" => qemu_test_keyboard(),
        "mkinitrd" => mkinitrd(),
        "build-userspace" => build_userspace(),
        _ => help(),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{err}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(target_os = "none")]
#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    loop {
        core::hint::spin_loop();
    }
}

#[cfg(target_os = "none")]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    loop {
        core::hint::spin_loop();
    }
}

#[cfg(not(target_os = "none"))]
fn help() -> Result<(), String> {
    println!("xtask commands:");
    println!("  check        cargo check kernel for x86_64-unknown-none");
    println!("  build        debug kernel build");
    println!("  release      release kernel build");
    println!("  bench        print/run perf-oriented benchmark pipeline when available");
    println!("  pgo          instrumented build, workload hook, optimized rebuild");
    println!("  qemu [elf]   boot the kernel via Limine/UEFI in QEMU q35 (KVM/WHPX with TCG fallback)");
    println!("  qemu-test    boot in QEMU headless and check serial output against tests/expected-boot.txt");
    println!("  qemu-test-keyboard  boot, then drive the shell via QEMU monitor `sendkey` and check the typed command ran");
    println!("  mkinitrd     build userspace + pack initrd/initrd.cpio (cpio-free)");
    println!("  build-userspace  build /bin/shell, /bin/echo, /bin/cat for x86_64-unknown-none");
    Ok(())
}

// ─── initrd (newc CPIO) packing ────────────────────────────────────────────

/// The userspace link flags, which MUST be passed through the RUSTFLAGS
/// *environment variable* rather than a `userspace/.cargo/config.toml`.
///
/// Cargo merges config files up the directory tree, so a nested config's
/// `rustflags` are appended to — not substituted for — the repo root's,
/// which pin `-C link-arg=-Tkernel/linker/x86_64.ld` (the KERNEL's
/// higher-half linker script) onto every x86_64-unknown-none build. Only
/// the RUSTFLAGS env var replaces config rustflags outright.
///
/// * `relocation-model=static` — the ELF loader rejects anything whose
///   `e_type != ET_EXEC` (`ElfError::NotExecutable`), i.e. no PIE.
/// * `--image-base=0x400000` — pins the image where init.asm/init2.asm
///   already live. The historical GNU-ld recipe spelled this
///   `-Ttext-segment=0x400000`; rust-lld rejects that spelling.
/// * `-z noseparate-code` — same intent as the NASM recipe's flag: keep the
///   program-header count down. The loader caps `e_phnum` at 8.
/// * `-z norelro` / `--no-eh-frame-hdr` — drop two phdrs nothing uses in a
///   static, non-unwinding image.
///
/// Deliberately no `+sse*`: x86_64-unknown-none defaults to soft-float with
/// no SSE, the configuration that is NOT affected by the rustc 1.97.0
/// aggregate-copy miscompile (docs/miscompile-audit.md).
#[cfg(not(target_os = "none"))]
const USERSPACE_RUSTFLAGS: &str = "-C relocation-model=static \
     -C link-arg=--image-base=0x400000 \
     -C link-arg=-z -C link-arg=noseparate-code \
     -C link-arg=-z -C link-arg=norelro \
     -C link-arg=--no-eh-frame-hdr";

/// `cargo xtask build-userspace`: build /bin/shell, /bin/echo and /bin/cat
/// for x86_64-unknown-none. See [`USERSPACE_RUSTFLAGS`] for why the flags
/// go through the environment.
#[cfg(not(target_os = "none"))]
fn build_userspace() -> Result<(), String> {
    let mut cmd = cargo();
    cmd.current_dir("userspace")
        .env("RUSTFLAGS", USERSPACE_RUSTFLAGS)
        .args([
            "build",
            "--target",
            "x86_64-unknown-none",
            "--release",
            "-p",
            "uecho",
            "-p",
            "ucat",
            "-p",
            "shell",
            "--features",
            "shell/bin",
        ]);
    run(&mut cmd)
}

/// `cargo xtask mkinitrd`: rebuild the userspace binaries and repack
/// `initrd/initrd.cpio` in the newc (SVR4, no-CRC) layout
/// `crates/fs/src/cpio.rs` parses. Exists because this project's Windows dev
/// box has neither `cpio` nor a real `python3` (the Store stub is inert) —
/// see initrd/Makefile and the memory `windows-toolchain-quirks`.
///
/// Archive layout:
/// * `tmp/` — an empty directory. `sys_open`'s O_CREAT path creates a file
///   only inside an EXISTING parent, so the shell's `> /tmp/shellout.txt`
///   needs this to be here.
/// * `bin/{shell,echo,cat}` — the unpacker's `ensure_path` builds the `bin`
///   directory automatically from the `/` in these names.
/// * `init2.elf`, `testfile.txt` — the pre-existing demo payload.
#[cfg(not(target_os = "none"))]
fn mkinitrd() -> Result<(), String> {
    build_userspace()?;

    let dir = Path::new("initrd");
    let user = Path::new("userspace/target/x86_64-unknown-none/release");
    let mut archive: Vec<u8> = Vec::new();

    // Directories first, so the unpacker has them before any file lands in
    // one (it would create them implicitly anyway, but an explicit entry is
    // what makes an EMPTY directory like /tmp exist at all).
    for d in ["tmp", "bin"] {
        cpio_newc_record(&mut archive, d.as_bytes(), 0o040755, &[]);
    }

    // (name inside the archive, file on disk, mode)
    let members: [(&str, std::path::PathBuf, u32); 5] = [
        ("init2.elf", dir.join("init2.elf"), 0o100755),
        ("testfile.txt", dir.join("testfile.txt"), 0o100644),
        ("bin/shell", user.join("shell"), 0o100755),
        ("bin/echo", user.join("echo"), 0o100755),
        ("bin/cat", user.join("cat"), 0o100755),
    ];
    for (name, src, mode) in members {
        let data = std::fs::read(&src).map_err(|e| {
            format!(
                "read {}: {e} (run `cargo xtask build-userspace` first?)",
                src.display()
            )
        })?;
        cpio_newc_record(&mut archive, name.as_bytes(), mode, &data);
        println!("  {name}: {} bytes", data.len());
    }

    // Terminating record: name "TRAILER!!!", mode 0, no data.
    cpio_newc_record(&mut archive, b"TRAILER!!!", 0, &[]);

    let out = dir.join("initrd.cpio");
    std::fs::write(&out, &archive).map_err(|e| format!("write {}: {e}", out.display()))?;
    println!("{}: {} bytes", out.display(), archive.len());
    Ok(())
}

/// Append one newc record (110-byte ASCII-hex header, NUL-terminated name,
/// 4-byte-aligned name and data) to `out`.
#[cfg(not(target_os = "none"))]
fn cpio_newc_record(out: &mut Vec<u8>, name: &[u8], mode: u32, data: &[u8]) {
    fn field(out: &mut Vec<u8>, v: u32) {
        // 8-char uppercase hex ASCII, matching cpio's newc encoding.
        out.extend_from_slice(format!("{v:08X}").as_bytes());
    }
    fn pad4(out: &mut Vec<u8>, len: usize) {
        for _ in 0..((4 - (len & 3)) & 3) {
            out.push(0);
        }
    }
    let namesize = name.len() as u32 + 1; // includes the trailing NUL
    let start = out.len();
    out.extend_from_slice(b"070701"); // newc magic
    field(out, 0); // ino (parser ignores)
    field(out, mode);
    field(out, 0); // uid
    field(out, 0); // gid
    field(out, 1); // nlink
    field(out, 0); // mtime
    field(out, data.len() as u32); // filesize
    field(out, 0); // devmajor
    field(out, 0); // devminor
    field(out, 0); // rdevmajor
    field(out, 0); // rdevminor
    field(out, namesize);
    field(out, 0); // check (0 for non-CRC newc)
    debug_assert_eq!(out.len() - start, 110);
    out.extend_from_slice(name);
    out.push(0);
    pad4(out, 110 + namesize as usize);
    out.extend_from_slice(data);
    pad4(out, data.len());
}

#[cfg(not(target_os = "none"))]
fn cargo() -> Command {
    Command::new(env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo")))
}

#[cfg(not(target_os = "none"))]
fn cargo_check() -> Result<(), String> {
    let mut cmd = cargo();
    cmd.args(["check", "-p", "kernel", "--target", "x86_64-unknown-none"]);
    run(&mut cmd)
}

#[cfg(not(target_os = "none"))]
fn cargo_kernel(release: bool) -> Result<(), String> {
    // The kernel embeds initrd/initrd.cpio via include_bytes!, and the
    // initrd now carries the userspace binaries — so it must be rebuilt and
    // repacked BEFORE the kernel, or a boot test could silently run a stale
    // /bin/shell. cargo's include_bytes! dependency tracking then rebuilds
    // the kernel whenever the archive actually changed.
    mkinitrd()?;

    let mut cmd = cargo();
    cmd.args(["build", "-p", "kernel", "--target", "x86_64-unknown-none"]);
    if release {
        cmd.arg("--release");
    }
    run(&mut cmd)
}

#[cfg(not(target_os = "none"))]
fn bench() -> Result<(), String> {
    cargo_kernel(true)?;
    println!("Kernel built. Suggested Linux perf run:");
    println!(
        "  perf stat -e cycles,instructions,branches,branch-misses,cache-misses cargo bench --workspace"
    );
    println!(
        "Cycle-accurate in-kernel benches should bracket code with RDTSCP from kernel-arch-x86_64."
    );
    Ok(())
}

#[cfg(not(target_os = "none"))]
fn pgo() -> Result<(), String> {
    let profile_dir = PathBuf::from("target/pgo");
    std::fs::create_dir_all(&profile_dir).map_err(|err| err.to_string())?;

    let mut instrumented = cargo();
    instrumented
        .env(
            "RUSTFLAGS",
            format!("-Cprofile-generate={}", profile_dir.display()),
        )
        .args([
            "build",
            "-p",
            "kernel",
            "--target",
            "x86_64-unknown-none",
            "--release",
        ]);
    run(&mut instrumented)?;

    println!(
        "Run the instrumented kernel workload now, then merge .profraw files with llvm-profdata."
    );
    println!("Example:");
    println!("  llvm-profdata merge -o target/pgo/kernel.profdata target/pgo/*.profraw");

    let profdata = profile_dir.join("kernel.profdata");
    if profdata.exists() {
        let mut optimized = cargo();
        optimized
            .env("RUSTFLAGS", format!("-Cprofile-use={}", profdata.display()))
            .args([
                "build",
                "-p",
                "kernel",
                "--target",
                "x86_64-unknown-none",
                "--release",
            ]);
        run(&mut optimized)?;
    }

    Ok(())
}

#[cfg(not(target_os = "none"))]
fn qemu(args: Vec<OsString>) -> Result<(), String> {
    let kernel = args
        .first()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/x86_64-unknown-none/release/kernel"));
    if !Path::new(&kernel).exists() {
        cargo_kernel(true)?;
    }
    let esp = prepare_esp(&kernel)?;
    let qemu_bin = qemu_binary()?;
    let ovmf = find_ovmf()?;

    let mut cmd = Command::new(qemu_bin);
    // Interactive run keeps the WHPX/KVM-with-TCG-fallback accel list;
    // qemu_test() below picks the accelerator explicitly instead, so the
    // regression check's timeout budget is predictable.
    let machine = if cfg!(target_os = "windows") {
        "q35,accel=whpx:tcg"
    } else {
        "q35,accel=kvm:tcg"
    };
    qemu_common_args(&mut cmd, machine, "max", &ovmf, &esp);
    cmd.args(["-serial", "stdio"]);
    run(&mut cmd)
}

/// `cargo xtask qemu-test`: boot the kernel headless and require every
/// line of tests/expected-boot.txt to appear (in order) on the serial
/// port within a timeout. This is the CI regression gate for the class of
/// boot/hardware/linker/asm bugs that host unit tests structurally cannot
/// catch (see docs in tests/expected-boot.txt and README "CI / Testing").
///
/// The kernel never exits after its self-tests — it idles — so this does
/// NOT wait for QEMU to terminate: it polls the serial log until every
/// expected line has matched (success, kill QEMU) or the timeout expires
/// (failure, kill QEMU, print what was missing).
#[cfg(not(target_os = "none"))]
fn qemu_test() -> Result<(), String> {
    use std::time::{Duration, Instant};

    cargo_kernel(true)?;
    let kernel = PathBuf::from("target/x86_64-unknown-none/release/kernel");
    let esp = prepare_esp(&kernel)?;
    let qemu_bin = qemu_binary()?;
    let ovmf = find_ovmf()?;

    let spec = std::fs::read_to_string("tests/expected-boot.txt")
        .map_err(|err| format!("read tests/expected-boot.txt: {err}"))?;
    let expected = parse_expected(&spec);
    if expected.is_empty() {
        return Err("tests/expected-boot.txt contains no expected lines".into());
    }

    // KVM on Linux when /dev/kvm is present; plain TCG everywhere else.
    // (`-cpu host` needs a hardware accelerator; `max` is the proven TCG
    // model — qemu64 lacks features this kernel probes for, e.g. PCID.)
    let use_kvm = cfg!(target_os = "linux") && Path::new("/dev/kvm").exists();
    let (cpu, accel_name) = if use_kvm { ("host", "kvm") } else { ("max", "tcg") };
    // TCG's translation buffer defaults to ~1 GiB backed by a single
    // contiguous VirtualAlloc; on a Windows host with a small page file that
    // allocation fails outright ("allocate ... bytes for jit buffer: the
    // paging file is too small"), before the kernel ever runs. `tb-size`
    // (MiB) caps it — 128 MiB is ample for this kernel and lets the gate run
    // on a constrained host. Passed via `-accel` (accel is configured there,
    // not on `-machine`, so the two forms do not conflict). Overridable via
    // RUMICOS_QEMU_TB_SIZE; ignored by KVM.
    let tb_size = env::var("RUMICOS_QEMU_TB_SIZE")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(128);
    let accel = if use_kvm {
        "kvm".to_string()
    } else {
        format!("tcg,tb-size={tb_size}")
    };
    let machine = "q35";

    let timeout_secs: u64 = env::var("RUMICOS_QEMU_TEST_TIMEOUT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);

    let log_path = PathBuf::from("target/qemu-test-serial.log");
    let _ = std::fs::remove_file(&log_path);

    let mut cmd = Command::new(&qemu_bin);
    qemu_common_args(&mut cmd, machine, cpu, &ovmf, &esp);
    cmd.arg("-accel");
    cmd.arg(&accel);
    cmd.arg("-serial");
    cmd.arg(format!("file:{}", log_path.display()));
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    // stderr inherited on purpose: QEMU's own startup errors (bad args,
    // missing firmware) should reach the CI log verbatim.

    eprintln!(
        "qemu-test: booting with accel={accel_name}, timeout {timeout_secs}s, \
         expecting {} lines",
        expected.len()
    );
    let mut child = cmd
        .spawn()
        .map_err(|err| format!("spawn {}: {err}", qemu_bin.display()))?;

    let started = Instant::now();
    let deadline = started + Duration::from_secs(timeout_secs);
    let mut log_text = String::new();
    let mut qemu_died = None;

    loop {
        std::thread::sleep(Duration::from_millis(300));
        if let Ok(raw) = std::fs::read(&log_path) {
            log_text = strip_serial_noise(&raw);
            if match_expected(&log_text, &expected).iter().all(Option::is_some) {
                let _ = child.kill();
                let _ = child.wait();
                eprintln!(
                    "qemu-test: OK — all {} expected lines matched in {:.1}s (accel={accel_name})",
                    expected.len(),
                    started.elapsed().as_secs_f32()
                );
                return Ok(());
            }
        }
        // QEMU exiting on its own is always a failure here (the kernel
        // idles forever after its tests): triple-fault reset, firmware
        // error, or bad invocation. Fall through to the report.
        if let Ok(Some(status)) = child.try_wait() {
            qemu_died = Some(status);
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break;
        }
    }

    // Failure: report exactly which expected lines never showed up.
    let matches = match_expected(&log_text, &expected);
    let mut report = String::new();
    for (exp, m) in expected.iter().zip(&matches) {
        let mark = if m.is_some() { "  ok " } else { "MISS " };
        report.push_str(&format!("{mark} {}\n", exp.text));
    }
    let tail: Vec<&str> = log_text.lines().rev().take(12).collect();
    let tail: Vec<&str> = tail.into_iter().rev().collect();
    let cause = match qemu_died {
        Some(status) => format!("QEMU exited early ({status}) — likely triple fault or boot failure"),
        None => format!("TIMEOUT after {timeout_secs}s — kernel may be hung"),
    };
    Err(format!(
        "qemu-test FAILED: {cause}\n\nexpected-line status:\n{report}\nlast serial output:\n  {}\n\n\
         full serial log: {}",
        tail.join("\n  "),
        log_path.display()
    ))
}

/// `cargo xtask qemu-test-keyboard`: boot headless, wait for the shell's
/// interactive prompt, then drive the PS/2 keyboard end-to-end by sending
/// `sendkey` commands through QEMU's monitor (a TCP socket, portable to the
/// Windows dev host) — no human at a graphical console. Verifies the typed
/// command both echoed (input reached read_line) and executed (its output
/// appeared), proving the whole I/O APIC → IRQ1 → ring → blocking-read →
/// shell path works.
///
/// Kept SEPARATE from `qemu-test`: it needs a live monitor connection and
/// per-keystroke timing, so it is inherently slower and a touch more
/// timing-sensitive than the deterministic serial-only gate. `qemu-test`
/// stays the headless, keyboard-independent CI gate; this is a targeted
/// verification command (run it by hand, or add it to CI once its timing
/// has proven stable on the runner).
#[cfg(not(target_os = "none"))]
fn qemu_test_keyboard() -> Result<(), String> {
    use std::io::Write;
    use std::net::TcpStream;
    use std::time::{Duration, Instant};

    cargo_kernel(true)?;
    let kernel = PathBuf::from("target/x86_64-unknown-none/release/kernel");
    let esp = prepare_esp(&kernel)?;
    let qemu_bin = qemu_binary()?;
    let ovmf = find_ovmf()?;

    let use_kvm = cfg!(target_os = "linux") && Path::new("/dev/kvm").exists();
    let (cpu, accel_name) = if use_kvm { ("host", "kvm") } else { ("max", "tcg") };
    let tb_size = env::var("RUMICOS_QEMU_TB_SIZE")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(128);
    let accel = if use_kvm {
        "kvm".to_string()
    } else {
        format!("tcg,tb-size={tb_size}")
    };

    // The command we "type" and the marker we look for in its OUTPUT. Absolute
    // path because the shell has no PATH search. `kbdok` appears TWICE on
    // success: once echoed as the line is typed, once as /bin/echo's output.
    let command = "/bin/echo kbdok";
    let marker = "kbdok";
    let monitor_port: u16 = env::var("RUMICOS_QEMU_MONITOR_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(55_432);

    let log_path = PathBuf::from("target/qemu-test-keyboard-serial.log");
    let _ = std::fs::remove_file(&log_path);

    let mut cmd = Command::new(&qemu_bin);
    qemu_common_args(&mut cmd, "q35", cpu, &ovmf, &esp);
    cmd.arg("-accel");
    cmd.arg(&accel);
    cmd.arg("-serial");
    cmd.arg(format!("file:{}", log_path.display()));
    // Expose the human monitor on a TCP socket QEMU listens on; we connect
    // to it once the guest is up and issue `sendkey` from there.
    cmd.arg("-monitor");
    cmd.arg(format!("tcp:127.0.0.1:{monitor_port},server,nowait"));
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());

    eprintln!("qemu-test-keyboard: booting with accel={accel_name}, monitor on 127.0.0.1:{monitor_port}");
    let mut child = cmd
        .spawn()
        .map_err(|err| format!("spawn {}: {err}", qemu_bin.display()))?;

    // A closure to read the current serial log (empty until QEMU creates it).
    let read_log = || -> String {
        std::fs::read(&log_path)
            .map(|raw| strip_serial_noise(&raw))
            .unwrap_or_default()
    };

    // 1. Wait for the shell's interactive prompt (it prints this right before
    //    it first blocks on the keyboard).
    let boot_deadline = Instant::now() + Duration::from_secs(40);
    loop {
        std::thread::sleep(Duration::from_millis(300));
        if read_log().contains("interactive mode") {
            break;
        }
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!(
                "qemu-test-keyboard FAILED: QEMU exited early ({status}) before the shell prompt"
            ));
        }
        if Instant::now() >= boot_deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err("qemu-test-keyboard FAILED: shell never reached interactive mode".into());
        }
    }

    // 2. Connect to the monitor and "type" the command, one key at a time.
    let mut mon = TcpStream::connect(("127.0.0.1", monitor_port))
        .map_err(|e| format!("connect QEMU monitor: {e}"))?;
    mon.set_write_timeout(Some(Duration::from_secs(2))).ok();
    for token in sendkey_tokens(command) {
        writeln!(mon, "sendkey {token}")
            .map_err(|e| format!("monitor sendkey {token}: {e}"))?;
        mon.flush().ok();
        // A short gap lets the guest's IRQ handler drain each keystroke.
        std::thread::sleep(Duration::from_millis(80));
    }
    // Enter: run the line.
    writeln!(mon, "sendkey ret").map_err(|e| format!("monitor sendkey ret: {e}"))?;
    mon.flush().ok();

    // 3. Wait for the marker to appear TWICE (echoed input + command output).
    let run_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        std::thread::sleep(Duration::from_millis(200));
        let log = read_log();
        if log.matches(marker).count() >= 2 {
            let _ = child.kill();
            let _ = child.wait();
            eprintln!(
                "qemu-test-keyboard: OK — typed `{command}` via sendkey; \
                 `{marker}` echoed and executed (accel={accel_name})"
            );
            return Ok(());
        }
        if Instant::now() >= run_deadline {
            let _ = child.kill();
            let _ = child.wait();
            let log = read_log();
            let tail: Vec<&str> = log.lines().rev().take(12).collect();
            let tail: Vec<&str> = tail.into_iter().rev().collect();
            return Err(format!(
                "qemu-test-keyboard FAILED: `{marker}` did not appear twice after typing \
                 `{command}` — keystrokes may not be reaching the shell.\n\n\
                 last serial output:\n  {}\n\nfull serial log: {}",
                tail.join("\n  "),
                log_path.display()
            ));
        }
    }
}

/// Map each character of `s` to the QEMU `sendkey` token that produces it.
/// Only the characters this checkpoint's test command needs are handled
/// (lower-case letters, digits, and a few symbols); anything else is
/// skipped with a note so the caller notices rather than silently mis-types.
#[cfg(not(target_os = "none"))]
fn sendkey_tokens(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    for c in s.chars() {
        let tok = match c {
            'a'..='z' | '0'..='9' => c.to_string(),
            ' ' => "spc".to_string(),
            '/' => "slash".to_string(),
            '-' => "minus".to_string(),
            '.' => "dot".to_string(),
            other => {
                eprintln!("qemu-test-keyboard: no sendkey mapping for {other:?}, skipping");
                continue;
            }
        };
        out.push(tok);
    }
    out
}

/// Copy the freshly built kernel and limine.conf into the Limine UEFI ESP
/// directory QEMU's vvfat driver boots from, verifying the Limine loader
/// itself is present. The kernel speaks the Limine boot protocol
/// (kernel/src/limine.rs); QEMU's own `-kernel` loader is multiboot-only
/// and cannot start it, hence the ESP indirection.
#[cfg(not(target_os = "none"))]
fn prepare_esp(kernel: &Path) -> Result<PathBuf, String> {
    let esp = PathBuf::from("run/esp");
    let limine_efi = esp.join("EFI/BOOT/BOOTX64.EFI");
    if !limine_efi.exists() {
        return Err(format!(
            "{} not found — download Limine's prebuilt UEFI loader:\n  \
             curl -Lo \"{}\" https://github.com/limine-bootloader/limine/raw/v10.x-binary/BOOTX64.EFI",
            limine_efi.display(),
            limine_efi.display()
        ));
    }
    std::fs::copy("limine.conf", esp.join("limine.conf"))
        .map_err(|err| format!("copy limine.conf into ESP: {err}"))?;
    std::fs::copy(kernel, esp.join("kernel.elf"))
        .map_err(|err| format!("copy {} into ESP: {err}", kernel.display()))?;
    Ok(esp)
}

/// The QEMU arguments shared by the interactive `qemu` command and the
/// `qemu-test` regression check — everything except the accelerator
/// choice and where the serial port goes.
#[cfg(not(target_os = "none"))]
fn qemu_common_args(cmd: &mut Command, machine: &str, cpu: &str, ovmf: &Path, esp: &Path) {
    // Guest RAM defaults to 2G but is overridable via RUMICOS_QEMU_MEM.
    // QEMU on Windows backs guest RAM with a single contiguous VirtualAlloc,
    // which can fail ("cannot set up guest memory 'pc.ram'") on a
    // fragmented host even with several GB free; a smaller value (e.g.
    // "512M") lets the regression test run there. The kernel boots
    // identically regardless — it uses only a few MB.
    let mem = env::var("RUMICOS_QEMU_MEM").unwrap_or_else(|_| "2G".to_string());
    cmd.args(["-machine", machine, "-cpu", cpu, "-smp", "4", "-m", &mem]);
    cmd.args(["-display", "none", "-no-reboot", "-no-shutdown"]);
    cmd.arg("-drive");
    cmd.arg(format!(
        "if=pflash,format=raw,read-only=on,file={}",
        ovmf.display()
    ));
    // vvfat mounted READ-ONLY: the guest only ever needs to read the ESP
    // (kernel.elf + limine.conf are copied in host-side by prepare_esp).
    // The previous `fat:rw:` mount let OVMF persist its NvVars variable
    // store through QEMU's vvfat write path, which is unmaintained and
    // heap-corrupts QEMU itself (host-side 0xc0000005 at a nondeterministic
    // boot stage — observed 2026-07-15, both with a freshly rebuilt kernel
    // and with the previous known-good flag set). Read-only removes that
    // write path entirely; OVMF falls back to volatile in-RAM variables,
    // which this boot flow never depended on.
    cmd.arg("-drive");
    // NB: a plain `read-only=on` drive is rejected by the q35 SATA disk
    // ("Block node is read-only"), so the guest-write path is cut with
    // `snapshot=on` instead: OVMF's NvVars write lands in a throwaway
    // temp overlay, vvfat only ever services reads, and the device still
    // presents as writable. (This is the vvfat-documented recipe.)
    cmd.arg(format!("format=raw,file=fat:{},snapshot=on", esp.display()));

    // A virtio-net-pci device (PCI vendor 0x1AF4) for the network checkpoint:
    // PCI enumeration reports it, and the virtio driver (Part B) binds to it.
    // The user-mode netdev backend needs no host-side configuration.
    cmd.args(["-netdev", "user,id=n0", "-device", "virtio-net-pci,netdev=n0"]);
    // Dump all traffic on that netdev to a pcap, so the virtio-net TX
    // self-test can be verified from the host: a transmitted frame lands in
    // this file (24-byte pcap global header + 16-byte record header + frame).
    cmd.args([
        "-object",
        "filter-dump,id=netdump,netdev=n0,file=target/virtio-capture.pcap",
    ]);
}

/// Locate `qemu-system-x86_64` via PATH, falling back to the default
/// Windows install location. A missing QEMU is an actionable error, not
/// a panic.
#[cfg(not(target_os = "none"))]
fn qemu_binary() -> Result<PathBuf, String> {
    let exe = if cfg!(target_os = "windows") {
        "qemu-system-x86_64.exe"
    } else {
        "qemu-system-x86_64"
    };
    if let Some(paths) = env::var_os("PATH") {
        for dir in env::split_paths(&paths) {
            let candidate = dir.join(exe);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    let win_default = Path::new(r"C:\Program Files\qemu").join(exe);
    if cfg!(target_os = "windows") && win_default.is_file() {
        return Ok(win_default);
    }
    Err(format!(
        "{exe} not found in PATH{} — install QEMU (Linux: your package manager; \
         Windows: winget install SoftwareFreedomConservancy.QEMU) or add it to PATH",
        if cfg!(target_os = "windows") {
            r" or C:\Program Files\qemu"
        } else {
            ""
        }
    ))
}

#[cfg(not(target_os = "none"))]
fn find_ovmf() -> Result<PathBuf, String> {
    if let Some(path) = env::var_os("RUMICOS_OVMF") {
        return Ok(PathBuf::from(path));
    }
    let mut candidates = vec![
        PathBuf::from(r"C:\Program Files\qemu\share\edk2-x86_64-code.fd"),
        PathBuf::from("/usr/share/qemu/edk2-x86_64-code.fd"),
        // Arch/CachyOS edk2-ovmf package layouts, old and new:
        PathBuf::from("/usr/share/edk2/x64/OVMF_CODE.4m.fd"),
        PathBuf::from("/usr/share/edk2/x64/OVMF_CODE.fd"),
        PathBuf::from("/usr/share/edk2-ovmf/x64/OVMF_CODE.fd"),
        PathBuf::from("/usr/share/OVMF/OVMF_CODE_4M.fd"),
        PathBuf::from("/usr/share/OVMF/OVMF_CODE.fd"),
    ];
    // QEMU installs its bundled firmware next to the binary: <dir>/share/.
    if let Ok(qemu) = qemu_binary()
        && let Some(dir) = qemu.parent()
    {
        candidates.insert(0, dir.join("share/edk2-x86_64-code.fd"));
    }
    candidates
        .into_iter()
        .find(|p| p.exists())
        .ok_or_else(|| {
            "UEFI firmware not found (looked for QEMU's bundled edk2-x86_64-code.fd \
             and the usual OVMF paths); set RUMICOS_OVMF to its location"
                .to_string()
        })
}

// ─── expected-boot.txt parsing and matching ───────────────────────────────

/// One line of tests/expected-boot.txt. `unordered` lines (`~` prefix)
/// must still appear at/after the last ordered match, but do not advance
/// the match cursor — used for output whose relative order legitimately
/// depends on scheduling (e.g. the fork test's parent/child interleaving,
/// which differs between otherwise-good boots).
#[cfg(not(target_os = "none"))]
struct ExpectedLine {
    text: String,
    unordered: bool,
}

#[cfg(not(target_os = "none"))]
fn parse_expected(spec: &str) -> Vec<ExpectedLine> {
    spec.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| match l.strip_prefix('~') {
            Some(rest) => ExpectedLine {
                text: rest.trim_start().to_string(),
                unordered: true,
            },
            None => ExpectedLine {
                text: l.to_string(),
                unordered: false,
            },
        })
        .collect()
}

/// Order-sensitive substring matching: each ordered line must appear at or
/// after the previous ordered line's match; unordered (`~`) lines must
/// appear at/after the cursor but leave it unchanged. Returns each line's
/// match position, `None` for misses.
#[cfg(not(target_os = "none"))]
fn match_expected(log: &str, expected: &[ExpectedLine]) -> Vec<Option<usize>> {
    let mut cursor = 0usize;
    let mut out = Vec::with_capacity(expected.len());
    for exp in expected {
        match log[cursor..].find(&exp.text) {
            Some(rel) => {
                let pos = cursor + rel;
                if !exp.unordered {
                    cursor = pos + exp.text.len();
                }
                out.push(Some(pos));
            }
            None => out.push(None),
        }
    }
    out
}

/// Decode the raw serial log, dropping `\r` and ANSI escape sequences
/// (Limine's terminal emits cursor-positioning CSI codes around its own
/// messages) so expected-line matching sees plain text.
#[cfg(not(target_os = "none"))]
fn strip_serial_noise(raw: &[u8]) -> String {
    let s = String::from_utf8_lossy(raw);
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => {
                let mut rest = chars.clone();
                if rest.next() == Some('[') {
                    // CSI: parameter/intermediate bytes end at 0x40..=0x7e.
                    chars = rest;
                    for c2 in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&c2) {
                            break;
                        }
                    }
                } else {
                    // Other escape: swallow the single following char.
                    let _ = chars.next();
                }
            }
            '\r' => {}
            _ => out.push(c),
        }
    }
    out
}

#[cfg(not(target_os = "none"))]
fn run(cmd: &mut Command) -> Result<(), String> {
    eprintln!("running: {cmd:?}");
    let status = cmd.status().map_err(|err| err.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("command failed with status {status}"))
    }
}

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use super::*;

    #[test]
    fn parse_expected_skips_comments_and_blanks_and_reads_unordered_prefix() {
        let spec = "# header comment\n\nfirst line\n~ maybe later\nsecond line\n";
        let parsed = parse_expected(spec);
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].text, "first line");
        assert!(!parsed[0].unordered);
        assert_eq!(parsed[1].text, "maybe later");
        assert!(parsed[1].unordered);
        assert_eq!(parsed[2].text, "second line");
    }

    #[test]
    fn match_expected_is_order_sensitive() {
        let expected = parse_expected("alpha\nbeta\n");
        // Both present but in the wrong order: beta must NOT match before
        // alpha's position.
        let wrong_order = match_expected("noise beta noise alpha end", &expected);
        assert!(wrong_order[0].is_some(), "alpha present");
        assert!(wrong_order[1].is_none(), "beta before alpha must not count");

        let right_order = match_expected("x alpha y beta z", &expected);
        assert!(right_order.iter().all(Option::is_some));
    }

    #[test]
    fn match_expected_matches_substrings_not_full_lines() {
        let expected = parse_expected("execve works!\n");
        let log = "limine: Loading executable `boot():/kernel.elf`...KCLNE\nexecve works!\n";
        assert!(match_expected(log, &expected)[0].is_some());
    }

    #[test]
    fn unordered_lines_tolerate_scheduler_interleaving() {
        let expected = parse_expected("start\n~child: one\n~parent: one\nend of file\n");
        // Same boot, two scheduler outcomes: parent first or child first.
        let child_first = "start\nchild: one\nparent: one\nend of file\n";
        let parent_first = "start\nparent: one\nchild: one\nend of file\n";
        assert!(match_expected(child_first, &expected).iter().all(Option::is_some));
        assert!(match_expected(parent_first, &expected).iter().all(Option::is_some));
        // But an unordered line still must not match BEFORE the cursor.
        let too_early = "child: one\nstart\nparent: one\nend of file\n";
        let m = match_expected(too_early, &expected);
        assert!(m[1].is_none(), "unordered line before its anchor must miss");
    }

    #[test]
    fn strip_serial_noise_removes_csi_and_cr() {
        let raw = b"\x1b[2J\x1b[01;01H\x1b[=3hHello\r\nworld";
        assert_eq!(strip_serial_noise(raw), "Hello\nworld");
    }

    // ── Part A: build-config guard against reverting the miscompile fix ────

    /// Regression guard: the kernel target-feature line must keep SSE
    /// disabled. If someone re-adds `+sse4.2` (or drops the `-sse*` set),
    /// the rustc 1.97.0 aggregate-copy miscompile returns silently — this
    /// test fails loudly in host CI before that can ship. See
    /// docs/miscompile-audit.md.
    #[test]
    fn cargo_config_keeps_sse_disabled() {
        let manifest = env!("CARGO_MANIFEST_DIR"); // .../xtask
        let cfg = Path::new(manifest).join("../.cargo/config.toml");
        let text = std::fs::read_to_string(&cfg)
            .unwrap_or_else(|e| panic!("read {}: {e}", cfg.display()));
        // Isolate the active target-feature flag (ignore comment lines).
        let feature_line = text
            .lines()
            .find(|l| l.contains("target-feature=") && !l.trim_start().starts_with('#'))
            .expect("no active target-feature line in .cargo/config.toml");
        assert!(
            feature_line.contains("-sse2") && feature_line.contains("-sse4.2"),
            "target-feature must disable SSE (found: {feature_line})"
        );
        assert!(
            !feature_line.contains("+sse"),
            "target-feature must NOT re-enable any +sse* level (found: {feature_line})"
        );
    }

    #[test]
    fn cpio_newc_record_has_valid_header_and_padding() {
        let mut out = Vec::new();
        cpio_newc_record(&mut out, b"ab", 0o100644, &[0xDE, 0xAD, 0xBE]);
        // magic
        assert_eq!(&out[0..6], b"070701");
        // mode field (offset 14..22) = 0o100644 = 0x81A4
        assert_eq!(&out[14..22], b"000081A4");
        // filesize field (54..62) = 3
        assert_eq!(&out[54..62], b"00000003");
        // namesize field (94..102) = len("ab")+1 = 3
        assert_eq!(&out[94..102], b"00000003");
        // header(110) + name "ab\0"(3) = 113, padded to 116 → 3 pad bytes
        assert_eq!(&out[110..113], b"ab\0");
        assert_eq!(&out[113..116], &[0, 0, 0]);
        // data (3 bytes) then padded to 4 → 1 pad byte; total 116+3+1 = 120
        assert_eq!(&out[116..119], &[0xDE, 0xAD, 0xBE]);
        assert_eq!(out.len(), 120);
    }

    #[test]
    fn cpio_newc_trailer_record_is_recognizable() {
        let mut out = Vec::new();
        cpio_newc_record(&mut out, b"TRAILER!!!", 0, &[]);
        assert_eq!(&out[0..6], b"070701");
        // filesize 0
        assert_eq!(&out[54..62], b"00000000");
        // name present, NUL-terminated
        assert_eq!(&out[110..121], b"TRAILER!!!\0");
    }
}
