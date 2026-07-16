#!/usr/bin/env python3
"""
elf2rust.py  —  Convert a raw ELF file into a Rust `pub static` byte array.
Usage:  python3 elf2rust.py <elf_file> <const_name> > output.rs
"""
import sys, struct, textwrap

def main():
    if len(sys.argv) < 3:
        print("usage: elf2rust.py <elf> <CONST_NAME>", file=sys.stderr)
        sys.exit(1)

    path, name = sys.argv[1], sys.argv[2]
    data = open(path, "rb").read()

    # Quick validation
    assert data[:4] == b'\x7fELF', "not an ELF file"
    e_entry    = struct.unpack_from('<Q', data, 24)[0]
    e_phentsize= struct.unpack_from('<H', data, 54)[0]
    e_phnum    = struct.unpack_from('<H', data, 56)[0]
    print(f"// ELF64 static executable: {len(data)} bytes")
    print(f"// e_entry    = {e_entry:#018x}")
    print(f"// e_phentsize= {e_phentsize}  e_phnum= {e_phnum}")
    print(f"pub static {name}: &[u8] = &[")
    for i in range(0, len(data), 16):
        chunk = data[i:i+16]
        row = ", ".join(f"0x{b:02x}" for b in chunk)
        print(f"    {row},")
    print("];")

if __name__ == "__main__":
    main()
