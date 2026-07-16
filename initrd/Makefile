# Build initrd/initrd.cpio containing init2.elf
# Requires: nasm >= 2.14, GNU ld >= 2.35ish, cpio
#
# Note on linker flags: a plain `ld -static -Ttext=0x400000` on recent
# binutils (observed with GNU ld 2.42) emits *three* PT_LOAD segments
# instead of one — including a header-only segment placed *below*
# 0x400000 — because of newer ld's default -z separate-code behavior.
# `-z noseparate-code` collapses .text/.rodata back into a single
# segment, and `-Ttext-segment=0x400000` (rather than `-Ttext=`) pins
# that segment's start exactly at 0x400000 instead of letting ld place
# the ELF header before it. `-s` strips the symbol table the static
# link leaves behind, just to keep the embedded byte array small.

.PHONY: all clean rebuild

NASM    ?= nasm
LD      ?= ld
PYTHON3 ?= python3
LDFLAGS  = -static -Ttext-segment=0x400000 -e _start --build-id=none -z noseparate-code -s

all: initrd.cpio

init2.o: init2.asm
	$(NASM) -f elf64 -o $@ $<

init2.elf: init2.o
	$(LD) $(LDFLAGS) -o $@ $<

initrd.cpio: init2.elf testfile.txt
	printf 'init2.elf\ntestfile.txt\n' | cpio -o --format=newc --quiet > $@
	@echo "initrd.cpio: $$(wc -c < $@) bytes"

# Alternative: regenerate the pre-built bytes via Python (no nasm/ld needed)
rebuild:
	$(PYTHON3) ../scripts/elf2rust.py init2.elf INIT2_ELF
	$(PYTHON3) -c "\
import subprocess, struct, os; \
data = open('init2.elf','rb').read(); \
from subprocess import check_output; \
print(f'init2.elf: {len(data)} bytes')"

clean:
	rm -f init2.o init2.elf initrd.cpio
