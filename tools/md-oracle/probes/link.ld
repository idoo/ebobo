/* A probe cartridge: vectors at 0, the header at 0x100, code after it. Probes keep their
   variables at absolute work RAM addresses (see probe.inc), so .data and .bss are not placed. */
OUTPUT_ARCH(m68k)
ENTRY(_start)
SECTIONS
{
    . = 0;
    .text : {
        KEEP(*(.vectors))
        KEEP(*(.header))
        *(.text*)
        *(.rodata*)
        . = ALIGN(2);
        _rom_end = .;
    }
    /DISCARD/ : { *(.data*) *(.bss*) *(.comment) *(.note*) }
}
