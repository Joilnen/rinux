/*
 * Kernel carregado pelo GRUB (multiboot2).
 *
 * O stub de entrada fica na metade baixa (1 MiB), porque o GRUB salta para
 * ele em modo protegido de 32 bits. O restante do kernel e ligado na metade
 * alta (0xFFFFFFFF80000000), que e o modelo de codigo padrao do alvo
 * x86_64-unknown-none. O stub cria paginas de identidade para o primeiro
 * GiB e tambem mapeia a metade alta, entao salta para kernel_main.
 */
ENTRY(_start)

KERNEL_VMA = 0xFFFFFFFF80000000;

SECTIONS
{
    . = 1M;

    /* Stub de 32/64 bits + cabecalho multiboot2 + tabelas de paginas. */
    .boot : ALIGN(8)
    {
        KEEP(*(.multiboot_header))
        *(.text.boot)
        *(.boot.rodata)
        *(.boot.data)
        *(.boot.bss)
    }

    /* A partir daqui, enderecos virtuais sao da metade alta. */
    . = ALIGN(4096);
    . += KERNEL_VMA;

    .text : AT(ADDR(.text) - KERNEL_VMA)
    {
        *(.text .text.*)
    }

    .rodata : AT(ADDR(.rodata) - KERNEL_VMA)
    {
        *(.rodata .rodata.*)
    }

    .data : AT(ADDR(.data) - KERNEL_VMA)
    {
        *(.data .data.*)
    }

    .bss : AT(ADDR(.bss) - KERNEL_VMA)
    {
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        __bss_end = .;
    }

    /DISCARD/ :
    {
        *(.eh_frame)
        *(.eh_frame_hdr)
        *(.comment)
        *(.note*)
    }
}
