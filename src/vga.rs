//! Driver de baixo nivel do console VGA em modo texto.
//!
//! O modo texto VGA usa uma matriz de 80x25 celulas de 16 bits em 0xB8000:
//!   bits 0..7  = caractere ASCII
//!   bits 8..15 = atributo (cor de frente/fundo)
//!
//! Este modulo apenas desenha uma matriz de celulas na tela e posiciona o
//! cursor de hardware. A nocao de varios consoles virtuais (tty0..tty7) fica
//! em `tty.rs`, que mantem um buffer por console e chama `render` ao trocar.

use crate::arch::port::outb;

pub const BUFFER_WIDTH: usize = 80;
pub const BUFFER_HEIGHT: usize = 25;
pub const CELLS: usize = BUFFER_WIDTH * BUFFER_HEIGHT;

/// Endereco fisico do framebuffer de texto VGA.
const VGA_BUFFER: *mut u16 = 0xB8000 as *mut u16;

/// Cinza claro sobre preto (atributo VGA padrao).
pub const DEFAULT_COLOR: u8 = 0x07;

const CRTC_INDEX: u16 = 0x3D4;
const CRTC_DATA: u16 = 0x3D5;

/// Celula vazia com a cor padrao.
pub const fn blank_cell() -> u16 {
    (b' ' as u16) | ((DEFAULT_COLOR as u16) << 8)
}

/// Inicializa o modo texto: limpa a tela e configura o formato do cursor.
pub fn init() {
    clear_hardware();

    unsafe {
        // Forma do cursor: linha de baixo (start=14, end=15).
        outb(CRTC_INDEX, 0x0A);
        outb(CRTC_DATA, 0x0E);
        outb(CRTC_INDEX, 0x0B);
        outb(CRTC_DATA, 0x0F);
    }
    set_cursor(0, 0);
}

/// Limpa o framebuffer de hardware.
pub fn clear_hardware() {
    let blank = blank_cell();
    for index in 0..CELLS {
        unsafe {
            core::ptr::write_volatile(VGA_BUFFER.add(index), blank);
        }
    }
}

/// Desenha um buffer completo de console e posiciona o cursor.
pub fn render(cells: &[u16; CELLS], row: usize, column: usize) {
    for index in 0..CELLS {
        unsafe {
            core::ptr::write_volatile(VGA_BUFFER.add(index), cells[index]);
        }
    }
    set_cursor(row, column);
}

/// Posiciona o cursor de hardware na celula (row, column).
pub fn set_cursor(row: usize, column: usize) {
    let position = (row * BUFFER_WIDTH + column) as u16;
    unsafe {
        outb(CRTC_INDEX, 0x0F);
        outb(CRTC_DATA, (position & 0xFF) as u8);
        outb(CRTC_INDEX, 0x0E);
        outb(CRTC_DATA, (position >> 8) as u8);
    }
}
