//! IDT (Interrupt Descriptor Table), excecoes, PIC e timer.
//!
//! A IDT tem 256 portas. As 32 primeiras sao excecoes da CPU; as 16 seguintes
//! sao as IRQs do PIC remapeadas para 0x20..0x2F; a porta 0x80 e uma
//! "syscall" didatica, acessivel em ring 3 (DPL = 3).
//!
//! Todas as entradas usam stubs em assembly, porque a CPU empilha o codigo de
//! erro em apenas algumas excecoes. O stub normaliza a pilha (empurra um erro
//! zero quando nao existe), salva os registradores e chama `interrupt_dispatch`.

use core::arch::{asm, global_asm};
use core::ptr;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::arch::port::outb;
use crate::{arch, paging, user};

const PIC1_COMMAND: u16 = 0x20;
const PIC1_DATA: u16 = 0x21;
const PIC2_COMMAND: u16 = 0xA0;
const PIC2_DATA: u16 = 0xA1;
const PIT_CHANNEL0: u16 = 0x40;
const PIT_COMMAND: u16 = 0x43;

const IDT_ENTRIES: usize = 256;

static TICKS: AtomicU64 = AtomicU64::new(0);

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct IdtEntry {
    offset_low: u16,
    selector: u16,
    ist: u8,
    type_attr: u8,
    offset_mid: u16,
    offset_high: u32,
    reserved: u32,
}

const EMPTY_ENTRY: IdtEntry = IdtEntry {
    offset_low: 0,
    selector: 0,
    ist: 0,
    type_attr: 0,
    offset_mid: 0,
    offset_high: 0,
    reserved: 0,
};

static mut IDT: [IdtEntry; IDT_ENTRIES] = [EMPTY_ENTRY; IDT_ENTRIES];

#[repr(C, packed)]
struct IdtPointer {
    limit: u16,
    base: u64,
}

