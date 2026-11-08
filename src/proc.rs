//! Sistema de arquivos virtual `/proc` (didatico).
//!
//! O Linux monta em `/proc` uma interface para o estado do kernel: uptime,
//! memoria, tarefas, versao e varios outros arquivos gerados sob demanda. Este
//! modulo implementa o mesmo conceito em escala reduzida. Nada e armazenado no
//! ramfs: cada leitura formata o conteudo atual em um buffer fixo e devolve os
//! bytes a partir do deslocamento do descritor.
//!
//! Os arquivos disponiveis sao:
//!
//!   /proc/version   versao e arquitetura
//!   /proc/uptime    tempo desde o boot (ticks do PIT)
//!   /proc/meminfo   frames, heap e tarefas
//!   /proc/ps        tabela de tarefas no formato do applet `ps`
//!   /proc/signals   sinais pendentes/bloqueados e status de saida
//!   /proc/cpuinfo   informacoes basicas da CPU
//!   /proc/mounts    sistemas de arquivos montados
//!   /proc/cmdline   linha de comando do boot
//!
//! Nao ha `proc_dir_entry`, seq\_file, permissao ou espaco de nomes: e um
//! `match` sobre o caminho e um `core::fmt::Write` sobre um buffer fixo.

use core::fmt::{self, Write};

use crate::{heap, idt, paging, task};

/// Cada arquivo virtual suportado.
#[derive(Clone, Copy, PartialEq)]
pub enum Entry {
    Version,
    Uptime,
    Meminfo,
    Ps,
    Signals,
    Cpuinfo,
    Mounts,
    Cmdline,
}

/// Arquivos listados por `ls /proc`, na ordem em que aparecem.
pub const FILES: &[(&str, Entry)] = &[
    ("version", Entry::Version),
    ("uptime", Entry::Uptime),
    ("meminfo", Entry::Meminfo),
    ("ps", Entry::Ps),
    ("signals", Entry::Signals),
    ("cpuinfo", Entry::Cpuinfo),
    ("mounts", Entry::Mounts),
    ("cmdline", Entry::Cmdline),
];

/// Resolve `/proc/<nome>` (ou `/proc/<nome>/`) para uma entrada.
pub fn lookup(path: &[u8]) -> Option<Entry> {
    let rest = path.strip_prefix(b"/proc/")?;
    let name = rest.strip_suffix(b"/").unwrap_or(rest);
    FILES
        .iter()
        .find(|(candidate, _)| candidate.as_bytes() == name)
        .map(|(_, entry)| *entry)
}

pub fn is_dir(path: &[u8]) -> bool {
    path == b"/proc" || path == b"/proc/"
}

/// Tamanho atual do arquivo virtual (o conteudo e regenerado a cada leitura).
pub fn size(entry: Entry) -> usize {
    let mut buffer = [0u8; BUF_SIZE];
    render(entry, &mut buffer)
}

/// Le `out.len()` bytes a partir de `offset`, como um arquivo regular.
pub fn read(entry: Entry, offset: usize, out: &mut [u8]) -> usize {
    let mut buffer = [0u8; BUF_SIZE];
    let len = render(entry, &mut buffer);
    if offset >= len {
        return 0;
    }
    let count = (len - offset).min(out.len());
    out[..count].copy_from_slice(&buffer[offset..offset + count]);
    count
}

const BUF_SIZE: usize = 2048;

struct Buffer<'a> {
    data: &'a mut [u8],
    len: usize,
}

impl Buffer<'_> {
    fn new(data: &mut [u8]) -> Buffer<'_> {
        Buffer { data, len: 0 }
    }
}

impl Write for Buffer<'_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let bytes = text.as_bytes();
        if self.len + bytes.len() > self.data.len() {
            return Err(fmt::Error);
        }
        self.data[self.len..self.len + bytes.len()].copy_from_slice(bytes);
        self.len += bytes.len();
        Ok(())
    }
}

