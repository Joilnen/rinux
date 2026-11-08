//! Shell minimo no estilo BusyBox, com applets em Rust.
//!
//! O shell le uma linha do teclado PS/2 ou da serial (COM1), separa os
//! argumentos (com suporte a aspas simples/duplas e `\`) e despacha para uma
//! tabela de applets. Os comandos de arquivo usam o ramfs de `ramfs.rs`.
//!
//! Nao ha pipes, redirecionamento, variaveis de ambiente, `fork` ou `exec`.
//! O shell roda como a tarefa de PID 1 de `task.rs`; tarefas de kernel podem
//! ser criadas com `spawn` e sao escalonadas de forma preemptiva (weighted fair
//! com `nice`, peso e `vruntime`, no estilo do CFS).

use core::sync::atomic::{AtomicU64, Ordering};

use crate::{arch, heap, idt, paging, println, proc, ramfs, task, tty, user, vfs};

const LINE_MAX: usize = 128;
const MAX_ARGS: usize = 24;

/// Fila de espera de demonstracao do applet `notify`, no estilo
/// `wait_queue_head_t` do Linux. Workers criados com `spawn ... wait` dormem
/// nela ate alguem sinalizar o evento.
static EVENT_QUEUE: task::WaitQueue = task::WaitQueue::new();

/// Contador monotonico de eventos. A condicao de `wait_event` compara o valor
/// atual com o observado antes de dormir, evitando wakeups espurios.
static EVENT_COUNT: AtomicU64 = AtomicU64::new(0);

/// Um argumento dentro da linha ja desquotada pelo parser.
#[derive(Clone, Copy)]
struct Span {
    start: usize,
    end: usize,
}

impl Span {
    const EMPTY: Self = Self { start: 0, end: 0 };
}

/// Linha de comando interpretada.
struct Command<'a> {
    line: &'a [u8],
    spans: [Span; MAX_ARGS],
    count: usize,
}

impl<'a> Command<'a> {
    /// Separa a linha em argumentos. A desquotacao e feita no proprio buffer,
    /// sempre escrevendo atras da posicao de leitura.
    fn parse(line: &'a mut [u8]) -> Result<Self, &'static str> {
        let mut spans = [Span::EMPTY; MAX_ARGS];
        let mut count = 0;
        let mut read = 0;
        let mut write = 0;

        while read < line.len() {
            while read < line.len() && line[read].is_ascii_whitespace() {
                read += 1;
            }
            if read >= line.len() {
                break;
            }
            if count >= MAX_ARGS {
                return Err("argumentos demais");
            }

            let start = write;
            let mut started = false;
            let mut quote = 0u8;

            while read < line.len() {
                let byte = line[read];
                if quote != 0 {
                    if byte == quote {
                        quote = 0;
                        read += 1;
                        started = true;
                    } else if byte == b'\\' && quote == b'"' && read + 1 < line.len() {
                        read += 1;
                        line[write] = line[read];
                        write += 1;
                        read += 1;
                        started = true;
                    } else {
                        line[write] = byte;
                        write += 1;
                        read += 1;
                        started = true;
                    }
                } else if byte == b'\'' || byte == b'"' {
                    quote = byte;
                    read += 1;
                    started = true;
                } else if byte == b'\\' && read + 1 < line.len() {
                    read += 1;
                    line[write] = line[read];
                    write += 1;
                    read += 1;
                    started = true;
                } else if byte.is_ascii_whitespace() {
                    break;
                } else {
                    line[write] = byte;
                    write += 1;
                    read += 1;
                    started = true;
                }
            }

            if quote != 0 {
                return Err("aspas sem fechamento");
            }
            if started {
                spans[count] = Span { start, end: write };
                count += 1;
            }
        }

        Ok(Self { line, spans, count })
    }

    fn len(&self) -> usize {
        self.count
    }

    fn is_empty(&self) -> bool {
        self.count == 0
    }

    fn arg(&self, index: usize) -> Option<&'a [u8]> {
        if index >= self.count {
            return None;
        }
        let span = self.spans[index];
        Some(&self.line[span.start..span.end])
    }
}

