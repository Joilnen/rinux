//! Syscalls didaticas via `int $0x80`.
//!
//! O usuario coloca o numero da chamada em `rax` e os argumentos em `rdi`,
//! `rsi`, `rdx`. O resultado volta em `rax`. As chamadas disponiveis sao:
//!
//!   0  exit(status)                 encerra o programa de usuario
//!   1  write(buf, len)              escreve no console ativo
//!   2  read(buf, len, flags)        le uma linha (flags bit 0 = sem eco)
//!   3  get_tty()                    devolve o indice do console ativo
//!
//! Toda syscall valida o intervalo do usuario antes de tocar no buffer. As
//! paginas sob demanda do usuario sao materializadas por `ensure_user_range`,
//! porque o kernel nao pode simplesmente provocar um `#PF` de usuario.

use crate::idt::InterruptFrame;
use crate::{paging, tty, user};

pub const SYS_EXIT: u64 = 0;
pub const SYS_WRITE: u64 = 1;
pub const SYS_READ: u64 = 2;
pub const SYS_GET_TTY: u64 = 3;

const READ_NO_ECHO: u64 = 1;
const MAX_READ: u64 = 256;

const ERR_POINTER: u64 = u64::MAX;

/// Despacha uma syscall. O frame pertence ao tratador de `int 0x80`; alterar
/// `frame.rax` muda o valor que o programa de usuario vera ao retornar.
pub fn dispatch(frame: &mut InterruptFrame) {
    match frame.rax {
        SYS_EXIT => user::abort(frame.rdi as i32),
        SYS_WRITE => sys_write(frame),
        SYS_READ => sys_read(frame),
        SYS_GET_TTY => frame.rax = tty::active() as u64,
        _ => {
            crate::println!(
                "[USER] syscall desconhecida: {} (rip={:#018x})",
                frame.rax,
                frame.rip
            );
            user::abort(-1);
        }
    }
}

fn sys_write(frame: &mut InterruptFrame) {
    let ptr = frame.rdi;
    let len = frame.rsi;

    if let Err(reason) = paging::ensure_user_range(ptr, len) {
        crate::println!("[USER] write invalido: {}", reason);
        frame.rax = ERR_POINTER;
        return;
    }

    let bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, len as usize) };
    tty::write_bytes(bytes);
    frame.rax = len;
}

fn sys_read(frame: &mut InterruptFrame) {
    let ptr = frame.rdi;
    let len = frame.rsi.min(MAX_READ);
    let flags = frame.rdx;
    let echo = flags & READ_NO_ECHO == 0;

    if len == 0 {
        frame.rax = 0;
        return;
    }

    if let Err(reason) = paging::ensure_user_range(ptr, len) {
        crate::println!("[USER] read invalido: {}", reason);
        frame.rax = ERR_POINTER;
        return;
    }

    // O getty pode estar rodando em um console diferente do ativo se o usuario
    // pressionar Alt+F1..F8. A leitura fica presa ao console de origem; o
    // echo tambem, para o prompt nao "vazar" para outro terminal.
    let console = tty::active();
    let mut count = 0u64;

    while count < len {
        tty::pump_input();

        let byte = match tty::read_byte_from(console) {
            Some(byte) => byte,
            None => {
                core::hint::spin_loop();
                continue;
            }
        };

        match byte {
            b'\n' | b'\r' => {
                if echo {
                    tty::write_to(console, b"\n");
                }
                unsafe {
                    *((ptr as *mut u8).add(count as usize)) = b'\n';
                }
                count += 1;
                break;
            }
            0x08 | 0x7F => {
                if count > 0 {
                    count -= 1;
                    if echo {
                        tty::write_to(console, &[0x08]);
                    }
                }
            }
            0x20..=0x7E => {
                unsafe {
                    *((ptr as *mut u8).add(count as usize)) = byte;
                }
                count += 1;
                if echo {
                    tty::write_to(console, &[byte]);
                }
            }
            _ => {}
        }
    }

    frame.rax = count;
}
