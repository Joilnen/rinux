//! Terminal minimo: prompt, edicao de linha e alguns comandos.
//!
//! Nao ha heap nem escalonador: a linha e lida byte a byte do teclado PS/2 ou
//! da serial (COM1) e interpretada diretamente. Os comandos `demand`,
//! `usertest` e `userprot` exercitam a paginacao e o modo usuario.

use crate::{arch, idt, keyboard, paging, println, serial, user, vga};

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
        println!("  demand        testa demand paging no kernel");
        println!("  usertest      roda um programa em ring 3 com page fault");
        println!("  userprot      testa a protecao usuario/kernel");
        println!("  mem           estatisticas de frames e paginas");
        println!("  reboot        reinicia o PC");
        println!("  halt          para a CPU");
    } else if line == b"clear" {
        vga::clear();
    } else if line == b"info" {
        println!("Kernel: Rust, no_std, x86_64");
        println!("Boot:   Multiboot2 (BIOS) -> modo longo");
        println!("Video:  VGA texto 80x25, buffer 0xB8000");
        println!("Entrada: teclado PS/2 e serial COM1 (polling)");
        println!("IDT:    256 portas; excecoes + IRQ0 (timer ~100 Hz)");
        println!("Memoria: paginas de 4 KiB, demand paging e frames fisicos");
        println!("Usuario: ring 3 com GDT/TSS e protecao U/S");
        println!("Ticks do timer: {}", idt::ticks());
    } else if line == b"demand" {
        demand_demo();
    } else if line == b"usertest" {
        user_demo(false);
    } else if line == b"userprot" {
        user_demo(true);
    } else if line == b"mem" {
        memory_info();
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

fn demand_demo() {
    let address = paging::kernel_demand_addr(0);
    let before = paging::demand_faults();

    // A pagina ainda nao esta presente: a escrita gera #PF. O tratador aloca
    // um frame, mapeia com U/S = 0 e a CPU repete a instrucao.
    unsafe {
        core::ptr::write_volatile(address as *mut u64, 0xDEAD_BEEF_CAFE_1234);
        let value = core::ptr::read_volatile(address as *const u64);
        println!(
            "[DEMAND] {:#018x} = {:#018x} (page faults: {})",
            address,
            value,
            paging::demand_faults() - before
        );
    }
}

fn user_demo(protection: bool) {
    let before = paging::demand_faults();
    let demo = if protection {
        user::Demo::Protection
    } else {
        user::Demo::Demand
    };

    println!(
        "[USER] entrando em ring 3 ({})...",
        if protection {
            "teste de protecao"
        } else {
            "demand paging"
        }
    );

    match user::run(demo) {
        Ok(0) => println!(
            "[USER] voltou ao kernel; page faults nesta execucao: {}",
            paging::demand_faults() - before
        ),
        Ok(status) => println!("[USER] programa encerrado com status {}", status),
        Err(reason) => println!("[USER] nao foi possivel executar: {}", reason),
    }
}

fn memory_info() {
    let stats = paging::stats();
    println!("Frames fisicos (4 KiB):");
    println!("  alocados : {}", stats.allocated);
    println!("  livres   : {}", stats.free);
    println!("  page faults atendidos: {}", stats.faults);
    println!("  ticks do timer       : {}", idt::ticks());

    let user = paging::page_flags(paging::USER_CODE);
    let kernel = paging::page_flags(paging::KERNEL_VMA);
    match user {
        Some(flags) => println!("  USER_CODE  flags={:#x} (U/S={})", flags, (flags >> 2) & 1),
        None => println!("  USER_CODE  ainda nao mapeada (rode 'usertest')"),
    }
    match kernel {
        Some(flags) => println!("  KERNEL_VMA flags={:#x} (U/S={})", flags, (flags >> 2) & 1),
        None => println!("  KERNEL_VMA sem mapeamento?"),
    }
}