type AppletFn = fn(&Command<'_>) -> Result<(), &'static str>;

struct Applet {
    name: &'static str,
    usage: &'static str,
    summary: &'static str,
    run: AppletFn,
}

static APPLETS: &[Applet] = &[
    Applet {
        name: "help",
        usage: "help [comando]",
        summary: "lista os applets ou mostra a ajuda de um deles",
        run: applet_help,
    },
    Applet {
        name: "echo",
        usage: "echo [-n] [texto...]",
        summary: "imprime o texto separado por espacos",
        run: applet_echo,
    },
    Applet {
        name: "clear",
        usage: "clear",
        summary: "limpa a tela com ANSI ESC[2J ESC[H",
        run: applet_clear,
    },
    Applet {
        name: "pwd",
        usage: "pwd",
        summary: "mostra o diretorio corrente",
        run: applet_pwd,
    },
    Applet {
        name: "cd",
        usage: "cd [diretorio]",
        summary: "troca o diretorio corrente",
        run: applet_cd,
    },
    Applet {
        name: "ls",
        usage: "ls [-l] [caminho]",
        summary: "lista arquivos e diretorios",
        run: applet_ls,
    },
    Applet {
        name: "cat",
        usage: "cat [-n] <arquivo>...",
        summary: "mostra o conteudo de arquivos",
        run: applet_cat,
    },
    Applet {
        name: "mkdir",
        usage: "mkdir <diretorio>...",
        summary: "cria diretorios",
        run: applet_mkdir,
    },
    Applet {
        name: "touch",
        usage: "touch <arquivo>...",
        summary: "cria arquivos vazios",
        run: applet_touch,
    },
    Applet {
        name: "write",
        usage: "write [-a] <arquivo> <texto...>",
        summary: "grava ou anexa texto a um arquivo",
        run: applet_write,
    },
    Applet {
        name: "rm",
        usage: "rm [-r] <caminho>...",
        summary: "remove arquivos e diretorios",
        run: applet_rm,
    },
    Applet {
        name: "rmdir",
        usage: "rmdir <diretorio>...",
        summary: "remove diretorios vazios",
        run: applet_rmdir,
    },
    Applet {
        name: "cp",
        usage: "cp <origem> <destino>",
        summary: "copia um arquivo",
        run: applet_cp,
    },
    Applet {
        name: "mv",
        usage: "mv <origem> <destino>",
        summary: "move ou renomeia um arquivo/diretorio",
        run: applet_mv,
    },
    Applet {
        name: "tree",
        usage: "tree [diretorio]",
        summary: "mostra a arvore do ramfs",
        run: applet_tree,
    },
    Applet {
        name: "uname",
        usage: "uname [-asnrm]",
        summary: "mostra informacoes do sistema",
        run: applet_uname,
    },
    Applet {
        name: "uptime",
        usage: "uptime",
        summary: "mostra o tempo desde o boot e os ticks",
        run: applet_uptime,
    },
    Applet {
        name: "free",
        usage: "free",
        summary: "mostra frames fisicos livres e usados",
        run: applet_free,
    },
    Applet {
        name: "heap",
        usage: "heap",
        summary: "testa e mostra o heap do kernel",
        run: applet_heap,
    },
    Applet {
        name: "vfs",
        usage: "vfs",
        summary: "testa open/read/write/lseek/close do VFS",
        run: applet_vfs,
    },
    Applet {
        name: "filetest",
        usage: "filetest",
        summary: "abre e le /etc/motd em ring 3 via syscalls",
        run: applet_filetest,
    },
    Applet {
        name: "ps",
        usage: "ps",
        summary: "lista as tarefas do escalonador",
        run: applet_ps,
    },
    Applet {
        name: "tasks",
        usage: "tasks",
        summary: "alias de ps",
        run: applet_ps,
    },
    Applet {
        name: "spawn",
        usage: "spawn [nome] [n] [cpu|sleep|wait|timeout|signal|exit] [nice]",
        summary: "cria tarefas de kernel preemptivas, dormentes, em wait queue ou com handler",
        run: applet_spawn,
    },
    Applet {
        name: "nice",
        usage: "nice [pid] [valor]",
        summary: "consulta ou ajusta a prioridade (nice) de uma tarefa",
        run: applet_nice,
    },
    Applet {
        name: "kill",
        usage: "kill <pid> [sinal]",
        summary: "envia um sinal para uma tarefa (padrao: TERM)",
        run: applet_kill,
    },
    Applet {
        name: "sig",
        usage: "sig [pid]",
        summary: "mostra sinais pendentes e bloqueados das tarefas",
        run: applet_sig,
    },
    Applet {
        name: "sigaction",
        usage: "sigaction <pid> <sinal> [default|ignore|handler]",
        summary: "consulta ou instala a acao de um sinal",
        run: applet_sigaction,
    },
    Applet {
        name: "sigblock",
        usage: "sigblock <pid> <sinal> <on|off>",
        summary: "bloqueia ou desbloqueia um sinal de uma tarefa",
        run: applet_sigblock,
    },
    Applet {
        name: "wait",
        usage: "wait [pid]",
        summary: "espera um filho encerrar e mostra o status",
        run: applet_wait,
    },
    Applet {
        name: "sleep",
        usage: "sleep [segundos]",
        summary: "bloqueia o shell por N segundos (schedule_timeout)",
        run: applet_sleep,
    },
    Applet {
        name: "notify",
        usage: "notify [one]",
        summary: "acorda tarefas em wait queue (wake_up/_all)",
        run: applet_notify,
    },
    Applet {
        name: "whoami",
        usage: "whoami",
        summary: "mostra o usuario da sessao",
        run: applet_whoami,
    },
    Applet {
        name: "hostname",
        usage: "hostname",
        summary: "mostra o nome do host",
        run: applet_hostname,
    },
    Applet {
        name: "info",
        usage: "info",
        summary: "mostra a configuracao do kernel e do console",
        run: applet_info,
    },
    Applet {
        name: "tty",
        usage: "tty [0..7]",
        summary: "mostra ou troca o console virtual",
        run: applet_tty,
    },
    Applet {
        name: "getty",
        usage: "getty [0..7]",
        summary: "roda o login em ring 3 no console",
        run: applet_getty,
    },
    Applet {
        name: "demand",
        usage: "demand",
        summary: "testa demand paging no kernel",
        run: applet_demand,
    },
    Applet {
        name: "usertest",
        usage: "usertest",
        summary: "roda a demo de demand paging em ring 3",
        run: applet_usertest,
    },
    Applet {
        name: "userprot",
        usage: "userprot",
        summary: "testa a protecao usuario/kernel",
        run: applet_userprot,
    },
    Applet {
        name: "mem",
        usage: "mem",
        summary: "estatisticas de frames e paginas",
        run: applet_mem,
    },
    Applet {
        name: "reboot",
        usage: "reboot",
        summary: "reinicia o PC",
        run: applet_reboot,
    },
    Applet {
        name: "halt",
        usage: "halt",
        summary: "para a CPU",
        run: applet_halt,
    },
];

pub fn run() -> ! {
    loop {
        // O shell e a tarefa PID 1: entrega sinais pendentes (por exemplo,
        // SIGCHLD ou um SIGUSR1 instalado por `sigaction`) antes do prompt.
        task::check_signals();

        let mut line = [0u8; LINE_MAX];
        let mut len = 0usize;
        let mut console = tty::active();

        crate::print!("rinux> ");

        loop {
            // Bombeia teclado e serial para o console ativo e trata
            // Alt+F1..Alt+F8.
            tty::pump_input();

            // Se o usuario trocou de console, a linha parcial pertence ao
            // console anterior: recomeca o prompt no novo.
            if tty::active() != console {
                console = tty::active();
                len = 0;
                crate::print!("rinux> ");
                continue;
            }

            let byte = match tty::read_byte() {
                Some(byte) => byte,
                None => {
                    // Bloqueia por um tick e volta a pollar em ~10 ms; sem
                    // outra tarefa, o `hlt` interno evita busy-wait.
                    task::idle();
                    task::check_signals();
                    continue;
                }
            };

            match byte {
                b'\n' | b'\r' => {
                    println!();
                    execute(&mut line[..len]);
                    break;
                }
                0x08 | 0x7F => {
                    if len > 0 {
                        len -= 1;
                        crate::print!("\x08");
                    }
                }
                b'\t' => {}
                0x20..=0x7E => {
                    if len < LINE_MAX {
                        line[len] = byte;
                        len += 1;
                        crate::print!("{}", byte as char);
                    }
                }
                _ => {}
            }
        }
    }
}

fn execute(line: &mut [u8]) {
    let command = match Command::parse(line) {
        Ok(command) => command,
        Err(reason) => {
            println!("shell: {}", reason);
            return;
        }
    };

    if command.is_empty() {
        return;
    }

    let name = command.arg(0).unwrap();
    if name.is_empty() {
        return;
    }
    match find_applet(name) {
        Some(applet) => match (applet.run)(&command) {
            Ok(()) => {}
            Err(reason) => println!("{}: {}", applet.name, reason),
        },
        None => {
            println!("comando desconhecido: {}", as_text(name));
            println!("digite 'help' para a lista de comandos");
        }
    }
}

fn find_applet(name: &[u8]) -> Option<&'static Applet> {
    APPLETS.iter().find(|applet| applet.name.as_bytes() == name)
}

fn as_text(bytes: &[u8]) -> &str {
    core::str::from_utf8(bytes).unwrap_or("<bytes invalidos>")
}

