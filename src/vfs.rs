//! Camada VFS minima e tabela de descritores de arquivo.
//!
//! O ramfs e o `/proc` didatico sao expostos pela mesma interface: abrir um
//! caminho, ler/escrever a partir de um deslocamento e fechar. Isso imita o
//! ponto central do VFS do Linux (o arquivo e o descritor, nao o dispositivo),
//! ainda que sem inodes, dentries, mounts ou permissoes.
//!
//! Os descritores 0, 1 e 2 sao pre-abertos no console (`/dev/tty`). Um
//! descritor de arquivo guarda apenas o no do ramfs (ou a entrada de `/proc`),
//! o deslocamento e as flags; nao ha `struct file` alocada no heap.

use crate::proc;
use crate::ramfs;
use crate::sync::SpinLock;
use crate::tty;

/// Numero maximo de descritores simultaneos.
pub const MAX_FDS: usize = 16;

pub const O_RDONLY: u64 = 0;
#[allow(dead_code)]
pub const O_WRONLY: u64 = 1;
pub const O_RDWR: u64 = 2;
pub const O_CREAT: u64 = 0x40;
pub const O_TRUNC: u64 = 0x200;
pub const O_APPEND: u64 = 0x400;

pub const SEEK_SET: u64 = 0;
pub const SEEK_CUR: u64 = 1;
pub const SEEK_END: u64 = 2;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Free,
    File,
    Console,
    Proc,
}

#[derive(Clone, Copy)]
struct Fd {
    kind: Kind,
    node: usize,
    offset: usize,
    flags: u64,
    proc_entry: Option<proc::Entry>,
}

impl Fd {
    const fn free() -> Self {
        Self {
            kind: Kind::Free,
            node: 0,
            offset: 0,
            flags: 0,
            proc_entry: None,
        }
    }

    const fn console() -> Self {
        Self {
            kind: Kind::Console,
            node: 0,
            offset: 0,
            flags: 0,
            proc_entry: None,
        }
    }
}

struct Table {
    fds: [Fd; MAX_FDS],
}

static TABLE: SpinLock<Table> = SpinLock::new(Table {
    fds: [Fd::free(); MAX_FDS],
});

/// Inicializa a tabela e pre-abre stdin/stdout/stderr no console.
pub fn init() {
    let mut table = TABLE.lock();
    for fd in table.fds.iter_mut() {
        *fd = Fd::free();
    }
    table.fds[0] = Fd::console();
    table.fds[1] = Fd::console();
    table.fds[2] = Fd::console();
}

fn is_console_path(path: &[u8]) -> bool {
    path == b"/dev/tty" || path == b"/dev/console"
}

const PROC_PATH_MAX: usize = 256;

/// Normaliza um caminho absoluto, removendo `.`, `..` e barras repetidas.
fn normalize_path(input: &[u8], out: &mut [u8]) -> Option<usize> {
    if input.first() != Some(&b'/') {
        return None;
    }

    let mut len = 1;
    out[0] = b'/';

    let mut index = 1;
    while index < input.len() {
        while index < input.len() && input[index] == b'/' {
            index += 1;
        }
        let start = index;
        while index < input.len() && input[index] != b'/' {
            index += 1;
        }
        let component = &input[start..index];

        if component.is_empty() || component == b"." {
            continue;
        }
        if component == b".." {
            if len > 1 {
                let mut cursor = len;
                while cursor > 1 && out[cursor - 1] != b'/' {
                    cursor -= 1;
                }
                len = if cursor > 1 { cursor - 1 } else { 1 };
            }
            continue;
        }

        if len > 1 {
            if len >= out.len() {
                return None;
            }
            out[len] = b'/';
            len += 1;
        }
        if len + component.len() > out.len() {
            return None;
        }
        out[len..len + component.len()].copy_from_slice(component);
        len += component.len();
    }

    Some(len)
}

