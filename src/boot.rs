//! Entrada de baixo nivel: multiboot2 -> modo protegido 32 bits -> modo longo.
//!
//! O GRUB carrega o ELF, coloca a CPU em modo protegido de 32 bits e salta para
//! `_start` com:
//!   eax = 0x36d76289 (assinatura multiboot2)
//!   ebx = endereco da estrutura multiboot2
//!
//! Layout de memoria:
//!   1 MiB ................ stub de boot (baixo, identidade)
//!   0xFFFFFFFF80000000 ... kernel em Rust (metade alta)
//!
//! O stub:
//!   1. salva a assinatura e o ponteiro do multiboot2;
//!   2. constroi a tabela de paginas;
//!   3. carrega a GDT de 64 bits;
//!   4. ativa PAE, long mode e paginacao;
//!   5. salta para `kernel_main(magic, info)` na metade alta.

use core::arch::global_asm;

/// Cabecalho multiboot2 (precisa ficar nos primeiros 32 KiB do arquivo).
///
/// magic       = 0xE85250D6
/// arquitetura = 0 (i386)
/// tamanho     = 24 bytes (16 do cabecalho + 8 da tag final)
/// checksum    = -(magic + arquitetura + tamanho)
#[repr(align(8))]
struct Aligned<T>(T);

#[used]
#[unsafe(link_section = ".multiboot_header")]
static MULTIBOOT2_HEADER: Aligned<[u32; 6]> = Aligned([
    0xE852_50D6, // magic
    0x0000_0000, // arquitetura i386
    0x0000_0018, // tamanho total do cabecalho
    0x17AD_AF12, // checksum
    0x0000_0000, // tag final: tipo = 0
    0x0000_0008, // tag final: tamanho = 8
]);

global_asm!(
    r#"
    .section .text.boot, "ax"
    .global _start
    .code32
_start:
    cli
    cld
    mov $boot_stack_top, %esp
    xor %ebp, %ebp

    # Guarda os argumentos do multiboot2 (enderecos baixos).
    mov %eax, mb_magic
    mov %ebx, mb_info

    # Zera as tres tabelas de paginas (3 * 4096 bytes).
    mov $pml4, %edi
    xor %eax, %eax
    mov $(3 * 4096 / 4), %ecx
    rep stosl

    # PML4[0] -> PDPT (identidade: 0 .. 1 GiB)
    mov $pdpt, %eax
    or $0x3, %eax
    mov %eax, pml4
    movl $0, pml4 + 4

    # PML4[511] -> PDPT (metade alta: 0xFFFFFFFF80000000 .. 0xFFFFFFFFC0000000)
    mov $pdpt, %eax
    or $0x3, %eax
    mov %eax, pml4 + 511 * 8
    movl $0, pml4 + 511 * 8 + 4

    # PDPT[0] -> PD
    mov $pd, %eax
    or $0x3, %eax
    mov %eax, pdpt
    movl $0, pdpt + 4

    # PDPT[510] -> PD
    mov $pd, %eax
    or $0x3, %eax
    mov %eax, pdpt + 510 * 8
    movl $0, pdpt + 510 * 8 + 4

    # PD: 512 entradas de 2 MiB = 1 GiB, identidade fisica.
    # Cada entrada tem 8 bytes; escrevemos so os 32 bits baixos e deixamos
    # os 32 bits altos zerados (a tabela foi zerada acima).
    mov $pd, %edi
    xor %ecx, %ecx
1:
    mov %ecx, %eax
    shl $21, %eax
    or $0x83, %eax
    mov %eax, (%edi, %ecx, 8)
    inc %ecx
    cmp $512, %ecx
    jne 1b

    # GDT com segmentos de codigo e dados para 64 bits.
    lgdt gdt64_ptr

    # CR0.EM = 0, CR0.MP = 1 (suporte a SSE, caso o compilador use).
    mov %cr0, %eax
    and $0xFFFFFFFB, %eax
    or $0x2, %eax
    mov %eax, %cr0

    # CR4.PAE | CR4.OSFXSR | CR4.OSXMMEXCPT
    mov %cr4, %eax
    or $(0x20 | 0x200 | 0x400), %eax
    mov %eax, %cr4

    # CR3 -> PML4
    mov $pml4, %eax
    mov %eax, %cr3

    # EFER.LME = 1
    mov $0xC0000080, %ecx
    rdmsr
    or $0x100, %eax
    wrmsr

    # CR0.PG | CR0.PE
    mov %cr0, %eax
    or $0x80000001, %eax
    mov %eax, %cr0

    # Long jump para um segmento de codigo com L=1: entra em modo longo.
    ljmp $0x08, $long_mode_entry_low

    .code64
long_mode_entry_low:
    mov $0x10, %ax
    mov %ax, %ds
    mov %ax, %es
    mov %ax, %fs
    mov %ax, %gs
    mov %ax, %ss

    # Pilha definitiva na metade alta.
    movabs $stack_top, %rsp
    xor %rbp, %rbp

    # Primeiro argumento = assinatura multiboot2.
    # Segundo argumento   = ponteiro para a estrutura multiboot2.
    mov mb_magic(%rip), %edi
    mov mb_info(%rip), %esi
    movabs $kernel_main, %rax
    call *%rax

3:
    cli
    hlt
    jmp 3b

    # GDT: descriptor nulo, codigo ring 0 (L=1), dados ring 0.
    .section .boot.rodata, "a"
    .align 8
gdt64:
    .quad 0x0000000000000000
    .quad 0x00AF9A000000FFFF
    .quad 0x00CF92000000FFFF
gdt64_end:

    # Ponteiro de 6 bytes para lgdt em modo de 32 bits (base < 4 GiB).
    .align 2
gdt64_ptr:
    .word gdt64_end - gdt64 - 1
    .long gdt64

    # Tabelas de paginas, pilha do stub e argumentos do multiboot2 (baixo).
    .section .boot.bss, "aw", @nobits
    .align 4096
pml4:
    .skip 4096
    .align 4096
pdpt:
    .skip 4096
    .align 4096
pd:
    .skip 4096
    .align 16
boot_stack_bottom:
    .skip 8192
boot_stack_top:

    .align 4
mb_magic:
    .long 0
mb_info:
    .long 0

    # Pilha do kernel, na metade alta.
    .section .bss
    .align 16
stack_bottom:
    .skip 32768
stack_top:
"#,
    options(att_syntax)
);
