//! Driver de console VGA em modo texto (equivalente simplificado ao vgacon).
//!
//! O modo texto VGA usa uma matriz de 80x25 celulas de 16 bits em 0xB8000:
//!   bits 0..7  = caractere ASCII
//!   bits 8..15 = atributo (cor de frente/fundo)
//!
//! O cursor de hardware e controlado pelos registradores da CRTC (portas
//! 0x3D4/0x3D5).

use core::fmt::{self, Write};

use crate::arch::port::outb;
use crate::serial;
use crate::sync::SpinLock;

pub const BUFFER_WIDTH: usize = 80;
pub const BUFFER_HEIGHT: usize = 25;

/// Endereco fisico do framebuffer de texto VGA.
const VGA_BUFFER: *mut u16 = 0xB8000 as *mut u16;

/// Cinza claro sobre preto (atributo VGA padrao). Verde claro: 0x0A.
const DEFAULT_COLOR: u8 = 0x07;

const CRTC_INDEX: u16 = 0x3D4;
const CRTC_DATA: u16 = 0x3D5;

const fn blank_cell() -> u16 {
    (b' ' as u16) | ((DEFAULT_COLOR as u16) << 8)
}

struct Console {
    column: usize,
    row: usize,
    color: u8,
}

impl Console {
    const fn new() -> Self {
        Self {
            column: 0,
            row: 0,
            color: DEFAULT_COLOR,
        }
    }

    fn clear(&mut self) {
        let blank = blank_cell();
        for i in 0..BUFFER_WIDTH * BUFFER_HEIGHT {
            unsafe {
                core::ptr::write_volatile(VGA_BUFFER.add(i), blank);
            }
        }
        self.column = 0;
        self.row = 0;
        self.update_cursor();
    }

    fn write_byte(&mut self, byte: u8) {
        match byte {
            b'\n' => self.newline(),
            b'\r' => {
                self.column = 0;
                self.update_cursor();
            }
            b'\t' => {
                let next = (self.column + 8) & !7;
                if next >= BUFFER_WIDTH {
                    self.newline();
                } else {
                    self.column = next;
                    self.update_cursor();
                }
            }
            0x08 => self.backspace(),
            0x20..=0x7E => self.write_char(byte as char),
            _ => self.write_char('?'),
        }
    }

    fn write_char(&mut self, ch: char) {
        if self.column >= BUFFER_WIDTH {
            self.newline();
        }

        let index = self.row * BUFFER_WIDTH + self.column;
        let cell = (ch as u16) | ((self.color as u16) << 8);
        unsafe {
            core::ptr::write_volatile(VGA_BUFFER.add(index), cell);
        }

        self.column += 1;
        if self.column >= BUFFER_WIDTH {
            self.newline();
        } else {
            self.update_cursor();
        }
    }

    fn write_string(&mut self, text: &str) {
        for byte in text.bytes() {
            self.write_byte(byte);
        }
    }

    fn newline(&mut self) {
        self.column = 0;
        if self.row + 1 >= BUFFER_HEIGHT {
            self.scroll();
        } else {
            self.row += 1;
        }
        self.update_cursor();
    }

    fn backspace(&mut self) {
        if self.column > 0 {
            self.column -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.column = BUFFER_WIDTH - 1;
        } else {
            return;
        }

        let index = self.row * BUFFER_WIDTH + self.column;
        unsafe {
            core::ptr::write_volatile(VGA_BUFFER.add(index), blank_cell());
        }
        self.update_cursor();
    }

    fn scroll(&mut self) {
        // Move a linha N para N-1.
        for row in 1..BUFFER_HEIGHT {
            for column in 0..BUFFER_WIDTH {
                let src = row * BUFFER_WIDTH + column;
                let dst = (row - 1) * BUFFER_WIDTH + column;
                let cell = unsafe { core::ptr::read_volatile(VGA_BUFFER.add(src)) };
                unsafe {
                    core::ptr::write_volatile(VGA_BUFFER.add(dst), cell);
                }
            }
        }

        // Limpa a ultima linha.
        let blank = blank_cell();
        for column in 0..BUFFER_WIDTH {
            let index = (BUFFER_HEIGHT - 1) * BUFFER_WIDTH + column;
            unsafe {
                core::ptr::write_volatile(VGA_BUFFER.add(index), blank);
            }
        }

        self.row = BUFFER_HEIGHT - 1;
        self.column = 0;
    }

    fn update_cursor(&self) {
        let position = (self.row * BUFFER_WIDTH + self.column) as u16;
        unsafe {
            outb(CRTC_INDEX, 0x0F);
            outb(CRTC_DATA, (position & 0xFF) as u8);
            outb(CRTC_INDEX, 0x0E);
            outb(CRTC_DATA, (position >> 8) as u8);
        }
    }
}

impl fmt::Write for Console {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.write_string(text);
        Ok(())
    }
}

static CONSOLE: SpinLock<Console> = SpinLock::new(Console::new());

/// Inicializa o console: limpa a tela e configura o cursor.
pub fn init() {
    let mut console = CONSOLE.lock();
    console.clear();

    // Forma do cursor: linha de baixo (start=14, end=15).
    unsafe {
        outb(CRTC_INDEX, 0x0A);
        outb(CRTC_DATA, 0x0E);
        outb(CRTC_INDEX, 0x0B);
        outb(CRTC_DATA, 0x0F);
    }
    console.update_cursor();
}

/// Limpa a tela VGA (comando `clear`).
pub fn clear() {
    CONSOLE.lock().clear();
}

/// Escritor usado pelas macros print!/println!: VGA + serial.
struct ConsoleWriter;

impl fmt::Write for ConsoleWriter {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        {
            let mut console = CONSOLE.lock();
            console.write_string(text);
        }
        serial::write_str(text);
        Ok(())
    }
}

/// Ponto de entrada das macros de impressao.
pub fn _print(args: fmt::Arguments<'_>) {
    let mut writer = ConsoleWriter;
    let _ = writer.write_fmt(args);
}