fn no_args(command: &Command<'_>) -> Result<(), &'static str> {
    if command.len() > 1 {
        Err("argumentos demais")
    } else {
        Ok(())
    }
}

fn applet_help(command: &Command<'_>) -> Result<(), &'static str> {
    if command.len() == 1 {
        println!("Comandos disponiveis:");
        for applet in APPLETS {
            println!("  {:<14} {}", applet.name, applet.summary);
        }
        return Ok(());
    }
    if command.len() > 2 {
        return Err("uso: help [comando]");
    }

    match find_applet(command.arg(1).unwrap()) {
        Some(applet) => {
            println!("uso: {}", applet.usage);
            println!("{}", applet.summary);
            Ok(())
        }
        None => Err("applet desconhecido"),
    }
}

fn applet_echo(command: &Command<'_>) -> Result<(), &'static str> {
    let mut start = 1;
    let mut newline = true;
    if command.arg(1) == Some(b"-n") {
        newline = false;
        start = 2;
    }

    let mut first = true;
    for index in start..command.len() {
        if !first {
            tty::write_bytes(b" ");
        }
        tty::write_bytes(command.arg(index).unwrap());
        first = false;
    }
    if newline {
        tty::write_bytes(b"\n");
    }
    Ok(())
}

fn applet_clear(command: &Command<'_>) -> Result<(), &'static str> {
    no_args(command)?;
    tty::clear();
    Ok(())
}

fn applet_pwd(command: &Command<'_>) -> Result<(), &'static str> {
    no_args(command)?;

    let mut buffer = [0u8; LINE_MAX];
    let len = {
        let fs = ramfs::lock();
        fs.path(fs.cwd(), &mut buffer)?
    };
    tty::write_bytes(&buffer[..len]);
    tty::write_bytes(b"\n");
    Ok(())
}

fn applet_cd(command: &Command<'_>) -> Result<(), &'static str> {
    if command.len() > 2 {
        return Err("uso: cd [diretorio]");
    }

    let path = command.arg(1).unwrap_or(b"/");
    let mut fs = ramfs::lock();
    let index = fs.resolve(path)?;
    if !fs.node(index).is_dir() {
        return Err("nao e um diretorio");
    }
    fs.set_cwd(index);
    Ok(())
}

fn applet_ls(command: &Command<'_>) -> Result<(), &'static str> {
    let mut long = false;
    let mut path: &[u8] = b".";
    let mut have_path = false;

    for index in 1..command.len() {
        let argument = command.arg(index).unwrap();
        if argument == b"-l" {
            long = true;
        } else if argument.starts_with(b"-") {
            return Err("uso: ls [-l] [caminho]");
        } else if have_path {
            return Err("uso: ls [-l] [caminho]");
        } else {
            path = argument;
            have_path = true;
        }
    }

    if proc::is_dir(path) {
        list_proc(long);
        return Ok(());
    }
    if let Some(entry) = proc::lookup(path) {
        print_proc_entry(entry, long);
        return Ok(());
    }

    let fs = ramfs::lock();
    let index = fs.resolve(path)?;

    // O diretorio `/proc` existe no ramfs como ponto de montagem, mas seus
    // filhos sao virtuais. Reconhece tanto `ls /proc` quanto `cd /proc; ls`.
    let mut absolute = [0u8; LINE_MAX];
    let absolute_len = fs.path(index, &mut absolute)?;
    if &absolute[..absolute_len] == b"/proc" {
        drop(fs);
        list_proc(long);
        return Ok(());
    }

    let node = fs.node(index);

    if !node.is_dir() {
        print_entry(node, long);
        return Ok(());
    }

    let mut child = node.first_child();
    while child != ramfs::NO_NODE {
        let entry = fs.node(child);
        print_entry(entry, long);
        child = entry.next_sibling();
    }
    Ok(())
}

fn list_proc(long: bool) {
    for (name, entry) in proc::FILES {
        if long {
            println!("- {:>6} {}", proc::size(*entry), name);
        } else {
            println!("{}", name);
        }
    }
}

fn print_proc_entry(entry: proc::Entry, long: bool) {
    let name = proc::FILES
        .iter()
        .find(|(_, candidate)| *candidate == entry)
        .map(|(name, _)| *name)
        .unwrap_or("?");
    if long {
        println!("- {:>6} {}", proc::size(entry), name);
    } else {
        println!("{}", name);
    }
}

fn print_entry(node: &ramfs::Node, long: bool) {
    if long {
        crate::print!(
            "{} {:>6} ",
            if node.is_dir() { 'd' } else { '-' },
            node.size()
        );
    }
    tty::write_bytes(node.name());
    if node.is_dir() {
        tty::write_bytes(b"/");
    }
    println!();
}

fn applet_cat(command: &Command<'_>) -> Result<(), &'static str> {
    let mut number = false;
    let mut start = 1;
    if command.arg(1) == Some(b"-n") {
        number = true;
        start = 2;
    }
    if command.len() <= start {
        return Err("uso: cat [-n] <arquivo>...");
    }

    let mut line_number = 1u64;
    let mut line_start = true;

    for index in start..command.len() {
        let path = command.arg(index).unwrap();
        let fd = vfs::open(path, vfs::O_RDONLY)?;
        let mut buffer = [0u8; ramfs::DATA_MAX];

        loop {
            let len = match vfs::read(fd, &mut buffer) {
                Ok(len) => len,
                Err(reason) => {
                    let _ = vfs::close(fd);
                    return Err(reason);
                }
            };
            if len == 0 {
                break;
            }

            if !number {
                tty::write_bytes(&buffer[..len]);
                continue;
            }

            let mut offset = 0;
            for (position, &byte) in buffer[..len].iter().enumerate() {
                if line_start {
                    crate::print!("{:>6}\t", line_number);
                    line_number += 1;
                    line_start = false;
                }
                if byte == b'\n' {
                    tty::write_bytes(&buffer[offset..=position]);
                    offset = position + 1;
                    line_start = true;
                }
            }
            if offset < len {
                tty::write_bytes(&buffer[offset..len]);
            }
        }

        let _ = vfs::close(fd);
    }
    Ok(())
}

fn applet_mkdir(command: &Command<'_>) -> Result<(), &'static str> {
    if command.len() < 2 {
        return Err("uso: mkdir <diretorio>...");
    }

    let mut fs = ramfs::lock();
    for index in 1..command.len() {
        fs.create(command.arg(index).unwrap(), true)?;
    }
    Ok(())
}

fn applet_touch(command: &Command<'_>) -> Result<(), &'static str> {
    if command.len() < 2 {
        return Err("uso: touch <arquivo>...");
    }

    let mut fs = ramfs::lock();
    for index in 1..command.len() {
        let path = command.arg(index).unwrap();
        match fs.resolve(path) {
            Ok(_) => {}
            Err("arquivo nao encontrado") => {
                fs.create(path, false)?;
            }
            Err(reason) => return Err(reason),
        }
    }
    Ok(())
}

fn applet_write(command: &Command<'_>) -> Result<(), &'static str> {
    let mut append = false;
    let mut start = 1;
    if command.arg(1) == Some(b"-a") {
        append = true;
        start = 2;
    }
    if command.len() <= start {
        return Err("uso: write [-a] <arquivo> <texto...>");
    }

    let path = command.arg(start).unwrap();
    let mut buffer = [0u8; LINE_MAX + 2];
    let mut len = 0;

    for index in (start + 1)..command.len() {
        let argument = command.arg(index).unwrap();
        if len > 0 {
            buffer[len] = b' ';
            len += 1;
        }
        if len + argument.len() + 1 > buffer.len() {
            return Err("texto longo");
        }
        buffer[len..len + argument.len()].copy_from_slice(argument);
        len += argument.len();
    }
    buffer[len] = b'\n';
    len += 1;

    let mut fs = ramfs::lock();
    fs.write(path, &buffer[..len], append)?;
    Ok(())
}

fn applet_rm(command: &Command<'_>) -> Result<(), &'static str> {
    let mut recursive = false;
    let mut start = 1;
    if command.arg(1) == Some(b"-r") {
        recursive = true;
        start = 2;
    }
    if command.len() <= start {
        return Err("uso: rm [-r] <caminho>...");
    }

    let mut fs = ramfs::lock();
    for index in start..command.len() {
        fs.remove(command.arg(index).unwrap(), recursive)?;
    }
    Ok(())
}

fn applet_rmdir(command: &Command<'_>) -> Result<(), &'static str> {
    if command.len() < 2 {
        return Err("uso: rmdir <diretorio>...");
    }

    let mut fs = ramfs::lock();
    for index in 1..command.len() {
        let path = command.arg(index).unwrap();
        let node = fs.resolve(path)?;
        if !fs.node(node).is_dir() {
            return Err("nao e um diretorio");
        }
        fs.remove(path, false)?;
    }
    Ok(())
}

fn applet_cp(command: &Command<'_>) -> Result<(), &'static str> {
    if command.len() != 3 {
        return Err("uso: cp <origem> <destino>");
    }

    let source_path = command.arg(1).unwrap();
    let dest_path = command.arg(2).unwrap();

    let mut data = [0u8; ramfs::DATA_MAX];
    let len = {
        let fs = ramfs::lock();
        fs.read(source_path, &mut data)?
    };

    let mut target = [0u8; LINE_MAX + 32];
    let target_len = {
        let fs = ramfs::lock();
        let source = fs.resolve(source_path)?;
        let name = fs.node(source).name();
        match fs.resolve(dest_path) {
            Ok(dest) if fs.node(dest).is_dir() => {
                build_child_path(dest_path, name, &mut target)?
            }
            _ => store_path(dest_path, &mut target)?,
        }
    };

    let mut fs = ramfs::lock();
    fs.write(&target[..target_len], &data[..len], false)?;
    Ok(())
}

fn applet_mv(command: &Command<'_>) -> Result<(), &'static str> {
    if command.len() != 3 {
        return Err("uso: mv <origem> <destino>");
    }

    let from = command.arg(1).unwrap();
    let to = command.arg(2).unwrap();

    let mut target = [0u8; LINE_MAX + 32];
    let target_len = {
        let fs = ramfs::lock();
        let source = fs.resolve(from)?;
        let name = fs.node(source).name();
        match fs.resolve(to) {
            Ok(dest) if fs.node(dest).is_dir() => build_child_path(to, name, &mut target)?,
            _ => store_path(to, &mut target)?,
        }
    };

    let mut fs = ramfs::lock();
    fs.rename(from, &target[..target_len])
}

fn build_child_path(dir: &[u8], name: &[u8], out: &mut [u8]) -> Result<usize, &'static str> {
    if dir.is_empty() || name.is_empty() {
        return Err("caminho vazio");
    }
    let slash = if dir.last() == Some(&b'/') { 0 } else { 1 };
    let total = dir.len() + slash + name.len();
    if total > out.len() {
        return Err("caminho longo");
    }

    out[..dir.len()].copy_from_slice(dir);
    let mut len = dir.len();
    if slash == 1 {
        out[len] = b'/';
        len += 1;
    }
    out[len..len + name.len()].copy_from_slice(name);
    Ok(len + name.len())
}

fn store_path(path: &[u8], out: &mut [u8]) -> Result<usize, &'static str> {
    if path.is_empty() {
        return Err("caminho vazio");
    }
    if path.len() > out.len() {
        return Err("caminho longo");
    }
    out[..path.len()].copy_from_slice(path);
    Ok(path.len())
}

fn applet_tree(command: &Command<'_>) -> Result<(), &'static str> {
    if command.len() > 2 {
        return Err("uso: tree [diretorio]");
    }

    let path = command.arg(1).unwrap_or(b".");
    let fs = ramfs::lock();
    let root = fs.resolve(path)?;
    if !fs.node(root).is_dir() {
        return Err("nao e um diretorio");
    }

    let node = fs.node(root);
    if root == 0 {
        println!("/");
    } else {
        tty::write_bytes(node.name());
        println!();
    }

    let mut prefix = [0u8; ramfs::MAX_NODES * 4];
    tree_children(&fs, root, &mut prefix, 0);
    Ok(())
}

fn tree_children(fs: &ramfs::Fs, dir: usize, prefix: &mut [u8], prefix_len: usize) {
    let mut child = fs.node(dir).first_child();
    while child != ramfs::NO_NODE {
        let node = fs.node(child);
        let last = node.next_sibling() == ramfs::NO_NODE;

        tty::write_bytes(&prefix[..prefix_len]);
        tty::write_bytes(if last { b"`-- " } else { b"|-- " });
        tty::write_bytes(node.name());
        if node.is_dir() {
            tty::write_bytes(b"/");
        }
        println!();

        if node.is_dir() {
            let extension: &[u8] = if last { b"    " } else { b"|   " };
            let new_len = prefix_len + extension.len();
            if new_len <= prefix.len() {
                prefix[prefix_len..new_len].copy_from_slice(extension);
                tree_children(fs, child, prefix, new_len);
            }
        }

        child = node.next_sibling();
    }
}

fn applet_uname(command: &Command<'_>) -> Result<(), &'static str> {
    if command.len() == 1 || command.arg(1) == Some(b"-a") {
        if command.len() > 2 || (command.len() == 2 && command.arg(1) != Some(b"-a")) {
            return Err("uso: uname [-asnrm]");
        }
        println!("Linux rinux 0.1.0 x86_64");
        return Ok(());
    }

    let mut first = true;
    for index in 1..command.len() {
        let text = match command.arg(index).unwrap() {
            b"-s" => "Linux",
            b"-n" => "rinux",
            b"-r" => "0.1.0",
            b"-m" => "x86_64",
            _ => return Err("uso: uname [-asnrm]"),
        };
        if !first {
            crate::print!(" ");
        }
        crate::print!("{}", text);
        first = false;
    }
    println!();
    Ok(())
}

fn applet_uptime(command: &Command<'_>) -> Result<(), &'static str> {
    no_args(command)?;
    let ticks = idt::ticks();
    println!(
        "up {}.{:02}s ({} ticks a ~100 Hz)",
        ticks / 100,
        ticks % 100,
        ticks
    );
    Ok(())
}

fn applet_free(command: &Command<'_>) -> Result<(), &'static str> {
    no_args(command)?;
    let stats = paging::stats();
    let total = stats.allocated + stats.free;
    println!("              total        used        free");
    println!(
        "Mem:     {:>8} KiB {:>8} KiB {:>8} KiB",
        total * 4,
        stats.allocated * 4,
        stats.free * 4
    );
    println!("Page faults: {}", stats.faults);
    Ok(())
}

fn applet_heap(command: &Command<'_>) -> Result<(), &'static str> {
    no_args(command)?;

    let mut values: alloc::vec::Vec<u64> = alloc::vec::Vec::new();
    for index in 0..32 {
        values.push((index * index) as u64);
    }

    let mut text = alloc::string::String::new();
    text.push_str("rinux heap");
    let sum: u64 = values.iter().sum();

    let stats = heap::stats();
    println!(
        "heap: {} itens, soma {}, texto \"{}\"",
        values.len(),
        sum,
        text
    );
    println!(
        "  mapeado: {} KiB, usado: {} B, alocacoes: {}",
        stats.mapped / 1024,
        stats.used,
        stats.allocs
    );
    Ok(())
}

fn applet_vfs(command: &Command<'_>) -> Result<(), &'static str> {
    no_args(command)?;

    let fd = vfs::open(b"/etc/motd", vfs::O_RDONLY)?;
    let mut buffer = [0u8; ramfs::DATA_MAX];
    let read = vfs::read(fd, &mut buffer)?;
    vfs::close(fd)?;

    crate::print!("vfs: li {} bytes de /etc/motd: ", read);
    tty::write_bytes(&buffer[..read]);

    let fd = vfs::open(
        b"/vfs.txt",
        vfs::O_CREAT | vfs::O_TRUNC | vfs::O_RDWR,
    )?;
    let written = vfs::write(fd, b"arquivo criado via VFS\n")?;
    vfs::lseek(fd, 0, vfs::SEEK_SET)?;
    let read = vfs::read(fd, &mut buffer)?;
    vfs::close(fd)?;

    crate::print!("vfs: {} bytes escritos e relidos: ", written);
    tty::write_bytes(&buffer[..read]);

    let fd = vfs::open(b"/proc/version", vfs::O_RDONLY)?;
    let read = vfs::read(fd, &mut buffer)?;
    vfs::close(fd)?;
    crate::print!("vfs: /proc/version: ");
    tty::write_bytes(&buffer[..read]);

    println!("vfs: descritores abertos agora: {}", vfs::open_count());
    Ok(())
}

fn applet_whoami(command: &Command<'_>) -> Result<(), &'static str> {
    no_args(command)?;
    println!("root");
    Ok(())
}

fn applet_hostname(command: &Command<'_>) -> Result<(), &'static str> {
    no_args(command)?;
    println!("rinux");
    Ok(())
}

fn applet_info(command: &Command<'_>) -> Result<(), &'static str> {
    no_args(command)?;

    let (nodes, bytes) = {
        let fs = ramfs::lock();
        fs.usage()
    };

    println!("Kernel: Rust, no_std, x86_64");
    println!("Boot:   Multiboot2 (BIOS) -> modo longo");
    println!("Video:  VGA texto 80x25, buffer 0xB8000");
    println!(
        "TTY:    {} consoles virtuais, ativo = tty{} (Alt+F1..Alt+F8)",
        tty::TTY_COUNT,
        tty::active()
    );
    println!("Entrada: teclado PS/2 e serial COM1 (polling)");
    println!("IDT:    256 portas; excecoes + IRQ0 (timer ~100 Hz)");
    println!("Memoria: paginas de 4 KiB, demand paging e frames fisicos");
    println!("RAMFS:  {} nos, {} bytes em arquivos", nodes, bytes);
    let heap_stats = heap::stats();
    println!(
        "HEAP:   {} KiB mapeados, {} B em uso",
        heap_stats.mapped / 1024,
        heap_stats.used
    );
    println!("Usuario: ring 3 com GDT/TSS, protecao U/S e int 0x80");
    let tasks = task::stats();
    println!(
        "Tarefas: {} na tabela, {} prontas, {} dormindo, {} paradas, {} encerradas (escalonador preemptivo)",
        tasks.total, tasks.ready, tasks.blocked, tasks.stopped, tasks.exited
    );
    println!("Ticks do timer: {}", idt::ticks());
    Ok(())
}

fn applet_tty(command: &Command<'_>) -> Result<(), &'static str> {
    match command.len() {
        1 => {
            println!(
                "console ativo: tty{} (disponiveis: tty0..tty{})",
                tty::active(),
                tty::TTY_COUNT - 1
            );
            Ok(())
        }
        2 => {
            let index = parse_console(command.arg(1).unwrap()).ok_or("uso: tty [0..7]")?;
            if index >= tty::TTY_COUNT {
                return Err("uso: tty [0..7]");
            }
            tty::switch(index);
            Ok(())
        }
        _ => Err("uso: tty [0..7]"),
    }
}

fn applet_getty(command: &Command<'_>) -> Result<(), &'static str> {
    match command.len() {
        1 => {
            getty_demo(None);
            Ok(())
        }
        2 => {
            let index = parse_console(command.arg(1).unwrap()).ok_or("uso: getty [0..7]")?;
            if index >= tty::TTY_COUNT {
                return Err("uso: getty [0..7]");
            }
            getty_demo(Some(index));
            Ok(())
        }
        _ => Err("uso: getty [0..7]"),
    }
}

fn applet_demand(command: &Command<'_>) -> Result<(), &'static str> {
    no_args(command)?;
    demand_demo();
    Ok(())
}

fn applet_usertest(command: &Command<'_>) -> Result<(), &'static str> {
    no_args(command)?;
    user_demo(false);
    Ok(())
}

fn applet_filetest(command: &Command<'_>) -> Result<(), &'static str> {
    no_args(command)?;
    match user::run_file() {
        Ok(0) => println!("[USER] filetest concluido"),
        Ok(status) => println!("[USER] filetest encerrado com status {}", status),
        Err(reason) => println!("[USER] filetest: {}", reason),
    }
    Ok(())
}

fn applet_userprot(command: &Command<'_>) -> Result<(), &'static str> {
    no_args(command)?;
    user_demo(true);
    Ok(())
}

/// Tarefa de kernel usada pelo applet `spawn`. Ha um yield inicial, mas o
/// laco seguinte nao cede a CPU: cada passo consome 10 ticks reais de CPU
/// (`cpu_ticks`), entao tarefas com `nice` diferentes levam tempos de parede
/// diferentes para concluir o mesmo trabalho.
fn worker_task() {
    let pid = task::current_pid();
    println!("[task {}] inicio (kernel thread, ppid={})", pid, task::current_ppid());
    task::yield_now();
    for round in 1..=3 {
        println!("[task {}] passo {}/3", pid, round);
        let start = task::current_cpu_ticks();
        while task::current_cpu_ticks().wrapping_sub(start) < 10 {
            // Ponto seguro: permite que `kill -USR1` execute o handler mesmo
            // em uma tarefa que ainda nao cedeu a CPU.
            task::check_signals();
            core::hint::spin_loop();
        }
    }
    println!("[task {}] fim", pid);
}

/// Variante de `worker_task` que dorme de verdade entre os passos. Diferente
/// do *busy-wait*, a tarefa sai da CPU (`Blocked`) e so volta quando o prazo
/// do escalonador vence, deixando o shell livre para responder.
fn sleeper_task() {
    let pid = task::current_pid();
    println!("[task {}] inicio (dorminhoco, ppid={})", pid, task::current_ppid());
    for round in 1..=3 {
        println!("[task {}] dormindo {}/3", pid, round);
        task::sleep(30);
        println!("[task {}] acordou {}/3", pid, round);
    }
    println!("[task {}] fim", pid);
}

/// Worker que dorme em uma fila de espera ate o applet `notify` sinalizar.
/// Equivale a `wait_event` sobre uma condicao compartilhada.
fn waiter_task() {
    let pid = task::current_pid();
    let seen = EVENT_COUNT.load(Ordering::Acquire);
    println!("[task {}] esperando evento (wait_event)", pid);
    EVENT_QUEUE.wait_event(|| EVENT_COUNT.load(Ordering::Acquire) != seen);
    println!("[task {}] evento recebido", pid);
}

/// Worker que espera por evento com prazo, como `wait_event_timeout`.
fn waiter_timeout_task() {
    let pid = task::current_pid();
    let seen = EVENT_COUNT.load(Ordering::Acquire);
    println!(
        "[task {}] esperando evento por 1 s (wait_event_timeout)",
        pid
    );
    if EVENT_QUEUE.wait_event_timeout(100, || EVENT_COUNT.load(Ordering::Acquire) != seen) {
        println!("[task {}] evento recebido antes do timeout", pid);
    } else {
        println!("[task {}] timeout: nenhum evento em 1 s", pid);
    }
}

/// Handler de demonstracao instalado por `sigaction` e por
/// `spawn ... signal`. Roda no contexto da propria tarefa em um ponto seguro.
fn demo_signal_handler(sig: i32) {
    println!(
        "[task {}] handler: recebi {} ({})",
        task::current_pid(),
        task::signal_name(sig as u8),
        sig
    );
}

/// Tarefa que instala um handler para `SIGUSR1` e dorme ate ser sinalizada.
/// `kill <pid> USR1` acorda a tarefa; `SIGTERM`/`SIGKILL` a encerram.
fn signal_task() {
    let pid = task::current_pid();
    match task::set_signal_action(
        pid,
        task::SIGUSR1,
        task::SignalAction::Handler(demo_signal_handler),
    ) {
        Ok(_) => println!(
            "[task {}] esperando {} (kill {} USR1)",
            pid,
            task::signal_name(task::SIGUSR1),
            pid
        ),
        Err(reason) => {
            println!("[task {}] sigaction: {}", pid, reason);
            return;
        }
    }

    loop {
        task::sleep(1000);
    }
}

/// Tarefa de vida curta que termina com status 42, deixando um zumbi para o
/// applet `wait` colher e demonstrar `SIGCHLD`/status de saida.
fn exit_task() {
    let pid = task::current_pid();
    println!("[task {}] encerrando com status 42 em 0,5 s", pid);
    task::sleep(50);
    task::exit_current_with_status(42)
}

fn applet_ps(command: &Command<'_>) -> Result<(), &'static str> {
    no_args(command)?;
    println!("  PID  PPID  ESTADO      NICE  CPU  VRUNTIME   YIELDS  PREEMP  NOME");
    task::for_each(|info| {
        let marker = if info.current { '*' } else { ' ' };
        println!(
            "{}{:>5} {:>5}  {:<11} {:>4} {:>4}  {:>9}  {:>6}  {:>6}  {}",
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

fn applet_spawn(command: &Command<'_>) -> Result<(), &'static str> {
    const USAGE: &str =
        "uso: spawn [nome] [n] [cpu|sleep|wait|timeout|signal|exit] [nice]";
    if command.len() > 5 {
        return Err(USAGE);
    }
    let name = command.arg(1).unwrap_or(b"worker");
    let count = match command.arg(2) {
        Some(arg) => parse_decimal(arg).ok_or(USAGE)?.clamp(1, 8),
        None => 1,
    };
    // Terceiro argumento opcional escolhe a entrada da tarefa: sem ele (ou com
    // "cpu"), o worker preemptivo de sempre; "sleep" bloqueia por prazo;
    // "wait" e "timeout" bloqueiam em uma fila de espera acordada por
    // `notify`; "signal" instala um handler de SIGUSR1; "exit" encerra com
    // status para demonstrar `wait`.
    let entry: fn() = match command.arg(3) {
        None => worker_task,
        Some(mode) if mode == b"cpu" => worker_task,
        Some(mode) if mode == b"sleep" => sleeper_task,
        Some(mode) if mode == b"wait" => waiter_task,
        Some(mode) if mode == b"timeout" => waiter_timeout_task,
        Some(mode) if mode == b"signal" => signal_task,
        Some(mode) if mode == b"exit" => exit_task,
        Some(_) => return Err(USAGE),
    };
    // Quarto argumento opcional: prioridade `nice` (-20..19).
    let nice = match command.arg(4) {
        Some(arg) => parse_signed_decimal(arg).ok_or(USAGE)?.clamp(
            task::NICE_MIN as i32,
            task::NICE_MAX as i32,
        ) as i8,
        None => 0,
    };

    for _ in 0..count {
        let pid = task::spawn(name, entry, nice)?;
        println!(
            "tarefa criada: pid={} nome={} nice={}",
            pid,
            core::str::from_utf8(name).unwrap_or("?"),
            nice
        );
    }
    Ok(())
}

/// Bloqueia o shell por `segundos` (1 tick = 10 ms a 100 Hz), demonstrando o
/// caminho de `schedule_timeout`: durante a espera a tarefa nao consome CPU e
/// o timer continua contando.
fn applet_sleep(command: &Command<'_>) -> Result<(), &'static str> {
    if command.len() > 2 {
        return Err("uso: sleep [segundos]");
    }
    let seconds = match command.arg(1) {
        Some(arg) => parse_decimal(arg).ok_or("uso: sleep [segundos]")?.clamp(1, 60),
        None => 1,
    };

    let start = idt::ticks();
    task::sleep(seconds as u64 * 100);
    let elapsed = idt::ticks().wrapping_sub(start);
    println!(
        "dormiu {} ticks (~{} ms, pedido: {} s)",
        elapsed,
        elapsed * 10,
        seconds
    );
    Ok(())
}

/// Sinaliza o evento global e acorda as tarefas bloqueadas na fila de espera.
/// `notify one` usa `wake_up` (uma tarefa); sem argumento usa `wake_up_all`.
fn applet_notify(command: &Command<'_>) -> Result<(), &'static str> {
    const USAGE: &str = "uso: notify [one]";
    if command.len() > 2 {
        return Err(USAGE);
    }
    let only_one = match command.arg(1) {
        None => false,
        Some(arg) if arg == b"one" => true,
        Some(_) => return Err(USAGE),
    };

    let waiting = EVENT_QUEUE.waiters();
    let count = EVENT_COUNT.fetch_add(1, Ordering::AcqRel) + 1;
    let woken = if only_one {
        EVENT_QUEUE.wake_up_one()
    } else {
        EVENT_QUEUE.wake_up_all()
    };
    println!(
        "evento #{} sinalizado; {} esperando, {} acordada(s) ({})",
        count,
        waiting,
        woken,
        if only_one { "wake_up" } else { "wake_up_all" }
    );
    Ok(())
}

fn parse_decimal(arg: &[u8]) -> Option<usize> {
    if arg.is_empty() {
        return None;
    }
    let mut value = 0usize;
    for &byte in arg {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add((byte - b'0') as usize)?;
    }
    Some(value)
}

/// Interpreta um inteiro decimal com sinal opcional (`-20`, `+5`, `10`).
fn parse_signed_decimal(arg: &[u8]) -> Option<i32> {
    let (negative, digits) = match arg.first()? {
        b'-' => (true, &arg[1..]),
        b'+' => (false, &arg[1..]),
        _ => (false, arg),
    };
    if digits.is_empty() {
        return None;
    }
    let mut value = 0i32;
    for &byte in digits {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value
            .checked_mul(10)?
            .checked_add((byte - b'0') as i32)?;
    }
    Some(if negative { -value } else { value })
}

/// `nice` sem argumentos mostra a prioridade do shell; `nice pid` consulta uma
/// tarefa; `nice pid valor` ajusta (como `setpriority`).
fn applet_nice(command: &Command<'_>) -> Result<(), &'static str> {
    const USAGE: &str = "uso: nice [pid] [valor]";
    if command.len() > 3 {
        return Err(USAGE);
    }

    if command.len() == 1 {
        let pid = task::current_pid();
        println!("nice: {} (pid={})", task::current_nice(), pid);
        return Ok(());
    }

    let pid = parse_decimal(command.arg(1).unwrap()).ok_or(USAGE)? as u64;
    let current = task::get_nice(pid).ok_or("tarefa nao encontrada")?;
    if command.len() == 2 {
        println!("pid={} nice={}", pid, current);
        return Ok(());
    }

    let requested = parse_signed_decimal(command.arg(2).unwrap()).ok_or(USAGE)?;
    let applied = task::set_nice(
        pid,
        requested.clamp(task::NICE_MIN as i32, task::NICE_MAX as i32) as i8,
    )?;
    println!("pid={} nice {} -> {}", pid, current, applied);
    Ok(())
}

fn applet_kill(command: &Command<'_>) -> Result<(), &'static str> {
    const USAGE: &str = "uso: kill <pid> [sinal]";
    if command.len() < 2 || command.len() > 3 {
        return Err(USAGE);
    }
    let pid = parse_decimal(command.arg(1).unwrap()).ok_or(USAGE)? as u64;
    let sig = match command.arg(2) {
        Some(arg) => task::parse_signal(arg).ok_or("sinal invalido")?,
        None => task::SIGTERM,
    };

    task::kill(pid, sig)?;
    if sig == 0 {
        println!("pid={} existe", pid);
    } else {
        println!(
            "pid={} sinal={} ({}) enviado",
            pid,
            sig,
            task::signal_name(sig)
        );
    }
    Ok(())
}

/// Imprime uma mascara de sinais como nomes, no estilo `/proc/<pid>/status`.
fn print_signal_mask(label: &str, mask: u32) {
    crate::print!("{}:", label);
    let mut any = false;
    for sig in 1..=31u8 {
        if mask & (1u32 << (sig - 1)) != 0 {
            crate::print!(" {}", task::signal_name(sig));
            any = true;
        }
    }
    if !any {
        crate::print!(" (vazio)");
    }
    println!();
}

fn applet_sig(command: &Command<'_>) -> Result<(), &'static str> {
    const USAGE: &str = "uso: sig [pid]";
    if command.len() > 2 {
        return Err(USAGE);
    }

    if let Some(arg) = command.arg(1) {
        let pid = parse_decimal(arg).ok_or(USAGE)? as u64;
        let mut found = false;
        task::for_each(|info| {
            if info.pid != pid || found {
                return;
            }
            found = true;
            println!(
                "pid={} ppid={} nome={} estado={}",
                info.pid,
                info.ppid,
                info.name,
                info.state.as_str()
            );
            print_signal_mask("pendentes", info.pending);
            print_signal_mask("bloqueados", info.blocked);
            if info.state == task::State::Exited {
                if info.exit_signal != 0 {
                    println!(
                        "terminado por {} ({}), status={}",
                        task::signal_name(info.exit_signal),
                        info.exit_signal,
                        info.exit_status
                    );
                } else {
                    println!("status de saida={}", info.exit_status);
                }
            }
        });
        if !found {
            return Err("tarefa nao encontrada");
        }
        return Ok(());
    }

    println!("  PID  NOME        PENDENTES  BLOQUEADOS  SITUACAO");
    task::for_each(|info| {
        crate::print!(
            "{:>5}  {:<10}  {:#08x}  {:#08x}",
            info.pid, info.name, info.pending, info.blocked
        );
        if info.state == task::State::Exited {
            if info.exit_signal != 0 {
                println!(
                    "  sinal {} status {}",
                    task::signal_name(info.exit_signal),
                    info.exit_status
                );
            } else {
                println!("  status {}", info.exit_status);
            }
        } else {
            println!("  {}", info.state.as_str());
        }
    });
    Ok(())
}

fn describe_signal_action(action: task::SignalAction) -> &'static str {
    match action {
        task::SignalAction::Default => "default",
        task::SignalAction::Ignore => "ignore",
        task::SignalAction::Handler(_) => "handler",
    }
}

fn applet_sigaction(command: &Command<'_>) -> Result<(), &'static str> {
    const USAGE: &str = "uso: sigaction <pid> <sinal> [default|ignore|handler]";
    if command.len() < 3 || command.len() > 4 {
        return Err(USAGE);
    }

    let pid = parse_decimal(command.arg(1).unwrap()).ok_or(USAGE)? as u64;
    let sig = task::parse_signal(command.arg(2).unwrap()).ok_or("sinal invalido")?;
    if sig == 0 {
        return Err("sinal invalido");
    }

    if command.len() == 3 {
        let action = task::signal_action(pid, sig).ok_or("tarefa nao encontrada")?;
        println!(
            "pid={} sinal={} ({}) acao={}",
            pid,
            sig,
            task::signal_name(sig),
            describe_signal_action(action)
        );
        return Ok(());
    }

    let action = match command.arg(3).unwrap() {
        b"default" => task::SignalAction::Default,
        b"ignore" => task::SignalAction::Ignore,
        b"handler" => task::SignalAction::Handler(demo_signal_handler),
        _ => return Err(USAGE),
    };
    let old = task::set_signal_action(pid, sig, action)?;
    println!(
        "pid={} sinal={} ({}) acao {} -> {}",
        pid,
        sig,
        task::signal_name(sig),
        describe_signal_action(old),
        describe_signal_action(action)
    );
    Ok(())
}

fn applet_sigblock(command: &Command<'_>) -> Result<(), &'static str> {
    const USAGE: &str = "uso: sigblock <pid> <sinal> <on|off>";
    if command.len() != 4 {
        return Err(USAGE);
    }

    let pid = parse_decimal(command.arg(1).unwrap()).ok_or(USAGE)? as u64;
    let sig = task::parse_signal(command.arg(2).unwrap()).ok_or("sinal invalido")?;
    if sig == 0 {
        return Err("sinal invalido");
    }
    let block = match command.arg(3).unwrap() {
        b"on" => true,
        b"off" => false,
        _ => return Err(USAGE),
    };

    let mask = task::set_signal_blocked(pid, sig, block)?;
    println!(
        "pid={} sinal={} ({}) {}; mascara={:#010x}",
        pid,
        sig,
        task::signal_name(sig),
        if block { "bloqueado" } else { "desbloqueado" },
        mask
    );
    Ok(())
}

fn applet_wait(command: &Command<'_>) -> Result<(), &'static str> {
    const USAGE: &str = "uso: wait [pid]";
    if command.len() > 2 {
        return Err(USAGE);
    }

    let target = match command.arg(1) {
        Some(arg) => Some(parse_decimal(arg).ok_or(USAGE)? as u64),
        None => None,
    };
    let result = task::wait_child(target)?;
    if result.signal != 0 {
        println!(
            "pid={} terminado por {} ({}), status={}",
            result.pid,
            task::signal_name(result.signal),
            result.signal,
            result.status
        );
    } else {
        println!("pid={} status={}", result.pid, result.status);
    }
    Ok(())
}

fn applet_mem(command: &Command<'_>) -> Result<(), &'static str> {
    no_args(command)?;
    memory_info();
    Ok(())
}

fn applet_reboot(command: &Command<'_>) -> Result<(), &'static str> {
    no_args(command)?;
    println!("Reiniciando...");
    arch::reboot()
}

fn applet_halt(command: &Command<'_>) -> Result<(), &'static str> {
    no_args(command)?;
    println!("CPU parada. Use reset ou feche o QEMU.");
    arch::halt_loop()
}

fn parse_console(arg: &[u8]) -> Option<usize> {
    if arg.len() == 1 && arg[0].is_ascii_digit() {
        Some((arg[0] - b'0') as usize)
    } else {
        None
    }
}

fn getty_demo(console: Option<usize>) {
    let target = console.unwrap_or_else(tty::active);
    println!("[GETTY] iniciando login em tty{}...", target);

    match user::run_getty(console) {
        Ok(0) => println!("[GETTY] login concluido, voltando ao shell"),
        Ok(status) => println!("[GETTY] encerrado com status {}", status),
        Err(reason) => println!("[GETTY] nao foi possivel executar: {}", reason),
    }
}

fn demand_demo() {
    let address = paging::kernel_demand_addr(0);
    let before = paging::demand_faults();

    // A pagina ainda nao esta presente: a escrita gera #PF. O tratador aloca
    // um frame, mapeia com U/S = 0 e a CPU repete a instrucao.
    unsafe {
        core::ptr::write_volatile(address as *mut u64, 0xDEAD_BEEF_CAFE_1234);
        let value = core::ptr::read_volatile(address as *const u64);
        println!(
            "[DEMAND] {:#018x} = {:#018x} (page faults: {})",
            address,
            value,
            paging::demand_faults() - before
        );
    }
}

fn user_demo(protection: bool) {
    let before = paging::demand_faults();
    let demo = if protection {
        user::Demo::Protection
    } else {
        user::Demo::Demand
    };

    println!(
        "[USER] entrando em ring 3 ({})...",
        if protection {
            "teste de protecao"
        } else {
            "demand paging"
        }
    );

    match user::run(demo) {
        Ok(0) => println!(
            "[USER] voltou ao kernel; page faults nesta execucao: {}",
            paging::demand_faults() - before
        ),
        Ok(status) => println!("[USER] programa encerrado com status {}", status),
        Err(reason) => println!("[USER] nao foi possivel executar: {}", reason),
    }
}

fn memory_info() {
    let stats = paging::stats();
    println!("Frames fisicos (4 KiB):");
    println!("  alocados : {}", stats.allocated);
    println!("  livres   : {}", stats.free);
    println!("  page faults atendidos: {}", stats.faults);
    println!("  ticks do timer       : {}", idt::ticks());

    let heap_stats = heap::stats();
    println!("Heap do kernel:");
    println!("  mapeado : {} KiB", heap_stats.mapped / 1024);
    println!("  em uso  : {} B", heap_stats.used);
    println!("  alocacoes: {}", heap_stats.allocs);

    let tasks = task::stats();
    println!("Escalonador de tarefas:");
    println!("  na tabela: {}", tasks.total);
    println!("  prontas  : {}", tasks.ready);
    println!("  dormindo : {}", tasks.blocked);
    println!("  paradas  : {}", tasks.stopped);
    println!("  encerradas: {}", tasks.exited);
    println!("  criadas  : {}", tasks.spawned);
    println!("  preempcoes: {}", tasks.preemptions);

    let user = paging::page_flags(paging::USER_CODE);
    let kernel = paging::page_flags(paging::KERNEL_VMA);
    match user {
        Some(flags) => println!("  USER_CODE  flags={:#x} (U/S={})", flags, (flags >> 2) & 1),
        None => println!("  USER_CODE  ainda nao mapeada (rode 'usertest')"),
    }
    match kernel {
        Some(flags) => println!("  KERNEL_VMA flags={:#x} (U/S={})", flags, (flags >> 2) & 1),
        None => println!("  KERNEL_VMA sem mapeamento?"),
    }
}
