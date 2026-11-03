//! Camada TTY: oito consoles virtuais (tty0..tty7).
//!
//! Cada console tem seu proprio buffer de 80x25 celulas, cursor e fila de
//! entrada. Apenas o console ativo e desenhado no hardware VGA; trocar de
//! console copia o buffer correspondente para 0xB8000.
//!
//! O teclado PS/2 e a serial COM1 alimentam a fila do console ativo. A
//! combinacao Alt+F1..Alt+F8 troca de console, como no console virtual do
//! Linux. A leitura/escrita continua sendo feita por polling, sem scheduler.

use core::fmt::{self, Write};
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::keyboard::{self, Key};
use crate::serial;
use crate::sync::SpinLock;
use crate::vga;

pub const TTY_COUNT: usize = 8;
const INPUT_SIZE: usize = 256;
const CSI_MAX_PARAMS: usize = 4;

/// Estado do interpretador de sequencias de escape ANSI/VT100.
#[derive(Clone, Copy, PartialEq, Eq)]
enum EscapeState {
    /// Texto normal.
    Ground,
    /// Recebeu ESC e espera o proximo byte.
    Start,
    /// Recebeu ESC [ e esta lendo parametros CSI.
    Csi,
}

#[derive(Clone, Copy)]
struct Tty {
    cells: [u16; vga::CELLS],
    row: usize,
    column: usize,
    input: [u8; INPUT_SIZE],
    head: usize,
    tail: usize,
    escape: EscapeState,
    csi_params: [u16; CSI_MAX_PARAMS],
    csi_count: usize,
    csi_value: u16,
    csi_digits: bool,
}

impl Tty {
    const fn new() -> Self {
        Self {
            cells: [vga::blank_cell(); vga::CELLS],
            row: 0,
            column: 0,
            input: [0; INPUT_SIZE],
            head: 0,
            tail: 0,
            escape: EscapeState::Ground,
            csi_params: [0; CSI_MAX_PARAMS],
            csi_count: 0,
            csi_value: 0,
            csi_digits: false,
        }
    }

    fn clear(&mut self) {
        self.cells = [vga::blank_cell(); vga::CELLS];
        self.row = 0;
        self.column = 0;
        self.escape = EscapeState::Ground;
        self.csi_count = 0;
        self.csi_value = 0;
        self.csi_digits = false;
    }

    fn push_input(&mut self, byte: u8) {
        let next = (self.tail + 1) % INPUT_SIZE;
        if next != self.head {
            self.input[self.tail] = byte;
            self.tail = next;
        }
    }

    fn pop_input(&mut self) -> Option<u8> {
        if self.head == self.tail {
            return None;
        }
        let byte = self.input[self.head];
        self.head = (self.head + 1) % INPUT_SIZE;
        Some(byte)
    }

