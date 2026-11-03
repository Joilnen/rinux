//! GDT de 64 bits com segmentos de usuario e TSS.
//!
//! O stub de boot carrega uma GDT minima (codigo/dados ring 0). Para entrar em
//! ring 3 e voltar por interrupcoes precisamos de mais tres pecas:
//!
//!   * descritores de codigo e dados com DPL = 3;
//!   * uma TSS, usada pela CPU para achar o `rsp0` ao trocar de ring 3 para
//!     ring 0 (interrupcoes e excecoes);
//!   * uma pilha IST para o tratador de double fault, que nao pode usar a
//!     pilha normal se ela estiver corrompida.

use core::arch::{asm, global_asm};
use core::ptr;

const KERNEL_CODE: u64 = 0x00AF_9A00_0000_FFFF;
const KERNEL_DATA: u64 = 0x00CF_9200_0000_FFFF;
const USER_DATA: u64 = 0x00CF_F200_0000_FFFF;
const USER_CODE: u64 = 0x00AF_FA00_0000_FFFF;

pub const KERNEL_DATA_SELECTOR: u16 = 0x10;
const TSS_SELECTOR: u16 = 0x28;

const USER_STACK_SIZE: usize = 16 * 1024;
const DOUBLE_FAULT_STACK_SIZE: usize = 8 * 1024;

#[repr(C, packed)]
struct TaskStateSegment {
    reserved0: u32,
    rsp0: u64,
    rsp1: u64,
    rsp2: u64,
    reserved1: u64,
    ist: [u64; 7],
    reserved2: u64,
    reserved3: u16,
    iomap_base: u16,
}

const _: () = assert!(core::mem::size_of::<TaskStateSegment>() == 104);

#[repr(C, align(16))]
struct TssStorage(TaskStateSegment);

static mut TSS: TssStorage = TssStorage(TaskStateSegment {
    reserved0: 0,
    rsp0: 0,
    rsp1: 0,
    rsp2: 0,
    reserved1: 0,
    ist: [0; 7],
    reserved2: 0,
    reserved3: 0,
    iomap_base: core::mem::size_of::<TaskStateSegment>() as u16,
});

static mut GDT: [u64; 7] = [0; 7];

#[repr(C, packed)]
struct DescriptorPointer {
    limit: u16,
    base: u64,
}

#[repr(align(16))]
struct Stack<const N: usize>([u8; N]);

static mut USER_KERNEL_STACK: Stack<USER_STACK_SIZE> = Stack([0; USER_STACK_SIZE]);
static mut DOUBLE_FAULT_STACK: Stack<DOUBLE_FAULT_STACK_SIZE> =
    Stack([0; DOUBLE_FAULT_STACK_SIZE]);

global_asm!(
    r#"
    .section .text
    .global gdt_reload_cs
gdt_reload_cs:
    pushq $0x08
    leaq 1f(%rip), %rax
    pushq %rax
    lretq
1:
    ret
"#,
    options(att_syntax)
);

extern "C" {
    fn gdt_reload_cs();
}

fn stack_top<const N: usize>(stack: *mut Stack<N>) -> u64 {
    stack as u64 + N as u64
}

/// Topo da pilha usada pela CPU quando um usuario entra no kernel.
pub fn user_kernel_stack_top() -> u64 {
    stack_top(ptr::addr_of_mut!(USER_KERNEL_STACK))
}

fn double_fault_stack_top() -> u64 {
    stack_top(ptr::addr_of_mut!(DOUBLE_FAULT_STACK))
}

fn descriptor(base: u64, limit: u32, access: u8, flags: u8) -> u64 {
    (limit as u64 & 0xFFFF)
        | ((base & 0x00FF_FFFF) << 16)
        | ((access as u64) << 40)
        | (((limit as u64 >> 16) & 0x0F) << 48)
        | (((flags as u64 >> 4) & 0x0F) << 52)
        | (((base >> 24) & 0xFF) << 56)
}

fn tss_descriptor(base: u64, limit: u32) -> (u64, u64) {
    let low = descriptor(base, limit, 0x89, 0);
    let high = base >> 32;
    (low, high)
}

/// Carrega a GDT definitiva, a TSS e as pilhas de privilegio.
pub fn init() {
    unsafe {
        let tss = ptr::addr_of_mut!(TSS) as *mut TaskStateSegment;
        ptr::addr_of_mut!((*tss).rsp0).write_unaligned(user_kernel_stack_top());
        ptr::addr_of_mut!((*tss).ist[1]).write_unaligned(double_fault_stack_top());
        ptr::addr_of_mut!((*tss).iomap_base)
            .write_unaligned(core::mem::size_of::<TaskStateSegment>() as u16);

        let (tss_low, tss_high) =
            tss_descriptor(tss as u64, core::mem::size_of::<TaskStateSegment>() as u32 - 1);

        let gdt = ptr::addr_of_mut!(GDT) as *mut u64;
        *gdt.add(0) = 0;
        *gdt.add(1) = KERNEL_CODE;
        *gdt.add(2) = KERNEL_DATA;
        *gdt.add(3) = USER_DATA;
        *gdt.add(4) = USER_CODE;
        *gdt.add(5) = tss_low;
        *gdt.add(6) = tss_high;

        let pointer = DescriptorPointer {
            limit: (7 * 8 - 1) as u16,
            base: gdt as u64,
        };

        asm!("lgdt [{}]", in(reg) &pointer, options(nostack, preserves_flags));
        gdt_reload_cs();

        asm!(
            "mov ds, ax",
            "mov es, ax",
            "mov fs, ax",
            "mov gs, ax",
            "mov ss, ax",
            in("ax") KERNEL_DATA_SELECTOR,
            options(nostack, preserves_flags)
        );

        asm!("ltr ax", in("ax") TSS_SELECTOR, options(nostack, preserves_flags));
    }
}
