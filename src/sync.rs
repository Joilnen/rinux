//! Spinlock com desabilitacao de interrupcoes locais (estilo `irqsave`).
//!
//! Em uma unica CPU, desligar interrupcoes enquanto o lock esta preso impede
//! que o timer tente preemptar uma tarefa que esta dentro da regiao critica.
//! Sem isso, o tratador do timer poderia tentar adquirir a mesma trava e
//! travar para sempre, pois a tarefa interrompida nao consegue libera-la.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

use crate::arch;

pub struct SpinLock<T> {
    locked: AtomicBool,
    data: UnsafeCell<T>,
}

// O acesso ao conteudo e protegido pelo proprio lock.
unsafe impl<T: Send> Sync for SpinLock<T> {}
unsafe impl<T: Send> Send for SpinLock<T> {}

impl<T> SpinLock<T> {
    pub const fn new(data: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            data: UnsafeCell::new(data),
        }
    }

    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        // `irq_save` tambem e correto quando o lock e adquirido dentro de uma
        // interrupcao: o valor salvo ja tem IF = 0 e sera restaurado assim.
        let flags = arch::irq_save();
        while self.locked.swap(true, Ordering::Acquire) {
            core::hint::spin_loop();
        }
        SpinLockGuard { lock: self, flags }
    }
}

pub struct SpinLockGuard<'a, T> {
    lock: &'a SpinLock<T>,
    flags: u64,
}

impl<T> Deref for SpinLockGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
        arch::irq_restore(self.flags);
    }
}
