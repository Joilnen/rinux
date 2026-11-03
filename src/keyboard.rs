//! Driver minimo do teclado PS/2 (equivalente simplificado ao atkbd + i8042).
//!
//! O controlador 8042 expoe:
//!   0x64 - status/comando
//!   0x60 - dados (scancode)
//!
//! Este driver usa polling (sem interrupcoes), o que e suficiente para um
//! terminal simples. Os scancodes sao do conjunto 1 (XT/AT).

use core::sync::atomic::{AtomicBool, Ordering};

use crate::arch::port::{inb, outb};

const KBD_DATA: u16 = 0x60;
const KBD_STATUS: u16 = 0x64;

const STATUS_OUTPUT_FULL: u8 = 0x01;
const STATUS_INPUT_FULL: u8 = 0x02;
const STATUS_MOUSE_DATA: u8 = 0x20;

static SHIFT: AtomicBool = AtomicBool::new(false);
static ALT: AtomicBool = AtomicBool::new(false);
static EXTENDED: AtomicBool = AtomicBool::new(false);

/// Resultado de uma tecla.
#[derive(Clone, Copy)]
pub enum Key {
    /// Caractere ASCII (Enter vira `\n`, Backspace vira 0x08, Tab vira `\t`).
    Char(u8),
    /// Alt+F1..Alt+F8: pede a troca para o console virtual 0..7.
    Console(usize),
}

/// Tabela de scancodes (conjunto 1) para ASCII sem Shift.
const fn build_keymap() -> [u8; 128] {
    let mut map = [0u8; 128];

    map[0x02] = b'1';
    map[0x03] = b'2';
    map[0x04] = b'3';
    map[0x05] = b'4';
    map[0x06] = b'5';
    map[0x07] = b'6';
    map[0x08] = b'7';
    map[0x09] = b'8';
    map[0x0A] = b'9';
    map[0x0B] = b'0';
    map[0x0C] = b'-';
    map[0x0D] = b'=';

    map[0x10] = b'q';
    map[0x11] = b'w';
    map[0x12] = b'e';
    map[0x13] = b'r';
    map[0x14] = b't';
    map[0x15] = b'y';
    map[0x16] = b'u';
    map[0x17] = b'i';
    map[0x18] = b'o';
    map[0x19] = b'p';
    map[0x1A] = b'[';
    map[0x1B] = b']';

    map[0x1E] = b'a';
    map[0x1F] = b's';
    map[0x20] = b'd';
    map[0x21] = b'f';
    map[0x22] = b'g';
    map[0x23] = b'h';
    map[0x24] = b'j';
    map[0x25] = b'k';
    map[0x26] = b'l';
    map[0x27] = b';';
    map[0x28] = b'\'';
    map[0x29] = b'`';

    map[0x2B] = b'\\';

    map[0x2C] = b'z';
    map[0x2D] = b'x';
    map[0x2E] = b'c';
    map[0x2F] = b'v';
    map[0x30] = b'b';
    map[0x31] = b'n';
    map[0x32] = b'm';
    map[0x33] = b',';
    map[0x34] = b'.';
    map[0x35] = b'/';

    map[0x39] = b' ';
    map
}

const KEYMAP: [u8; 128] = build_keymap();

fn apply_shift(byte: u8) -> u8 {
    match byte {
        b'a'..=b'z' => byte - 32,
        b'1'..=b'9' => b"!@#$%^&*("[(byte - b'1') as usize],
        b'0' => b')',
        b'-' => b'_',
        b'=' => b'+',
        b'[' => b'{',
        b']' => b'}',
        b';' => b':',
        b'\'' => b'"',
        b'`' => b'~',
        b'\\' => b'|',
        b',' => b'<',
        b'.' => b'>',
        b'/' => b'?',
        other => other,
    }
}

unsafe fn wait_input_clear() -> bool {
    for _ in 0..100_000 {
        if inb(KBD_STATUS) & STATUS_INPUT_FULL == 0 {
            return true;
        }
    }
    false
}

unsafe fn wait_output_full() -> bool {
    for _ in 0..100_000 {
        if inb(KBD_STATUS) & STATUS_OUTPUT_FULL != 0 {
            return true;
        }
    }
    false
}

