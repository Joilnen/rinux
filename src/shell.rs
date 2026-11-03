//! Shell minimo no estilo BusyBox, com applets em Rust.
//!
//! O shell le uma linha do teclado PS/2 ou da serial (COM1), separa os
//! argumentos (com suporte a aspas simples/duplas e `\`) e despacha para uma
//! tabela de applets. Os comandos de arquivo usam o ramfs de `ramfs.rs`.
//!
//! Nao ha pipes, redirecionamento, variaveis de ambiente, `fork` ou `exec`.
//! Tudo roda no kernel, de forma sincrona, sem heap e sem escalonador.

use crate::{arch, idt, paging, println, ramfs, tty, user};

const LINE_MAX: usize = 128;
const MAX_ARGS: usize = 24;

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
                    // Sem interrupcoes, apenas cede a CPU por um instante.
                    core::hint::spin_loop();
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

    let fs = ramfs::lock();
    let index = fs.resolve(path)?;
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
    for index in start..command.len() {
        let path = command.arg(index).unwrap();
        let mut buffer = [0u8; ramfs::DATA_MAX];
        let len = {
            let fs = ramfs::lock();
            fs.read(path, &mut buffer)?
        };

        if !number {
            tty::write_bytes(&buffer[..len]);
            continue;
        }

        let mut offset = 0;
        let mut line_start = true;
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
    println!("Usuario: ring 3 com GDT/TSS, protecao U/S e int 0x80");
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

fn applet_userprot(command: &Command<'_>) -> Result<(), &'static str> {
    no_args(command)?;
    user_demo(true);
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
