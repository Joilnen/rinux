//! Entrada e saida de ring 3 (didatico).
//!
//! O kernel monta um pequeno programa de maquina, copia para a pagina
//! `paging::USER_CODE`, prepara um frame de `iretq` e salta para CPL 3.
//! A volta acontece por interrupcao:
//!
//!   * `int $0x80` com `rax = 0` encerra o programa normalmente;
//!   * uma excecao vira `user::abort`, que restaura a pilha do kernel salva
//!     por `user_enter` e devolve o controle ao shell.
//!
//! Nada disso depende de heap ou de um processo de verdade.

use core::arch::global_asm;
use core::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use crate::{paging, tty};

static ACTIVE: AtomicBool = AtomicBool::new(false);
static EXIT_STATUS: AtomicI32 = AtomicI32::new(-1);

global_asm!(
    r#"
    .section .text
    .global user_enter
user_enter:
    pushq %rbx
    pushq %rbp
    pushq %r12
    pushq %r13
    pushq %r14
    pushq %r15
    movq %rsp, user_saved_rsp(%rip)

    movw $0x1B, %ax
    movw %ax, %ds
    movw %ax, %es

    pushq $0x1B          # SS do usuario (0x18 | 3)
    pushq %rsi           # RSP do usuario
    pushq $0x202         # RFLAGS: IF = 1
    pushq $0x23          # CS do usuario (0x20 | 3)
    pushq %rdi           # RIP do programa
    iretq

    .global user_resume
user_resume:
    movq user_saved_rsp(%rip), %rsp

    movw $0x10, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %fs
    movw %ax, %gs
    movw %ax, %ss

    popq %r15
    popq %r14
    popq %r13
    popq %r12
    popq %rbp
    popq %rbx
    sti
    ret

    .section .bss
    .align 8
user_saved_rsp:
    .quad 0
"#,
    options(att_syntax)
);

extern "C" {
    fn user_enter(entry: u64, stack_top: u64);
    fn user_resume();
}

// Programa de getty/login em ring 3.
//
// O blob e copiado inteiro (codigo + strings) para `USER_CODE`, portanto as
// referencias RIP-relativas continuam validas no endereco do usuario. O buffer
// de leitura fica na pilha do usuario, que e materializada sob demanda.
//
// Syscalls usadas: 1 = write(buf,len), 2 = read(buf,len,flags), 0 = exit.
global_asm!(
    r#"
    .section .text.getty, "ax"

    .macro GETTY_WRITE start, end
    lea \start(%rip), %rdi
    lea \end(%rip), %rsi
    sub %rdi, %rsi
    mov $1, %rax
    int $0x80
    .endm

    .global getty_start
    .global getty_end

getty_start:
    GETTY_WRITE getty_login_msg, getty_login_end

    # Reserva 128 bytes na pilha do usuario para o buffer.
    subq $128, %rsp

    # read(buffer, 32, echo)
    movq %rsp, %rdi
    movq $32, %rsi
    xorq %rdx, %rdx
    movq $2, %rax
    int $0x80
    movq %rax, %r12

    # Confere "root".
    cmpq $4, %r12
    jb getty_bad
    cmpl $0x746f6f72, (%rsp)
    jne getty_bad

    GETTY_WRITE getty_pass_msg, getty_pass_end

    # read(buffer, 32, sem eco)
    movq %rsp, %rdi
    movq $32, %rsi
    movq $1, %rdx
    movq $2, %rax
    int $0x80
    movq %rax, %r13

    # A senha nao foi ecoada; imprime a quebra de linha.
    GETTY_WRITE getty_newline, getty_newline_end

    # Confere "rinux".
    cmpq $5, %r13
    jb getty_bad
    cmpl $0x756e6972, (%rsp)
    jne getty_bad
    cmpb $0x78, 4(%rsp)
    jne getty_bad

    GETTY_WRITE getty_ok_msg, getty_ok_end
    xorq %rdi, %rdi
    xorq %rax, %rax
    int $0x80
    jmp getty_halt

getty_bad:
    GETTY_WRITE getty_bad_msg, getty_bad_end
    movq $1, %rdi
    xorq %rax, %rax
    int $0x80

getty_halt:
    jmp getty_halt

getty_login_msg:
    .ascii "rinux login: "
getty_login_end:
getty_pass_msg:
    .ascii "Password: "
getty_pass_end:
getty_newline:
    .ascii "\n"
getty_newline_end:
getty_ok_msg:
    .ascii "Bem-vindo ao rinux, root!\n"
getty_ok_end:
getty_bad_msg:
    .ascii "Login incorreto. Use root/rinux.\n"
getty_bad_end:

getty_end:
"#,
    options(att_syntax)
);

