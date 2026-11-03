//! Terminal minimo: prompt, edicao de linha e alguns comandos.
//!
//! Nao ha heap nem processos: a linha e lida byte a byte do teclado PS/2 ou
//! da serial (COM1) e interpretada diretamente.

use crate::{arch, keyboard, println, serial, vga};

const LINE_MAX: usize = 128;

pub fn run() -> ! {
    loop {
        crate::print!("rinux> ");

        let mut line = [0u8; LINE_MAX];
        let mut len = 0usize;

        loop {
            // Aceita entrada da janela VGA (teclado PS/2) ou do terminal
            // serial (`-serial stdio`).
            let byte = match keyboard::read_char().or_else(serial::read_byte) {
                Some(byte) => byte,
                None => {
                    // Sem interrupcoes, apenas cede a CPU por um instante.
                    core::hint::spin_loop();
                    continue;
                }
            };

            match byte {
                b'\n' | b'\r' => {
                    println!();
                    execute(&line[..len]);
                    break;
                }
                0x08 | 0x7F => {
                    if len > 0 {
                        len -= 1;
                        crate::print!("\x08");
                    }
                }
                b'\t' => {}
                0x20..=0x7E => {
                    if len < LINE_MAX {
                        line[len] = byte;
                        len += 1;
                        crate::print!("{}", byte as char);
                    }
                }
                _ => {}
            }
        }
    }
}

fn trim(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(start, |index| index + 1);
    &bytes[start..end]
}

fn as_text(bytes: &[u8]) -> &str {
    core::str::from_utf8(bytes).unwrap_or("<bytes invalidos>")
}

fn execute(line: &[u8]) {
    let line = trim(line);
    if line.is_empty() {
        return;
    }

    if line == b"help" {
        println!("Comandos disponiveis:");
        println!("  help          mostra esta ajuda");
        println!("  clear         limpa a tela");
        println!("  echo <texto>  imprime o texto");
        println!("  info          informacoes do kernel");
        println!("  reboot        reinicia o PC");
        println!("  halt          para a CPU");
    } else if line == b"clear" {
        vga::clear();
    } else if line == b"info" {
        println!("Kernel: Rust, no_std, x86_64");
        println!("Boot:   Multiboot2 (BIOS) -> modo longo");
        println!("Video:  VGA texto 80x25, buffer 0xB8000");
        println!("Entrada: teclado PS/2 e serial COM1 (polling, sem IRQ)");
        println!("Heap:   nenhum (apenas core, sem alocacao)");
    } else if line == b"reboot" {
        println!("Reiniciando...");
        arch::reboot();
    } else if line == b"halt" {
        println!("CPU parada. Use reset ou feche o QEMU.");
        arch::halt_loop();
    } else if let Some(text) = line.strip_prefix(b"echo ") {
        println!("{}", as_text(text));
    } else {
        println!("comando desconhecido: {}", as_text(line));
        println!("digite 'help' para a lista de comandos");
    }
}
