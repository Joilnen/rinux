//! Syscalls didaticas via `int $0x80`.
//!
//! O usuario coloca o numero da chamada em `rax` e os argumentos em `rdi`,
//! `rsi`, `rdx`. O resultado volta em `rax`. As chamadas disponiveis sao:
//!
//!   0  exit(status)                 encerra o programa de usuario
//!   1  write(buf, len)              escreve no console ativo
//!   2  read(buf, len, flags)        le uma linha (flags bit 0 = sem eco)
//!   3  get_tty()                    devolve o indice do console ativo
//!   4  open(path, len, flags)       abre um arquivo do VFS
//!   5  close(fd)                    fecha um descritor
//!   6  read(fd, buf, len)           le bytes de um descritor
//!   7  write(fd, buf, len)          escreve bytes em um descritor
//!   8  lseek(fd, offset, whence)    reposiciona o descritor
//!   9  getpid()                     PID da tarefa corrente
//!  10  getppid()                    PID do pai
//!  11  kill(pid, sig)               envia um sinal (0 = so verifica)
//!
//! Toda syscall valida o intervalo do usuario antes de tocar no buffer. As
//! paginas sob demanda do usuario sao materializadas por `ensure_user_range`,
//! porque o kernel nao pode simplesmente provocar um `#PF` de usuario.

use crate::idt::InterruptFrame;
use crate::{paging, task, tty, user, vfs};

pub const SYS_EXIT: u64 = 0;
pub const SYS_WRITE: u64 = 1;
pub const SYS_READ: u64 = 2;
pub const SYS_GET_TTY: u64 = 3;
pub const SYS_OPEN: u64 = 4;
pub const SYS_CLOSE: u64 = 5;
pub const SYS_READ_FD: u64 = 6;
pub const SYS_WRITE_FD: u64 = 7;
pub const SYS_LSEEK: u64 = 8;
pub const SYS_GETPID: u64 = 9;
pub const SYS_GETPPID: u64 = 10;
pub const SYS_KILL: u64 = 11;

const READ_NO_ECHO: u64 = 1;
const MAX_READ: u64 = 256;
const MAX_PATH: u64 = 128;

const ERR_POINTER: u64 = u64::MAX;
const ERR_SYSCALL: u64 = u64::MAX;

/// Despacha uma syscall. O frame pertence ao tratador de `int 0x80`; alterar
/// `frame.rax` muda o valor que o programa de usuario vera ao retornar.
pub fn dispatch(frame: &mut InterruptFrame) {
    match frame.rax {
        SYS_EXIT => user::abort(frame.rdi as i32),
        SYS_WRITE => sys_write(frame),
        SYS_READ => sys_read(frame),
        SYS_GET_TTY => frame.rax = tty::active() as u64,
        SYS_OPEN => sys_open(frame),
        SYS_CLOSE => sys_close(frame),
        SYS_READ_FD => sys_read_fd(frame),
        SYS_WRITE_FD => sys_write_fd(frame),
        SYS_LSEEK => sys_lseek(frame),
        SYS_GETPID => frame.rax = task::current_pid(),
        SYS_GETPPID => frame.rax = task::current_ppid(),
        SYS_KILL => sys_kill(frame),
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
    frame.rax = read_line(ptr, len, flags & READ_NO_ECHO == 0);
}

/// Le uma linha do console para o buffer do usuario. Usada tanto pela syscall
/// antiga `read` quanto por `read(0, ...)`.
fn read_line(ptr: u64, len: u64, echo: bool) -> u64 {
    if len == 0 {
        return 0;
    }

    if let Err(reason) = paging::ensure_user_range(ptr, len) {
        crate::println!("[USER] read invalido: {}", reason);
        return ERR_POINTER;
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

    count
}

fn sys_open(frame: &mut InterruptFrame) {
    let ptr = frame.rdi;
    let len = frame.rsi.min(MAX_PATH);
    let flags = frame.rdx;

    if len == 0 {
        frame.rax = ERR_SYSCALL;
        return;
    }
    if let Err(reason) = paging::ensure_user_range(ptr, len) {
        crate::println!("[USER] open invalido: {}", reason);
        frame.rax = ERR_POINTER;
        return;
    }

    let path = unsafe { core::slice::from_raw_parts(ptr as *const u8, len as usize) };
    match vfs::open(path, flags) {
        Ok(fd) => frame.rax = fd as u64,
        Err(reason) => {
            crate::println!("[USER] open: {}", reason);
            frame.rax = ERR_SYSCALL;
        }
    }
}

fn sys_close(frame: &mut InterruptFrame) {
    match vfs::close(frame.rdi as usize) {
        Ok(()) => frame.rax = 0,
        Err(reason) => {
            crate::println!("[USER] close: {}", reason);
            frame.rax = ERR_SYSCALL;
        }
    }
}

fn sys_read_fd(frame: &mut InterruptFrame) {
    let fd = frame.rdi as usize;
    let ptr = frame.rsi;
    let len = frame.rdx.min(MAX_READ);

    if fd <= 2 {
        frame.rax = read_line(ptr, len, true);
        return;
    }

    if len == 0 {
        frame.rax = 0;
        return;
    }
    if let Err(reason) = paging::ensure_user_range(ptr, len) {
        crate::println!("[USER] read invalido: {}", reason);
        frame.rax = ERR_POINTER;
        return;
    }

    let buffer = unsafe { core::slice::from_raw_parts_mut(ptr as *mut u8, len as usize) };
    match vfs::read(fd, buffer) {
        Ok(count) => frame.rax = count as u64,
        Err(reason) => {
            crate::println!("[USER] read: {}", reason);
            frame.rax = ERR_SYSCALL;
        }
    }
}

fn sys_write_fd(frame: &mut InterruptFrame) {
    let fd = frame.rdi as usize;
    let ptr = frame.rsi;
    let len = frame.rdx;

    if let Err(reason) = paging::ensure_user_range(ptr, len) {
        crate::println!("[USER] write invalido: {}", reason);
        frame.rax = ERR_POINTER;
        return;
    }

    let buffer = unsafe { core::slice::from_raw_parts(ptr as *const u8, len as usize) };
    match vfs::write(fd, buffer) {
        Ok(count) => frame.rax = count as u64,
        Err(reason) => {
            crate::println!("[USER] write: {}", reason);
            frame.rax = ERR_SYSCALL;
        }
    }
}

fn sys_lseek(frame: &mut InterruptFrame) {
    match vfs::lseek(frame.rdi as usize, frame.rsi, frame.rdx) {
        Ok(offset) => frame.rax = offset,
        Err(reason) => {
            crate::println!("[USER] lseek: {}", reason);
            frame.rax = ERR_SYSCALL;
        }
    }
}

/// `kill(pid, sig)`: envia um sinal ou apenas verifica a existencia da tarefa
/// (`sig = 0`). O sinal e entregue no proximo ponto seguro da tarefa alvo.
fn sys_kill(frame: &mut InterruptFrame) {
    let pid = frame.rdi;
    let sig = frame.rsi as u8;

    if frame.rsi > 31 {
        frame.rax = ERR_SYSCALL;
        return;
    }
    match task::kill(pid, sig) {
        Ok(()) => frame.rax = 0,
        Err(reason) => {
            crate::println!("[USER] kill: {}", reason);
            frame.rax = ERR_SYSCALL;
        }
    }
}

