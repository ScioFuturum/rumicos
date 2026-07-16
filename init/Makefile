# Build init/init.elf and regenerate kernel/src/init.rs from it.
# Requires: nasm >= 2.14, GNU ld >= 2.35ish, python3
#
# See initrd/Makefile for why -Ttext-segment=/-z noseparate-code/-s are
# needed instead of a plain `ld -static -Ttext=0x400000` — recent binutils
# otherwise splits the output into 3 PT_LOAD segments (including a
# header-only one below 0x400000) instead of the single RX segment the
# original (and this) loader expects.

.PHONY: all clean regen-init-rs

NASM    ?= nasm
LD      ?= ld
PYTHON3 ?= python3
LDFLAGS  = -static -Ttext-segment=0x400000 -e _start --build-id=none -z noseparate-code -s

all: init.elf

init.o: init.asm
	$(NASM) -f elf64 -o $@ $<

init.elf: init.o
	$(LD) $(LDFLAGS) -o $@ $<
	@echo "init.elf: $$(wc -c < $@) bytes"

# Regenerate kernel/src/init.rs's embedded INIT_ELF byte array from a
# freshly built init.elf. The descriptive header comment above the array
# is hand-maintained (offsets/sizes change whenever init.asm's data layout
# does) — this only refreshes the generated array body, which you'll need
# to splice back in below that header by hand.
regen-init-rs: init.elf
	$(PYTHON3) ../scripts/elf2rust.py init.elf INIT_ELF

clean:
	rm -f init.o init.elf
