//! Driver minimo da UART 16550 (COM1), usado para depuracao.
//!
//! No QEMU: `-serial stdio`. Em um PC fisico, COM1 aparece em 0x3F8.

use core::sync::atomic::{AtomicBool, Ordering};

use crate::arch::port::{inb, outb};

const COM1: u16 = 0x3F8;

static PRESENT: AtomicBool = AtomicBool::new(false);

/// Inicializa a COM1 em 38400 8N1, sem interrupcoes.
pub fn init() {
    unsafe {
        outb(COM1 + 1, 0x00); // desabilita interrupcoes
        outb(COM1 + 3, 0x80); // habilita DLAB
        outb(COM1 + 0, 0x03); // divisor = 3 -> 38400 baud
        outb(COM1 + 1, 0x00);
        outb(COM1 + 3, 0x03); // 8 bits, sem paridade, 1 stop
        outb(COM1 + 2, 0xC7); // FIFO de 14 bytes
        outb(COM1 + 4, 0x0B); // DTR/RTS, IRQs desligadas

        // Em uma maquina sem UART, o barramento costuma devolver 0xFF.
        PRESENT.store(inb(COM1 + 5) != 0xFF, Ordering::Relaxed);
    }
}

/// Le um byte recebido pela serial, se houver.
///
/// Isso permite digitar no terminal do QEMU (`-serial stdio`) alem de usar
/// o teclado PS/2 na janela VGA.
pub fn read_byte() -> Option<u8> {
    if !PRESENT.load(Ordering::Relaxed) {
        return None;
    }

    unsafe {
        // LSR bit 0: ha um byte pronto para leitura.
        if inb(COM1 + 5) & 0x01 == 0 {
            return None;
        }
        Some(inb(COM1))
    }
}

fn write_byte(byte: u8) {
    unsafe {
        // Se nao houver UART, inb devolve 0xFF e o bit 0x20 fica setado,
        // portanto nao travamos.
        while inb(COM1 + 5) & 0x20 == 0 {}
        outb(COM1, byte);
    }
}

/// Escreve bytes na serial, convertendo \n em \r\n.
pub fn write_bytes(bytes: &[u8]) {
    for &byte in bytes {
        if byte == b'\n' {
            write_byte(b'\r');
        }
        write_byte(byte);
    }
}

/// Escreve uma string na serial, convertendo \n em \r\n.
#[allow(dead_code)]
pub fn write_str(text: &str) {
    write_bytes(text.as_bytes());
}
