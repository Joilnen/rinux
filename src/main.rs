//! Kernel minimo em Rust para PC padrao (BIOS/x86_64).
//!
//! Fluxo:
//!   GRUB (multiboot2) -> stub 32 bits -> modo longo -> kernel_main
//!
//! Modulos principais:
//!   boot      : cabecalho multiboot2, GDT, paginacao e entrada em modo longo
//!   gdt       : GDT com segmentos de usuario e TSS
//!   idt       : IDT, excecoes, PIC e timer (IRQ0)
//!   paging    : alocador de frames, paginas de 4 KiB e demand paging
//!   user      : entrada em ring 3 e demos de protecao usuario/kernel
//!   arch      : portas de I/O, halt e reboot
//!   vga       : driver do console VGA em modo texto (80x25, 0xB8000)
//!   keyboard  : driver do teclado PS/2 (8042), por polling
//!   serial    : COM1, usado para depuracao
//!   shell     : terminal/linha de comando minima
//!   sync      : spinlock usado pelo console

#![no_std]
#![no_main]

use core::panic::PanicInfo;

mod arch;
mod boot;
mod gdt;
mod idt;
mod keyboard;
mod paging;
mod serial;
mod shell;
mod sync;
mod user;
mod vga;

/// Imprime sem alocar memoria.
#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {
        $crate::vga::_print(format_args!($($arg)*))
    };
}

/// Imprime com quebra de linha, sem alocar memoria.
#[macro_export]
macro_rules! println {
    () => {
        $crate::print!("\n")
    };
    ($($arg:tt)*) => {
        $crate::print!("{}\n", format_args!($($arg)*))
    };
}

/// Ponto de entrada em Rust, chamado pelo stub de `boot.rs`.
///
/// `multiboot_magic` deve ser 0x36d76289; `multiboot_info` e o endereco da
/// estrutura de informacoes do multiboot2.
#[unsafe(no_mangle)]
pub extern "C" fn kernel_main(multiboot_magic: u32, multiboot_info: u32) -> ! {
    // 1. Console VGA em modo texto.
    vga::init();

    // 2. Serial (opcional, para depuracao em qemu -serial stdio).
    serial::init();

    // 3. Alocador de frames fisicos, a partir do mapa do Multiboot2.
    paging::init(multiboot_magic, multiboot_info);

    // 4. GDT de 64 bits com segmentos de usuario e TSS.
    gdt::init();

    // 5. Teclado PS/2 (ainda por polling; IRQ1 fica mascarada).
    keyboard::init();

    // 6. IDT, PIC e timer. Depois disso as interrupcoes podem ser ligadas.
    idt::init();
    arch::enable_interrupts();

    println!();
    println!("==============================================================");
    println!("  Rust VGA Kernel - x86_64 / modo longo");
    println!("  Boot: Multiboot2 (BIOS)");
    println!("  Video: VGA texto 80x25 @ 0xB8000");
    println!("  Memoria: paginas de 4 KiB + demand paging");
    println!("  Interrupcoes: IDT + PIC + timer IRQ0 (~100 Hz)");
    println!("==============================================================");

    if multiboot_magic != 0x36d7_6289 {
        println!(
            "AVISO: assinatura multiboot2 invalida: {:#010x}",
            multiboot_magic
        );
    }
    println!(
        "Multiboot2: magic={:#010x} info={:#010x}",
        multiboot_magic, multiboot_info
    );
    println!();
    println!("Digite 'help' para ver os comandos.");
    println!();

    shell::run()
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    crate::println!();
    crate::println!("[PANIC] {}", info);
    crate::arch::halt_loop()
}
