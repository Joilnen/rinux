//! Operacoes de baixo nivel da arquitetura x86_64.

use core::arch::asm;

/// Acesso a portas de I/O (equivalente a inb/outb do Linux).
pub mod port {
    use core::arch::asm;

    /// Le um byte de uma porta de I/O.
    ///
    /// # Safety
    /// A porta deve ser valida para leitura no hardware atual.
    #[inline]
    pub unsafe fn inb(port: u16) -> u8 {
        let value: u8;
        asm!(
            "in al, dx",
            in("dx") port,
            out("al") value,
            options(nomem, nostack, preserves_flags)
        );
        value
    }

    /// Escreve um byte em uma porta de I/O.
    ///
    /// # Safety
    /// A porta deve ser valida para escrita no hardware atual.
    #[inline]
    pub unsafe fn outb(port: u16, value: u8) {
        asm!(
            "out dx, al",
            in("dx") port,
            in("al") value,
            options(nomem, nostack, preserves_flags)
        );
    }
}

/// Para a CPU permanentemente (interrupcoes desligadas).
pub fn halt_loop() -> ! {
    loop {
        unsafe {
            asm!("cli; hlt", options(nomem, nostack, preserves_flags));
        }
    }
}

/// Liga interrupcoes mascaraeis (IF = 1).
pub fn enable_interrupts() {
    unsafe {
        asm!("sti", options(nomem, nostack, preserves_flags));
    }
}

/// Desliga interrupcoes e devolve o valor anterior de `RFLAGS`.
///
/// O par `irq_save`/`irq_restore` e o equivalente x86 do `local_irq_save` do
/// Linux: permite aninhar regioes criticas sem ligar interrupcoes cedo demais.
#[inline]
pub fn irq_save() -> u64 {
    let flags: u64;
    unsafe {
        asm!(
            "pushfq",
            "pop {flags}",
            "cli",
            flags = out(reg) flags,
            options(nomem)
        );
    }
    flags
}

/// Restaura `RFLAGS` salvo por [`irq_save`].
#[inline]
pub fn irq_restore(flags: u64) {
    unsafe {
        asm!(
            "push {flags}",
            "popfq",
            flags = in(reg) flags,
            options(nomem)
        );
    }
}

/// Reinicia a maquina pulsando a linha de reset do controlador 8042.
pub fn reboot() -> ! {
    unsafe {
        // Espera o buffer de entrada do 8042 esvaziar.
        for _ in 0..100_000 {
            if port::inb(0x64) & 0x02 == 0 {
                break;
            }
        }
        port::outb(0x64, 0xFE);
    }

    halt_loop()
}
