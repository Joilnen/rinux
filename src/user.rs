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

use crate::paging;

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

    // mov eax, 0 ; int 0x80 ; jmp $
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