unsafe fn flush_output() {
    for _ in 0..64 {
        if inb(KBD_STATUS) & STATUS_OUTPUT_FULL == 0 {
            break;
        }
        let _ = inb(KBD_DATA);
    }
}

unsafe fn send_command(command: u8) {
    if wait_input_clear() {
        outb(KBD_STATUS, command);
    }
}

unsafe fn send_data(data: u8) {
    if wait_input_clear() {
        outb(KBD_DATA, data);
    }
}

/// Inicializa o 8042 e o teclado.
///
/// A BIOS/GRUB pode deixar o controlador com a traducao de scancodes
/// desligada, ou o teclado com a varredura desabilitada. Sem isso, o teclado
/// fisico nao entrega os codigos que este driver espera (conjunto 1).
pub fn init() {
    unsafe {
        flush_output();

        // Desabilita o teclado durante a configuracao.
        send_command(0xAD);
        flush_output();

        // Le o byte de configuracao do 8042.
        send_command(0x20);
        let mut config = if wait_output_full() { inb(KBD_DATA) } else { 0x45 };

        // Bit 6: traduz o conjunto 2 (padrao do teclado) para o conjunto 1.
        // Bit 0: IRQ1 desligada, pois usamos polling.
        // Bit 4: clock do teclado habilitado (0 = habilitado).
        config |= 0x40;
        config &= !0x01;
        config &= !0x10;

        send_command(0x60);
        send_data(config);

        // Reabilita o teclado e a varredura.
        send_command(0xAE);
        flush_output();

        send_data(0xF4); // 0xF4 = enable scanning
        if wait_output_full() {
            let _ = inb(KBD_DATA); // ACK (0xFA)
        }
        flush_output();
    }
}

/// Le uma tecla, se disponivel.
///
/// O driver continua por polling. A novidade em relacao ao terminal original e
/// o estado das teclas modificadoras: Shift muda o caractere e Alt+F1..Alt+F8
/// devolve `Key::Console`, usado pela camada TTY para trocar de console.
pub fn read_key() -> Option<Key> {
    unsafe {
        let status = inb(KBD_STATUS);

        // Status bit 0: ha um byte no buffer de saida.
        if status & STATUS_OUTPUT_FULL == 0 {
            return None;
        }
        let scancode = inb(KBD_DATA);

        // Dados do mouse compartilham o 8042; descarta-os.
        if status & STATUS_MOUSE_DATA != 0 {
            return None;
        }

        // Prefixo de teclas estendidas (setas, etc.): ignora o proximo codigo.
        if EXTENDED.swap(false, Ordering::Relaxed) {
            return None;
        }
        if scancode == 0xE0 {
            EXTENDED.store(true, Ordering::Relaxed);
            return None;
        }

        // Bit 7 setado = tecla solta (break code).
        if scancode & 0x80 != 0 {
            match scancode {
                0xAA | 0xB6 => SHIFT.store(false, Ordering::Relaxed),
                0xB8 => ALT.store(false, Ordering::Relaxed),
                _ => {}
            }
            return None;
        }

        match scancode {
            0x2A | 0x36 => {
                SHIFT.store(true, Ordering::Relaxed);
                None
            }
            0x38 => {
                ALT.store(true, Ordering::Relaxed);
                None
            }
            // Alt+F1..Alt+F8 -> console virtual 0..7.
            0x3B..=0x42 if ALT.load(Ordering::Relaxed) => {
                Some(Key::Console((scancode - 0x3B) as usize))
            }
            0x1C => Some(Key::Char(b'\n')), // Enter
            0x0E => Some(Key::Char(0x08)),  // Backspace
            0x0F => Some(Key::Char(b'\t')), // Tab
            _ => {
                let key = KEYMAP[scancode as usize];
                if key == 0 {
                    None
                } else if SHIFT.load(Ordering::Relaxed) {
                    Some(Key::Char(apply_shift(key)))
                } else {
                    Some(Key::Char(key))
                }
            }
        }
    }
}

/// Compatibilidade: le apenas caracteres, descartando pedidos de troca de
/// console. O shell usa `read_key` diretamente.
#[allow(dead_code)]
pub fn read_char() -> Option<u8> {
    match read_key() {
        Some(Key::Char(byte)) => Some(byte),
        _ => None,
    }
}