extern "C" {
    static getty_start: u8;
    static getty_end: u8;
}

// Programa de demonstracao do VFS em ring 3.
//
// Syscalls novas:
//   4 open(path, len, flags)
//   5 close(fd)
//   6 read(fd, buf, len)
//   7 write(fd, buf, len)
//   9 getpid()
global_asm!(
    r#"
    .section .text.filetest, "ax"

    .global filetest_start
    .global filetest_end

filetest_start:
    # write(1, "filetest ring 3: ", ...)
    mov $7, %rax
    mov $1, %rdi
    lea file_hello(%rip), %rsi
    lea file_hello_end(%rip), %rdx
    sub %rsi, %rdx
    int $0x80

    # Buffer de 256 bytes na pilha do usuario.
    subq $256, %rsp

    # fd = open("/etc/motd", 9, O_RDONLY)
    mov $4, %rax
    lea file_path(%rip), %rdi
    mov $9, %rsi
    xor %rdx, %rdx
    int $0x80
    mov %rax, %r12
    cmpq $-1, %rax
    je file_bad

    # n = read(fd, buffer, 255)
    mov $6, %rax
    mov %r12, %rdi
    mov %rsp, %rsi
    mov $255, %rdx
    int $0x80
    mov %rax, %r13
    cmpq $-1, %rax
    je file_bad

    # write(1, buffer, n)
    mov $7, %rax
    mov $1, %rdi
    mov %rsp, %rsi
    mov %r13, %rdx
    int $0x80

    # close(fd)
    mov $5, %rax
    mov %r12, %rdi
    int $0x80

    # getpid()
    mov $9, %rax
    int $0x80

    # exit(0)
    xor %rdi, %rdi
    xor %rax, %rax
    int $0x80
    jmp file_halt

file_bad:
    mov $7, %rax
    mov $1, %rdi
    lea file_error(%rip), %rsi
    lea file_error_end(%rip), %rdx
    sub %rsi, %rdx
    int $0x80
    mov $1, %rdi
    xor %rax, %rax
    int $0x80

file_halt:
    jmp file_halt

file_hello:
    .ascii "filetest ring 3: "
file_hello_end:
file_path:
    .ascii "/etc/motd"
file_error:
    .ascii "filetest: open/read falhou\n"
file_error_end:

filetest_end:
"#,
    options(att_syntax)
);

extern "C" {
    static filetest_start: u8;
    static filetest_end: u8;
}

pub enum Demo {
    /// Escreve na pagina `USER_DATA`, que ainda nao esta presente.
    Demand,
    /// Tenta ler o endereco do kernel, violando a protecao U/S.
    Protection,
}

struct Program {
    bytes: [u8; 64],
    len: usize,
}

impl Program {
    const fn new() -> Self {
        Self {
            bytes: [0; 64],
            len: 0,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        self.bytes[self.len..self.len + bytes.len()].copy_from_slice(bytes);
        self.len += bytes.len();
    }

    fn movabs_rax(&mut self, value: u64) {
        self.push(&[0x48, 0xB8]);
        self.push(&value.to_le_bytes());
    }
}

fn program(demo: Demo) -> Program {
    let mut code = Program::new();

    match demo {
        Demo::Demand => {
            // push/pop toca a pilha do usuario, que tambem e sob demanda.
            code.push(&[0x50, 0x58]);
            code.movabs_rax(paging::USER_DATA);
            // mov qword ptr [rax], 0x12345678
            code.push(&[0x48, 0xC7, 0x00]);
            code.push(&0x1234_5678u32.to_le_bytes());
            // mov rax, [rax]  (confirma que a pagina e legivel)
            code.push(&[0x48, 0x8B, 0x00]);
        }
        Demo::Protection => {
            // Endereco do kernel: presente, porem com U/S = 0.
            code.movabs_rax(crate::paging::KERNEL_VMA);
            // mov rax, [rax]
            code.push(&[0x48, 0x8B, 0x00]);
        }
    }

    // xor edi, edi ; mov eax, 0 ; int 0x80 ; jmp $
    code.push(&[0x31, 0xFF]);
    code.push(&[0xB8, 0, 0, 0, 0, 0xCD, 0x80, 0xEB, 0xFE]);
    code
}

pub fn is_active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// Executa o demo em ring 3 e devolve o status informado por `int 0x80` ou
/// pela excecao que matou o programa.
pub fn run(demo: Demo) -> Result<i32, &'static str> {
    if ACTIVE.swap(true, Ordering::SeqCst) {
        return Err("ja existe um programa de usuario ativo");
    }

