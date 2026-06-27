/* Linker script for PICKLE OS position-independent executables (PIE) */

ENTRY(_start)

SECTIONS
{
    /* No fixed base address - loader will choose */
    /* . = 0; would set to 0, but we omit it to let the linker decide */

    .text : {
        *(.text .text.*)
    }

    .rodata : {
        *(.rodata .rodata.*)
    }

    .data : {
        *(.data .data.*)
    }

    .bss : {
        *(.bss .bss.*)
        *(COMMON)
    }

    .dynamic : {
        *(.dynamic)
    }

    .rela.dyn : {
        *(.rela.*)
    }

    .dynsym : {
        *(.dynsym)
    }

    .dynstr : {
        *(.dynstr)
    }

    /* Discard unnecessary sections */
    /DISCARD/ : {
        *(.comment)
        *(.eh_frame)
        *(.note.*)
    }
}
