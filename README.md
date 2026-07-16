# Rumicos 🦀

**An experimental x86-64 operating-system kernel written in Rust** (`no_std`, no libc,
booted by [Limine](https://github.com/limine-bootloader/limine) over UEFI).

*Экспериментальное ядро ОС для x86-64 на Rust — `no_std`, без libc, загрузка через Limine по UEFI.*

**273** host tests passing · **19/19** live QEMU boot checks · `GPL-3.0-only`

> 🌐 **Website / Сайт:** https://sciofuturum.github.io/rumicos/
> ⬇️ **Download the ready-to-boot build / Скачать готовую сборку:** see the [website](https://sciofuturum.github.io/rumicos/#download).

---

## English

### What it is
Rumicos is a small, from-scratch Unix-like kernel. Every feature below is verified **live in
QEMU**, not just unit-tested:

- **Processes** — copy-on-write `fork()`, `execve()` loading real ELF binaries, `clone()` (CLONE_VM threads)
- **Virtual memory** — 4-level paging, `mmap`/`munmap`, demand-zero & file-backed pages, copy-on-write faults
- **Signals** — `sigaction`, `kill`, delivery on syscall/interrupt return, a real `sigreturn` trampoline
- **SMP scheduler** — multi-core boot, MLFQ with work-stealing, cross-CPU TLB shootdown
- **Filesystem** — a VFS layer with ramfs, devfs (`/dev/null`, `zero`, `serial`), a page cache, CPIO initrd
- **IPC** — anonymous `pipe()` with blocking readers/writers, `dup()`/`dup2()`, kernel futexes
- **Process lifecycle** — `wait4()`, zombie reaping, exit-status encoding, SIGCHLD

The performance-foundation layers (arch intrinsics, cache-padded sync, zero-copy IPC rings,
CMPXCHG16B tagged stack, per-CPU slab, PCID) are described in
[`docs/architecture-notes.md`](docs/architecture-notes.md); deeper write-ups are in
[`IMPLEMENTATION_DETAILS.md`](IMPLEMENTATION_DETAILS.md) and the rest of `docs/`.

### The compiler-bug hunt 🐛
The standout story of this project: a `rustc 1.97.0` / LLVM 22.1.6 **miscompile** that silently
shredded aggregate struct copies (byte stores at stride-2 offsets — only even bytes written).
Found with a custom gdbstub client + hardware watchpoints, isolated to a target-feature trigger
(`+sse*` combined with the target's default `+soft-float`), and fixed with a one-line build-flag
change. It matches the public P-critical issue [rust-lang/rust#159035](https://github.com/rust-lang/rust/issues/159035).
Full write-up: [`docs/miscompile-audit.md`](docs/miscompile-audit.md).

### Download
Grab the compiled, ready-to-boot build from the **[website](https://sciofuturum.github.io/rumicos/#download)**
(`rumicos-boot.zip` = Limine + kernel + install guide, or the raw `kernel.elf`). Rumicos prints
to the **serial port (COM1)**, not the screen.

```sh
# quickest way to try it — unzip the package, then:
qemu-system-x86_64 -machine q35 -m 512M -serial stdio -display none \
  -drive if=pflash,format=raw,readonly=on,file=OVMF_CODE.fd \
  -drive format=raw,file=fat:rw:.
```

### Build from source
Cross-compiles on Linux, macOS and Windows with a stable Rust toolchain (the kernel links with
`rust-lld`, so no host C toolchain is needed):

```sh
rustup target add x86_64-unknown-none
cargo build -p kernel --target x86_64-unknown-none --release   # release kernel
cargo xtask qemu-test                                          # boot in QEMU, check serial output
cargo test --workspace                                         # 273 host tests
cargo xtask mkinitrd                                           # repack initrd.cpio (no cpio needed)
```

`cargo xtask qemu-test` needs `qemu-system-x86_64` in `PATH` plus UEFI firmware (auto-detected,
or set `RUMICOS_OVMF`). Guest RAM defaults to 2G — set `RUMICOS_QEMU_MEM=512M` on a
memory-constrained host. Repository layout: `kernel/` (entry & boot), `crates/` (arch, memory,
paging, proc, fs, sched, smp, sync, cpu, apic, ipc), `xtask/` (build/test orchestration),
`docs/` (engineering notes), `site/` (this website).

---

## Русский

### Что это
Rumicos — небольшое Unix-подобное ядро, написанное с нуля. Все возможности ниже проверены
**вживую в QEMU**, а не только модульными тестами:

- **Процессы** — copy-on-write `fork()`, `execve()` с загрузкой настоящих ELF, `clone()` (потоки CLONE_VM)
- **Виртуальная память** — 4-уровневая трансляция, `mmap`/`munmap`, ленивые и файловые страницы, copy-on-write
- **Сигналы** — `sigaction`, `kill`, доставка при возврате из syscall/прерываний, трамплин `sigreturn`
- **SMP-планировщик** — загрузка нескольких ядер, MLFQ с work-stealing, TLB-shootdown между ядрами
- **Файловая система** — слой VFS с ramfs, devfs (`/dev/null`, `zero`, `serial`), кэш страниц, initrd (CPIO)
- **IPC** — анонимные `pipe()` с блокировкой, `dup()`/`dup2()`, ядерные futex
- **Жизненный цикл процессов** — `wait4()`, уборка зомби, кодирование статуса выхода, SIGCHLD

### Охота на баг компилятора 🐛
Самая необычная часть проекта: **miscompile** в `rustc 1.97.0` / LLVM 22.1.6, который незаметно
портил агрегатное копирование структур (побайтовые записи со смещением через один — писались
только чётные байты). Найден самодельным gdbstub-клиентом с аппаратными точками останова,
локализован до триггера target-фич (`+sse*` вместе с дефолтным `+soft-float`) и исправлен
однострочной правкой флага сборки. Совпадает с публичной P-critical проблемой
[rust-lang/rust#159035](https://github.com/rust-lang/rust/issues/159035). Полный разбор:
[`docs/miscompile-audit.md`](docs/miscompile-audit.md).

### Скачать и запустить
Готовую к загрузке сборку берите на **[сайте](https://sciofuturum.github.io/rumicos/#download)**
(`rumicos-boot.zip` = Limine + ядро + инструкция, либо просто `kernel.elf`). Rumicos выводит всё
в **последовательный порт (COM1)**, а не на экран. Команды сборки — в разделе English выше.

---

## Support the project / Поддержать 💜
This is a free, open-source hobby kernel. Crypto tips are welcome — see the **Donate** section on
the [website](https://sciofuturum.github.io/rumicos/#donate).
*Это бесплатное открытое ядро. Крипто-донат приветствуется — см. раздел «Донат» на сайте.*

## License / Лицензия

Rumicos is free software, licensed under the **GNU General Public License, version 3 only**
(`GPL-3.0-only`) — see [`LICENSE`](LICENSE) for the full text.

This means you may use, study, share and modify it, but any distributed derivative work must
also be released under the GPLv3 and ship its complete corresponding source. It comes with
**no warranty** — see sections 15 and 16 of the licence.

*Rumicos — свободное программное обеспечение под лицензией **GNU General Public License версии 3**
(`GPL-3.0-only`), полный текст — в файле [`LICENSE`](LICENSE). Это значит, что вы можете
использовать, изучать, распространять и изменять его, но любая распространяемая производная
работа тоже обязана выходить под GPLv3 и сопровождаться полным исходным кодом. Программа
поставляется **без каких-либо гарантий** (см. разделы 15 и 16 лицензии).*

The bundled Limine bootloader binary (`run/esp/EFI/BOOT/BOOTX64.EFI`, and the copy inside
`site/downloads/rumicos-boot.zip`) is third-party software distributed under its own
BSD-2-Clause licence, which is GPL-compatible.