    let code = program(demo);
    if let Err(reason) = paging::install_user_code(&code.bytes[..code.len]) {
        ACTIVE.store(false, Ordering::SeqCst);
        return Err(reason);
    }

    EXIT_STATUS.store(-1, Ordering::SeqCst);
    unsafe {
        user_enter(paging::USER_CODE, paging::USER_STACK_TOP);
    }

    // Esquece pilha/dados do demo para que a proxima execucao volte a passar
    // pelo page fault (o codigo continua residente).
    paging::reset_user_demand();

    ACTIVE.store(false, Ordering::SeqCst);
    Ok(EXIT_STATUS.load(Ordering::SeqCst))
}

/// Executa o getty/login em ring 3 no console `console` (ou no console ativo).
///
/// O programa roda de forma sincrona, como os demais demos: o shell fica
/// bloqueado ate o login terminar. Isso e suficiente para exercitar a camada
/// TTY e as syscalls sem um escalonador de processos.
pub fn run_getty(console: Option<usize>) -> Result<i32, &'static str> {
    if ACTIVE.swap(true, Ordering::SeqCst) {
        return Err("ja existe um programa de usuario ativo");
    }

    if let Some(index) = console {
        if !tty::switch(index) {
            ACTIVE.store(false, Ordering::SeqCst);
            return Err("console virtual invalido");
        }
    }

    let start = core::ptr::addr_of!(getty_start) as *const u8;
    let end = core::ptr::addr_of!(getty_end) as *const u8;
    let len = end as usize - start as usize;
    let code = unsafe { core::slice::from_raw_parts(start, len) };

    if let Err(reason) = paging::install_user_code(code) {
        ACTIVE.store(false, Ordering::SeqCst);
        return Err(reason);
    }

    EXIT_STATUS.store(-1, Ordering::SeqCst);
    unsafe {
        user_enter(paging::USER_CODE, paging::USER_STACK_TOP);
    }

    paging::reset_user_demand();
    ACTIVE.store(false, Ordering::SeqCst);
    Ok(EXIT_STATUS.load(Ordering::SeqCst))
}

/// Executa o demo de arquivos/VFS em ring 3.
pub fn run_file() -> Result<i32, &'static str> {
    if ACTIVE.swap(true, Ordering::SeqCst) {
        return Err("ja existe um programa de usuario ativo");
    }

    let start = core::ptr::addr_of!(filetest_start) as *const u8;
    let end = core::ptr::addr_of!(filetest_end) as *const u8;
    let len = end as usize - start as usize;
    let code = unsafe { core::slice::from_raw_parts(start, len) };

    if let Err(reason) = paging::install_user_code(code) {
        ACTIVE.store(false, Ordering::SeqCst);
        return Err(reason);
    }

    EXIT_STATUS.store(-1, Ordering::SeqCst);
    unsafe {
        user_enter(paging::USER_CODE, paging::USER_STACK_TOP);
    }

    paging::reset_user_demand();
    ACTIVE.store(false, Ordering::SeqCst);
    Ok(EXIT_STATUS.load(Ordering::SeqCst))
}

/// Chamado pelo tratador de excecoes/syscall. Abandona o frame de interrupcao
/// e volta para o ponto em que `user_enter` foi chamado.
pub fn abort(status: i32) -> ! {
    EXIT_STATUS.store(status, Ordering::SeqCst);
    ACTIVE.store(false, Ordering::SeqCst);

    unsafe {
        user_resume();
    }
    unreachable!()
}