/// Converte um caminho absoluto ou relativo ao diretorio corrente em absoluto.
fn absolute_path(path: &[u8], out: &mut [u8]) -> Option<usize> {
    if path.first() == Some(&b'/') {
        return normalize_path(path, out);
    }

    let mut cwd = [0u8; PROC_PATH_MAX];
    let cwd_len = {
        let fs = ramfs::lock();
        fs.path(fs.cwd(), &mut cwd).ok()?
    };

    let mut joined = [0u8; PROC_PATH_MAX];
    let mut joined_len = cwd_len;
    joined[..joined_len].copy_from_slice(&cwd[..cwd_len]);
    if joined_len == 0 || joined[joined_len - 1] != b'/' {
        if joined_len >= joined.len() {
            return None;
        }
        joined[joined_len] = b'/';
        joined_len += 1;
    }
    if joined_len + path.len() > joined.len() {
        return None;
    }
    joined[joined_len..joined_len + path.len()].copy_from_slice(path);
    joined_len += path.len();

    normalize_path(&joined[..joined_len], out)
}

/// Resolve uma entrada de `/proc` mesmo quando o caminho e relativo ao
/// diretorio corrente (por exemplo `cd /proc; cat version`).
fn proc_entry(path: &[u8]) -> Option<proc::Entry> {
    if let Some(entry) = proc::lookup(path) {
        return Some(entry);
    }
    let mut absolute = [0u8; PROC_PATH_MAX];
    let len = absolute_path(path, &mut absolute)?;
    proc::lookup(&absolute[..len])
}

fn proc_is_dir(path: &[u8]) -> bool {
    if proc::is_dir(path) {
        return true;
    }
    let mut absolute = [0u8; PROC_PATH_MAX];
    match absolute_path(path, &mut absolute) {
        Some(len) => proc::is_dir(&absolute[..len]),
        None => false,
    }
}

/// Abre um caminho do ramfs (ou `/dev/tty`) e devolve o descritor.
pub fn open(path: &[u8], flags: u64) -> Result<usize, &'static str> {
    if path.is_empty() {
        return Err("caminho vazio");
    }

    if is_console_path(path) {
        let mut table = TABLE.lock();
        let slot = table.free_slot().ok_or("descritores esgotados")?;
        table.fds[slot] = Fd {
            kind: Kind::Console,
            node: 0,
            offset: 0,
            flags,
            proc_entry: None,
        };
        return Ok(slot);
    }

    if proc_is_dir(path) {
        return Err("e um diretorio");
    }
    if let Some(entry) = proc_entry(path) {
        let mut table = TABLE.lock();
        let slot = table.free_slot().ok_or("descritores esgotados")?;
        table.fds[slot] = Fd {
            kind: Kind::Proc,
            node: 0,
            offset: 0,
            flags,
            proc_entry: Some(entry),
        };
        return Ok(slot);
    }
    if path.starts_with(b"/proc/") {
        return Err("arquivo nao encontrado");
    }

    let index = {
        let mut fs = ramfs::lock();
        match fs.resolve(path) {
            Ok(index) => index,
            Err("arquivo nao encontrado") if flags & O_CREAT != 0 => {
                fs.create(path, false)?
            }
            Err(reason) => return Err(reason),
        }
    };

    {
        let mut fs = ramfs::lock();
        if fs.node(index).is_dir() {
            return Err("e um diretorio");
        }
        if flags & O_TRUNC != 0 {
            fs.truncate(index)?;
        }
    }

    let mut table = TABLE.lock();
    let slot = table.free_slot().ok_or("descritores esgotados")?;
    table.fds[slot] = Fd {
        kind: Kind::File,
        node: index,
        offset: 0,
        flags,
        proc_entry: None,
    };
    Ok(slot)
}

/// Fecha um descritor.
pub fn close(fd: usize) -> Result<(), &'static str> {
    let mut table = TABLE.lock();
    let entry = table.fds.get_mut(fd).ok_or("descritor invalido")?;
    if entry.kind == Kind::Free {
        return Err("descritor invalido");
    }
    *entry = Fd::free();
    Ok(())
}