    fn write_bytes(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.write_byte(byte);
        }
    }

    fn write_byte(&mut self, byte: u8) {
        match self.escape {
            EscapeState::Ground => {
                if byte == 0x1B {
                    self.escape = EscapeState::Start;
                } else {
                    self.write_plain(byte);
                }
            }
            EscapeState::Start => {
                if byte == b'[' {
                    self.escape = EscapeState::Csi;
                    self.csi_params = [0; CSI_MAX_PARAMS];
                    self.csi_count = 0;
                    self.csi_value = 0;
                    self.csi_digits = false;
                } else {
                    // ESC seguido de outra coisa (ex.: ESC 7/ESC 8) e
                    // descartado por enquanto.
                    self.escape = EscapeState::Ground;
                }
            }
            EscapeState::Csi => self.write_csi(byte),
        }
    }

    fn write_plain(&mut self, byte: u8) {
        match byte {
            b'\n' => self.newline(),
            b'\r' => self.column = 0,
            b'\t' => {
                let next = (self.column + 8) & !7;
                if next >= vga::BUFFER_WIDTH {
                    self.newline();
                } else {
                    self.column = next;
                }
            }
            0x08 | 0x7F => self.backspace(),
            0x20..=0x7E => self.write_char(byte as char),
            _ => self.write_char('?'),
        }
    }

    fn write_csi(&mut self, byte: u8) {
        match byte {
            b'0'..=b'9' => {
                let digit = (byte - b'0') as u16;
                self.csi_value = self
                    .csi_value
                    .saturating_mul(10)
                    .saturating_add(digit);
                self.csi_digits = true;
            }
            b';' => {
                self.push_csi_param();
                self.csi_value = 0;
                self.csi_digits = false;
            }
            // Marcadores privados (ex.: ESC [ ? 25 l) e bytes intermediarios
            // sao aceitos e ignorados.
            0x20..=0x3F => {}
            0x40..=0x7E => {
                if self.csi_digits || self.csi_count > 0 {
                    self.push_csi_param();
                }
                self.escape = EscapeState::Ground;
                self.execute_csi(byte);
            }
            _ => self.escape = EscapeState::Ground,
        }
    }

    fn push_csi_param(&mut self) {
        if self.csi_count < CSI_MAX_PARAMS {
            self.csi_params[self.csi_count] = self.csi_value;
            self.csi_count += 1;
        }
    }

    fn csi_param(&self, index: usize, default: u16) -> u16 {
        if index < self.csi_count {
            self.csi_params[index]
        } else {
            default
        }
    }

    fn csi_count_param(&self, index: usize) -> usize {
        self.csi_param(index, 1).max(1) as usize
    }

    fn execute_csi(&mut self, final_byte: u8) {
        match final_byte {
            // EL: apaga parte da linha.
            b'K' => self.erase_line(self.csi_param(0, 0)),
            // ED: apaga parte da tela.
            b'J' => self.erase_display(self.csi_param(0, 0)),
            // CUP/HVP: posiciona o cursor (linha; coluna, base 1).
            b'H' | b'f' => {
                let row = self.csi_count_param(0);
                let column = self.csi_count_param(1);
                self.row = (row - 1).min(vga::BUFFER_HEIGHT - 1);
                self.column = (column - 1).min(vga::BUFFER_WIDTH - 1);
            }
            b'A' => {
                let count = self.csi_count_param(0);
                self.row = self.row.saturating_sub(count);
            }
            b'B' => {
                let count = self.csi_count_param(0);
                self.row = (self.row + count).min(vga::BUFFER_HEIGHT - 1);
            }
            b'C' => {
                let count = self.csi_count_param(0);
                self.column = (self.column + count).min(vga::BUFFER_WIDTH - 1);
            }
            b'D' => {
                let count = self.csi_count_param(0);
                self.column = self.column.saturating_sub(count);
            }
            // SGR (cores), modos e outras sequencias nao mudam o texto.
            b'm' | b'h' | b'l' => {}
            _ => {}
        }
    }

    fn erase_display(&mut self, mode: u16) {
        match mode {
            // Do cursor ate o fim da tela.
            0 => {
                self.erase_line(0);
                for row in (self.row + 1)..vga::BUFFER_HEIGHT {
                    self.blank_row(row);
                }
            }
            // Do inicio da tela ate o cursor.
            1 => {
                for row in 0..self.row {
                    self.blank_row(row);
                }
                self.erase_line(1);
            }
            // Tela inteira. O cursor permanece onde estava; o comando clear
            // envia ESC [ H em seguida para voltar ao canto superior esquerdo.
            2 | 3 => self.cells = [vga::blank_cell(); vga::CELLS],
            _ => {}
        }
    }

    fn erase_line(&mut self, mode: u16) {
        let start = self.row * vga::BUFFER_WIDTH;
        match mode {
            // Do cursor ate o fim da linha.
            0 => {
                for column in self.column..vga::BUFFER_WIDTH {
                    self.cells[start + column] = vga::blank_cell();
                }
            }
            // Do inicio da linha ate o cursor.
            1 => {
                for column in 0..=self.column.min(vga::BUFFER_WIDTH - 1) {
                    self.cells[start + column] = vga::blank_cell();
                }
            }
            // Linha inteira.
            2 => self.blank_row(self.row),
            _ => {}
        }
    }

    fn blank_row(&mut self, row: usize) {
        let start = row * vga::BUFFER_WIDTH;
        for column in 0..vga::BUFFER_WIDTH {
            self.cells[start + column] = vga::blank_cell();
        }
    }

    fn write_char(&mut self, ch: char) {
        if self.column >= vga::BUFFER_WIDTH {
            self.newline();
        }

        let index = self.row * vga::BUFFER_WIDTH + self.column;
        self.cells[index] = (ch as u16) | ((vga::DEFAULT_COLOR as u16) << 8);
        self.column += 1;
        if self.column >= vga::BUFFER_WIDTH {
            self.newline();
        }
    }

    fn newline(&mut self) {
        self.column = 0;
        if self.row + 1 >= vga::BUFFER_HEIGHT {
            self.scroll();
        } else {
            self.row += 1;
        }
    }

    fn backspace(&mut self) {
        if self.column > 0 {
            self.column -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.column = vga::BUFFER_WIDTH - 1;
        } else {
            return;
        }

        let index = self.row * vga::BUFFER_WIDTH + self.column;
        self.cells[index] = vga::blank_cell();
    }

    fn scroll(&mut self) {
        for row in 1..vga::BUFFER_HEIGHT {
            for column in 0..vga::BUFFER_WIDTH {
                let src = row * vga::BUFFER_WIDTH + column;
                let dst = (row - 1) * vga::BUFFER_WIDTH + column;
                self.cells[dst] = self.cells[src];
            }
        }

        let last = (vga::BUFFER_HEIGHT - 1) * vga::BUFFER_WIDTH;
        for column in 0..vga::BUFFER_WIDTH {
            self.cells[last + column] = vga::blank_cell();
        }

        self.row = vga::BUFFER_HEIGHT - 1;
        self.column = 0;
    }
}

