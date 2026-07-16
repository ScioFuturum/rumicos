OUTPUT_FORMAT(elf64-x86-64)
ENTRY(_start)

KERNEL_BASE = 0xffffffff80000000;

PHDRS
{
    text PT_LOAD FLAGS(5);
    rodata PT_LOAD FLAGS(4);
    data PT_LOAD FLAGS(6);
}

SECTIONS
{
    . = KERNEL_BASE;

    /* The __*_start symbols live INSIDE their section blocks so they see
       the post-ALIGN(4K) address. Declared outside (as before 2026-07-10)
       they capture the PRE-align address — i.e. the tail of the previous
       section — and paging init then remaps the previous section's last
       page with this section's flags. That exact off-by-alignment marked
       the final .text page (memcpy/memset live there) no-execute on the
       first real boot. */
    .text ALIGN(4K) : {
        __text_start = .;
        *(.text._start)
        *(.text .text.*)
    } :text
    __text_end = .;

    .rodata ALIGN(4K) : {
        __rodata_start = .;
        KEEP(*(.requests_start_marker))
        KEEP(*(.requests))
        KEEP(*(.requests_end_marker))
        *(.rodata .rodata.*)
    } :rodata
    __rodata_end = .;

    .data ALIGN(4K) : {
        __data_start = .;
        *(.data .data.*)
    } :data
    __data_end = .;

    .bss ALIGN(4K) : {
        __bss_start = .;
        *(COMMON)
        *(.bss .bss.*)
    } :data
    __bss_end = .;

    /DISCARD/ : {
        *(.comment)
        *(.eh_frame .eh_frame.*)
        *(.note .note.*)
    }
}