/// Le de um arquivo. Para o console, o chamador deve usar a syscall de leitura
/// de linha, que trata eco e bloqueio.
pub fn read(fd: usize, out: &mut [u8]) -> Result<usize, &'static str> {
    let (kind, node, offset, proc_entry) = {
        let table = TABLE.lock();
        let entry = table.fds.get(fd).ok_or("descritor invalido")?;
        if entry.kind == Kind::Free {
            return Err("descritor invalido");
        }
        (entry.kind, entry.node, entry.offset, entry.proc_entry)
    };

    if kind == Kind::Console {
        return Err("console nao suporta read no VFS");
    }

    let read = if kind == Kind::Proc {
        let entry = proc_entry.ok_or("entrada de /proc invalida")?;
        proc::read(entry, offset, out)
    } else {
        let fs = ramfs::lock();
        fs.read_at(node, offset, out)?
    };

    let mut table = TABLE.lock();
    if let Some(entry) = table.fds.get_mut(fd) {
        entry.offset += read;
    }
    Ok(read)
}

/// Escreve em um arquivo ou no console.
pub fn write(fd: usize, data: &[u8]) -> Result<usize, &'static str> {
    let (kind, node, offset, flags) = {
        let table = TABLE.lock();
        let entry = table.fds.get(fd).ok_or("descritor invalido")?;
        if entry.kind == Kind::Free {
            return Err("descritor invalido");
        }
        (entry.kind, entry.node, entry.offset, entry.flags)
    };

    if kind == Kind::Console {
        tty::write_bytes(data);
        return Ok(data.len());
    }
    if kind == Kind::Proc {
        return Err("arquivo de /proc e somente leitura");
    }

    let start = if flags & O_APPEND != 0 {
        let fs = ramfs::lock();
        fs.node(node).size()
    } else {
        offset
    };

    let written = {
        let mut fs = ramfs::lock();
        fs.write_at(node, start, data)?
    };

    let mut table = TABLE.lock();
    if let Some(entry) = table.fds.get_mut(fd) {
        entry.offset = start + written;
    }
    Ok(written)
}

/// Reposiciona o deslocamento de um descritor.
pub fn lseek(fd: usize, offset: u64, whence: u64) -> Result<u64, &'static str> {
    let (kind, node, current, proc_entry) = {
        let table = TABLE.lock();
        let entry = table.fds.get(fd).ok_or("descritor invalido")?;
        if entry.kind == Kind::Free {
            return Err("descritor invalido");
        }
        (entry.kind, entry.node, entry.offset, entry.proc_entry)
    };
    if kind == Kind::Console {
        return Err("lseek no console");
    }

    let size = if kind == Kind::Proc {
        let entry = proc_entry.ok_or("entrada de /proc invalida")?;
        proc::size(entry)
    } else {
        let fs = ramfs::lock();
        fs.node(node).size()
    };

    let base = match whence {
        SEEK_SET => 0i64,
        SEEK_CUR => current as i64,
        SEEK_END => size as i64,
        _ => return Err("whence invalido"),
    };
    let target = base.saturating_add(offset as i64);
    if target < 0 {
        return Err("deslocamento negativo");
    }

    let target = target as usize;
    let mut table = TABLE.lock();
    if let Some(entry) = table.fds.get_mut(fd) {
        entry.offset = target;
    }
    Ok(target as u64)
}

/// Numero de descritores abertos, para diagnostico.
pub fn open_count() -> usize {
    let table = TABLE.lock();
    table
        .fds
        .iter()
        .filter(|fd| fd.kind != Kind::Free)
        .count()
}

impl Table {
    fn free_slot(&self) -> Option<usize> {
        self.fds
            .iter()
            .position(|fd| fd.kind == Kind::Free)
    }
}