/// Layout da pilha depois do stub comum.
///
/// Os campos `rsp` e `ss` so existem quando a interrupcao veio de ring 3.
#[repr(C)]
pub struct InterruptFrame {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rbp: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,
    pub vector: u64,
    pub error: u64,
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

global_asm!(
    r#"
    .section .text

    .macro isr_no_err n
    .global isr_stub_\n
isr_stub_\n:
    pushq $0
    pushq $\n
    jmp isr_common
    .endm

    .macro isr_err n
    .global isr_stub_\n
isr_stub_\n:
    pushq $\n
    jmp isr_common
    .endm

    isr_no_err 0
    isr_no_err 1
    isr_no_err 2
    isr_no_err 3
    isr_no_err 4
    isr_no_err 5
    isr_no_err 6
    isr_no_err 7
    isr_err    8
    isr_no_err 9
    isr_err    10
    isr_err    11
    isr_err    12
    isr_err    13
    isr_err    14
    isr_no_err 15
    isr_no_err 16
    isr_err    17
    isr_no_err 18
    isr_no_err 19
    isr_no_err 20
    isr_err    21
    isr_no_err 22
    isr_no_err 23
    isr_no_err 24
    isr_no_err 25
    isr_no_err 26
    isr_no_err 27
    isr_no_err 28
    isr_err    29
    isr_err    30
    isr_no_err 31

    isr_no_err 32
    isr_no_err 33
    isr_no_err 34
    isr_no_err 35
    isr_no_err 36
    isr_no_err 37
    isr_no_err 38
    isr_no_err 39
    isr_no_err 40
    isr_no_err 41
    isr_no_err 42
    isr_no_err 43
    isr_no_err 44
    isr_no_err 45
    isr_no_err 46
    isr_no_err 47

    isr_no_err 128

isr_common:
    pushq %rax
    pushq %rbx
    pushq %rcx
    pushq %rdx
    pushq %rbp
    pushq %rsi
    pushq %rdi
    pushq %r8
    pushq %r9
    pushq %r10
    pushq %r11
    pushq %r12
    pushq %r13
    pushq %r14
    pushq %r15

    movq %rsp, %rdi

    # Preserva SSE (xmm0..xmm15) e MXCSR: o tratador em Rust pode usar
    # instrucoes vetoriais, mas o codigo interrompido pode ter valores vivos.
    subq $272, %rsp
    movups %xmm0, 0(%rsp)
    movups %xmm1, 16(%rsp)
    movups %xmm2, 32(%rsp)
    movups %xmm3, 48(%rsp)
    movups %xmm4, 64(%rsp)
    movups %xmm5, 80(%rsp)
    movups %xmm6, 96(%rsp)
    movups %xmm7, 112(%rsp)
    movups %xmm8, 128(%rsp)
    movups %xmm9, 144(%rsp)
    movups %xmm10, 160(%rsp)
    movups %xmm11, 176(%rsp)
    movups %xmm12, 192(%rsp)
    movups %xmm13, 208(%rsp)
    movups %xmm14, 224(%rsp)
    movups %xmm15, 240(%rsp)
    stmxcsr 256(%rsp)

    movq %rsp, %rbp
    andq $-16, %rsp
    call interrupt_dispatch
    movq %rbp, %rsp

    ldmxcsr 256(%rsp)
    movups 0(%rsp), %xmm0
    movups 16(%rsp), %xmm1
    movups 32(%rsp), %xmm2
    movups 48(%rsp), %xmm3
    movups 64(%rsp), %xmm4
    movups 80(%rsp), %xmm5
    movups 96(%rsp), %xmm6
    movups 112(%rsp), %xmm7
    movups 128(%rsp), %xmm8
    movups 144(%rsp), %xmm9
    movups 160(%rsp), %xmm10
    movups 176(%rsp), %xmm11
    movups 192(%rsp), %xmm12
    movups 208(%rsp), %xmm13
    movups 224(%rsp), %xmm14
    movups 240(%rsp), %xmm15
    addq $272, %rsp

    popq %r15
    popq %r14
    popq %r13
    popq %r12
    popq %r11
    popq %r10
    popq %r9
    popq %r8
    popq %rdi
    popq %rsi
    popq %rbp
    popq %rdx
    popq %rcx
    popq %rbx
    popq %rax

    addq $16, %rsp
    iretq

    .section .rodata
    .align 8
    .global isr_stub_table
isr_stub_table:
    .quad isr_stub_0,  isr_stub_1,  isr_stub_2,  isr_stub_3
    .quad isr_stub_4,  isr_stub_5,  isr_stub_6,  isr_stub_7
    .quad isr_stub_8,  isr_stub_9,  isr_stub_10, isr_stub_11
    .quad isr_stub_12, isr_stub_13, isr_stub_14, isr_stub_15
    .quad isr_stub_16, isr_stub_17, isr_stub_18, isr_stub_19
    .quad isr_stub_20, isr_stub_21, isr_stub_22, isr_stub_23
    .quad isr_stub_24, isr_stub_25, isr_stub_26, isr_stub_27
    .quad isr_stub_28, isr_stub_29, isr_stub_30, isr_stub_31
    .quad isr_stub_32, isr_stub_33, isr_stub_34, isr_stub_35
    .quad isr_stub_36, isr_stub_37, isr_stub_38, isr_stub_39
    .quad isr_stub_40, isr_stub_41, isr_stub_42, isr_stub_43
    .quad isr_stub_44, isr_stub_45, isr_stub_46, isr_stub_47
    .quad isr_stub_128
"#,
    options(att_syntax)
);

extern "C" {
    static isr_stub_table: [u64; 49];
}

/// Instala a IDT, remapeia o PIC e liga o timer (IRQ0, ~100 Hz).
pub fn init() {
    unsafe {
        let idt = ptr::addr_of_mut!(IDT) as *mut IdtEntry;
        let table = ptr::addr_of!(isr_stub_table) as *const u64;

        for vector in 0..48 {
            let handler = ptr::read_unaligned(table.add(vector));
            set_gate(idt, vector, handler, 0, if vector == 8 { 1 } else { 0 });
        }

        // Porta 0x80: DPL = 3, para o usuario poder chamar `int $0x80`.
        let handler = ptr::read_unaligned(table.add(48));
        set_gate(idt, 0x80, handler, 3, 0);

        let pointer = IdtPointer {
            limit: (IDT_ENTRIES * core::mem::size_of::<IdtEntry>() - 1) as u16,
            base: idt as u64,
        };
        asm!("lidt [{}]", in(reg) &pointer, options(nostack, preserves_flags));

        remap_pic();
        pit_init();

        // Apenas IRQ0 (timer). IRQ1 continua mascarada: o teclado usa polling.
        outb(PIC1_DATA, 0xFE);
        outb(PIC2_DATA, 0xFF);
    }
}

unsafe fn set_gate(idt: *mut IdtEntry, vector: usize, handler: u64, dpl: u8, ist: u8) {
    let entry = IdtEntry {
        offset_low: handler as u16,
        selector: 0x08,
        ist: ist & 0x07,
        type_attr: 0x8E | ((dpl & 0x03) << 5),
        offset_mid: (handler >> 16) as u16,
        offset_high: (handler >> 32) as u32,
        reserved: 0,
    };
    ptr::write_unaligned(idt.add(vector), entry);
}

unsafe fn io_wait() {
    outb(0x80, 0);
}

unsafe fn remap_pic() {
    // ICW1: reinicia e indica ICW4.
    outb(PIC1_COMMAND, 0x11);
    io_wait();
    outb(PIC2_COMMAND, 0x11);
    io_wait();

    // ICW2: vetores base (0x20 e 0x28).
    outb(PIC1_DATA, 0x20);
    io_wait();
    outb(PIC2_DATA, 0x28);
    io_wait();

    // ICW3: escravos ligados por IRQ2.
    outb(PIC1_DATA, 0x04);
    io_wait();
    outb(PIC2_DATA, 0x02);
    io_wait();

    // ICW4: modo 8086.
    outb(PIC1_DATA, 0x01);
    io_wait();
    outb(PIC2_DATA, 0x01);
    io_wait();
}

unsafe fn pit_init() {
    let divisor: u16 = 11_931; // 1.193182 MHz / 100 Hz
    outb(PIT_COMMAND, 0x36);
    outb(PIT_CHANNEL0, divisor as u8);
    outb(PIT_CHANNEL0, (divisor >> 8) as u8);
}

unsafe fn eoi(irq: u8) {
    if irq >= 8 {
        outb(PIC2_COMMAND, 0x20);
    }
    outb(PIC1_COMMAND, 0x20);
}

pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

fn exception_name(vector: u64) -> &'static str {
    match vector {
        0 => "divide error",
        1 => "debug",
        2 => "NMI",
        3 => "breakpoint",
        4 => "overflow",
        5 => "bound range",
        6 => "invalid opcode",
        7 => "device not available",
        8 => "double fault",
        10 => "invalid TSS",
        11 => "segment not present",
        12 => "stack fault",
        13 => "general protection",
        14 => "page fault",
        16 => "x87 floating point",
        17 => "alignment check",
        18 => "machine check",
        19 => "SIMD floating point",
        20 => "virtualization",
        21 => "control protection",
        _ => "excecao",
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn interrupt_dispatch(frame: *mut InterruptFrame) {
    let frame = unsafe { &mut *frame };

    match frame.vector {
        14 => page_fault(frame),
        13 => general_protection(frame),
        0x80 => syscall(frame),
        32 => {
            TICKS.fetch_add(1, Ordering::Relaxed);
            unsafe { eoi(0) };
        }
        33..=47 => {
            unsafe { eoi((frame.vector - 32) as u8) };
        }
        0..=31 => {
            if user_fault(frame) {
                return;
            }
            fatal(frame);
        }
        _ => fatal(frame),
    }
}

fn page_fault(frame: &mut InterruptFrame) {
    let address: u64;
    unsafe {
        asm!("mov {}, cr2", out(reg) address, options(nomem, nostack, preserves_flags));
    }

    match paging::handle_page_fault(address, frame.error) {
        Ok(()) => {
            let origin = if frame.cs & 3 == 3 { "usuario" } else { "kernel" };
            crate::println!(
                "[PF] demanda atendida: {:#018x} ({}, erro={:#x})",
                address,
                origin,
                frame.error
            );
        }
        Err(reason) => {
            if frame.cs & 3 == 3 && user::is_active() {
                crate::println!(
                    "[USER] page fault em {:#018x}: {} (erro={:#x}, rip={:#018x})",
                    address,
                    reason,
                    frame.error,
                    frame.rip
                );
                user::abort(1);
            }

            crate::println!();
            crate::println!("[PF] falha fatal em {:#018x}", address);
            crate::println!("     motivo : {}", reason);
            crate::println!(
                "     erro={:#x} rip={:#018x} cs={:#x} rflags={:#x}",
                frame.error,
                frame.rip,
                frame.cs,
                frame.rflags
            );
            crate::println!("     CR2={:#018x}", address);
            arch::halt_loop();
        }
    }
}

fn general_protection(frame: &mut InterruptFrame) {
    if user_fault(frame) {
        return;
    }

    crate::println!();
    crate::println!(
        "[EXC] #GP erro={:#x} rip={:#018x} cs={:#x}",
        frame.error,
        frame.rip,
        frame.cs
    );
    arch::halt_loop();
}

fn user_fault(frame: &mut InterruptFrame) -> bool {
    if frame.cs & 3 == 3 && user::is_active() {
        crate::println!(
            "[USER] excecao {} ({}), rip={:#018x}",
            frame.vector,
            exception_name(frame.vector),
            frame.rip
        );
        user::abort(frame.vector as i32);
    } else {
        false
    }
}

fn syscall(frame: &mut InterruptFrame) {
    crate::syscall::dispatch(frame);
}

fn fatal(frame: &InterruptFrame) -> ! {
    crate::println!();
    crate::println!(
        "[EXC] vetor {} ({}), erro={:#x}",
        frame.vector,
        exception_name(frame.vector),
        frame.error
    );
    crate::println!(
        "      rip={:#018x} cs={:#x} rflags={:#x}",
        frame.rip,
        frame.cs,
        frame.rflags
    );
    if frame.cs & 3 == 3 {
        crate::println!("      rsp={:#018x} ss={:#x}", frame.rsp, frame.ss);
    }
    arch::halt_loop();
}