fn render(entry: Entry, out: &mut [u8]) -> usize {
    let mut writer = Buffer::new(out);
    let _ = match entry {
        Entry::Version => write!(
            writer,
            "rinux 0.1.0 (Rust, no_std, x86_64) #1 SMP\n"
        ),
        Entry::Uptime => {
            let ticks = idt::ticks();
            write!(
                writer,
                "{}.{:02} {}.{:02}\n",
                ticks / 100,
                ticks % 100,
                0,
                0
            )
        }
        Entry::Meminfo => render_meminfo(&mut writer),
        Entry::Ps => render_ps(&mut writer),
        Entry::Signals => render_signals(&mut writer),
        Entry::Cpuinfo => write!(
            writer,
            "processor\t: 0\n\
             vendor_id\t: GenuineIntel\n\
             cpu family\t: 6\n\
             model name\t: QEMU Virtual CPU (x86_64)\n\
             flags\t\t: fpu tsc msr pae pge apic\n"
        ),
        Entry::Mounts => write!(
            writer,
            "ramfs / ramfs rw 0 0\n\
             proc /proc proc ro 0 0\n\
             devtmpfs /dev devtmpfs rw 0 0\n"
        ),
        Entry::Cmdline => write!(writer, "BOOT_IMAGE=/boot/kernel.elf console=tty0\n"),
    };
    writer.len
}

fn render_meminfo(writer: &mut Buffer<'_>) -> fmt::Result {
    let memory = paging::stats();
    let heap_stats = heap::stats();
    let tasks = task::stats();
    let total_kib = (memory.allocated + memory.free) * 4;

    write!(
        writer,
        "MemTotal:       {:>8} KiB\n\
         MemFree:        {:>8} KiB\n\
         PageFaults:     {:>8}\n\
         HeapMapped:     {:>8} KiB\n\
         HeapUsed:       {:>8} B\n\
         Tasks:          {:>8}\n\
         TasksReady:     {:>8}\n\
         TasksBlocked:   {:>8}\n\
         TasksStopped:   {:>8}\n\
         TasksExited:    {:>8}\n",
        total_kib,
        memory.free * 4,
        memory.faults,
        heap_stats.mapped / 1024,
        heap_stats.used,
        tasks.total,
        tasks.ready,
        tasks.blocked,
        tasks.stopped,
        tasks.exited
    )
}

fn render_ps(writer: &mut Buffer<'_>) -> fmt::Result {
    write!(
        writer,
        "  PID  PPID  ESTADO      NICE  CPU  VRUNTIME   YIELDS  PREEMP  NOME\n"
    )?;
    task::for_each(|info| {
        let marker = if info.current { '*' } else { ' ' };
        let _ = write!(
            writer,
            "{}{:>5} {:>5}  {:<11} {:>4} {:>4}  {:>9}  {:>6}  {:>6}  {}\n",
            marker,
            info.pid,
            info.ppid,
            info.state.as_str(),
            info.nice,
            info.cpu_ticks,
            info.vruntime,
            info.yields,
            info.preemptions,
            info.name
        );
    });
    Ok(())
}

fn render_signals(writer: &mut Buffer<'_>) -> fmt::Result {
    write!(
        writer,
        "  PID  NOME        PENDENTES  BLOQUEADOS  SITUACAO\n"
    )?;
    task::for_each(|info| {
        let _ = write!(
            writer,
            "{:>5}  {:<10}  {:#08x}  {:#08x}",
            info.pid, info.name, info.pending, info.blocked
        );
        if info.state == task::State::Exited {
            if info.exit_signal != 0 {
                let _ = write!(
                    writer,
                    "  sinal {} status {}\n",
                    task::signal_name(info.exit_signal),
                    info.exit_status
                );
            } else {
                let _ = write!(writer, "  status {}\n", info.exit_status);
            }
        } else {
            let _ = write!(writer, "  {}\n", info.state.as_str());
        }
    });
    Ok(())
}