static TTYS: SpinLock<[Tty; TTY_COUNT]> = SpinLock::new([Tty::new(); TTY_COUNT]);
static ACTIVE: AtomicUsize = AtomicUsize::new(0);

/// Inicializa os oito consoles e desenha tty0.
pub fn init() {
    let mut ttys = TTYS.lock();
    for tty in ttys.iter_mut() {
        tty.clear();
    }
    ACTIVE.store(0, Ordering::Relaxed);
    vga::render(&ttys[0].cells, 0, 0);
}

/// Indice do console ativo (0..7).
pub fn active() -> usize {
    ACTIVE.load(Ordering::Relaxed)
}

/// Troca para o console `index`. Retorna `false` se o indice for invalido.
pub fn switch(index: usize) -> bool {
    if index >= TTY_COUNT {
        return false;
    }

    let ttys = TTYS.lock();
    ACTIVE.store(index, Ordering::Relaxed);
    let tty = &ttys[index];
    vga::render(&tty.cells, tty.row, tty.column);
    true
}

/// Limpa o console ativo com a sequencia ANSI/VT100 `ESC [ 2 J ESC [ H`.
///
/// A sequencia passa pelo mesmo caminho de saida dos programas de usuario
/// (incluindo o espelhamento na serial), em vez de mexer diretamente no
/// buffer do console.
pub fn clear() {
    write_bytes(b"\x1b[2J\x1b[H");
}

/// Escreve no console ativo.
pub fn write_bytes(bytes: &[u8]) {
    write_to(active(), bytes);
}

/// Escreve em um console especifico. O console ativo e redesenhado e toda a
/// saida tambem e espelhada na serial (util para testes headless).
pub fn write_to(index: usize, bytes: &[u8]) {
    if index >= TTY_COUNT {
        return;
    }

    let active = active();
    {
        let mut ttys = TTYS.lock();
        ttys[index].write_bytes(bytes);
        if index == active {
            let tty = &ttys[index];
            vga::render(&tty.cells, tty.row, tty.column);
        }
    }

    serial::write_bytes(bytes);
}

/// Le um byte da fila do console ativo.
pub fn read_byte() -> Option<u8> {
    read_byte_from(active())
}

/// Le um byte de um console especifico. Se ele nao for o ativo, a entrada
/// continua na fila para quando o console voltar a ser selecionado.
pub fn read_byte_from(index: usize) -> Option<u8> {
    if index >= TTY_COUNT || index != active() {
        return None;
    }
    TTYS.lock()[index].pop_input()
}

/// Bombeia teclado e serial para a fila do console ativo. Tambem trata
/// Alt+F1..Alt+F8. Deve ser chamada pelos lacos que esperam entrada.
pub fn pump_input() {
    loop {
        match keyboard::read_key() {
            Some(Key::Char(byte)) => push_input(byte),
            Some(Key::Console(index)) => {
                let _ = switch(index);
            }
            None => break,
        }
    }

    while let Some(byte) = serial::read_byte() {
        push_input(byte);
    }
}

fn push_input(byte: u8) {
    let active = active();
    TTYS.lock()[active].push_input(byte);
}

/// Escritor usado pelas macros print!/println!.
struct TtyWriter;

impl fmt::Write for TtyWriter {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        write_bytes(text.as_bytes());
        Ok(())
    }
}

/// Ponto de entrada das macros de impressao.
pub fn _print(args: fmt::Arguments<'_>) {
    let mut writer = TtyWriter;
    let _ = writer.write_fmt(args);
}
