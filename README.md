# Rust VGA Kernel

Kernel mínimo escrito em Rust (`#![no_std]`) que inicializa um PC padrão em
BIOS/x86_64, entra em modo longo e abre um terminal em **modo texto VGA 80x25**
com oito consoles virtuais (`tty0`..`tty7`), teclado PS/2, syscalls via
`int 0x80` e um `getty`/login executando em ring 3.

Este diretório é uma versão didática e reduzida dos módulos do Linux
necessários para "sair do bootloader e abrir um console VGA". Não é um porte
do Linux: é um kernel independente, sem dependências externas de Rust, que
reimplementa o caminho mínimo de boot, console e teclado.

## O que funciona

- Boot BIOS via GRUB/multiboot2.
- Transição 32 bits -> modo longo (x86_64).
- Paginação com identidade dos primeiros 1 GiB e mapeamento da metade alta.
- IDT com 256 portas: exceções, IRQs do PIC remapeadas e `int 0x80`.
- PIC remapeado para `0x20..0x2F` e timer PIT em ~100 Hz (IRQ0).
- Alocador de frames físicos de 4 KiB baseado no mapa de memória do Multiboot2.
- Demand paging: páginas de usuário e de kernel são materializadas no `#PF`.
- GDT com segmentos ring 3 e TSS; proteção usuário/kernel pelos bits U/S.
- Demos em ring 3 (`usertest`, `userprot`) com retorno ao kernel via `int 0x80`
  ou por exceção.
- Console VGA em modo texto (80x25, buffer em `0xB8000`), com rolagem,
  cursor de hardware e cores.
- Camada TTY com oito consoles virtuais (`tty0`..`tty7`), buffer e fila de
  entrada por console; troca com `Alt+F1`..`Alt+F8` ou com o comando `tty`.
- Interpretador ANSI/VT100 mínimo por console: `ESC[2J` limpa a tela,
  `ESC[H` leva o cursor ao início, `ESC[K` apaga a linha e `ESC[A/B/C/D`
  movem o cursor. O comando `clear` usa exatamente essas sequências.
- Teclado PS/2 (controlador 8042) por polling, com Shift, Alt, Backspace e
  detecção de `Alt+F1`..`Alt+F8`.
- Syscalls didáticas por `int 0x80`: `exit`, `write`, `read` e `get_tty`, com
  validação de ponteiros e materialização das páginas do usuário.
- `getty`/login em ring 3 (`root`/`rinux`), usando `read` com eco e `read` sem
  eco para a senha.
- Saída de depuração em COM1 (38400 8N1), espelhando o console ativo.
- Shell no estilo BusyBox, com parser de argumentos (aspas simples/duplas e
  `\`) e uma tabela estática de applets: `help`, `echo`, `clear`, `pwd`, `cd`,
  `ls`, `cat`, `mkdir`, `touch`, `write`, `rm`, `rmdir`, `cp`, `mv`, `tree`,
  `uname`, `uptime`, `free`, `whoami`, `hostname`, `info`, `tty`, `getty`,
  `demand`, `usertest`, `userprot`, `mem`, `reboot`, `halt`.
- RamFS didático em memória: 48 nós, nomes de até 31 bytes e arquivos de até
  1024 bytes, com criação, leitura, escrita, remoção e renomeação. Sem heap,
  permissões, timestamps ou persistência.
- Panic handler que imprime a mensagem no console e para a CPU.

Testado em QEMU 11.1.3 (SeaBIOS): boot, tela VGA, teclado e comandos funcionam.

## Estrutura

```
.
├── Cargo.toml
├── rust-toolchain.toml
├── .cargo/config.toml        # alvo x86_64-unknown-none + linker
├── linker.ld                 # stub em 1 MiB, kernel na metade alta
├── Makefile                  # build do ELF e da ISO
├── boot/grub/grub.cfg        # menu multiboot2, console VGA
└── src/
    ├── main.rs               # kernel_main, macros print!/println!, panic
    ├── boot.rs               # multiboot2, GDT inicial, paginação, modo longo
    ├── gdt.rs                # GDT definitiva, segmentos ring 3, TSS e IST
    ├── idt.rs                # IDT, stubs de exceção/IRQ, PIC, PIT e int 0x80
    ├── paging.rs             # frames, tabelas de 4 KiB, demand paging e U/S
    ├── user.rs               # ring 3, demos e getty/login
    ├── syscall.rs            # exit/write/read/get_tty via int 0x80
    ├── arch.rs               # inb/outb, halt, reboot, sti
    ├── vga.rs                # renderizador de baixo nível do VGA texto
    ├── tty.rs                # consoles virtuais tty0..tty7 e filas de entrada
    ├── keyboard.rs           # driver do teclado PS/2 (atkbd/i8042)
    ├── serial.rs             # UART 16550 / COM1
    ├── ramfs.rs              # sistema de arquivos em memória (sem heap)
    ├── shell.rs              # parser de linha e tabela de applets
    └── sync.rs               # spinlock do console e do ramfs
```

## Requisitos

- Rust com `rustup` (o `rust-toolchain.toml` já pede o alvo
  `x86_64-unknown-none`). O `scripts/rustc` garante que `cargo build` use o
  compilador do `rustup`; o `Makefile` também chama `rustup run stable cargo`
  quando o `rustup` existe. Isso evita usar o `cargo`/`rustc` do sistema, que
  em algumas distros não têm esse alvo instalado.
- `ld.lld` (LLVM linker).
- `grub-mkrescue` e `xorriso`/`libisoburn` para gerar a ISO.
- QEMU (opcional, para executar): `qemu-system-x86`.

No Arch Linux:

```sh
sudo pacman -S rustup lld grub libisoburn qemu-system-x86 mtools
rustup target add x86_64-unknown-none
```

## Compilar e executar

```sh
cd rinux
make            # gera target/x86_64-unknown-none/release/vga-kernel e vga-kernel.iso
make run        # abre o QEMU mostrando a tela VGA
```

Também é possível compilar diretamente com Cargo, sem `--target`:

```sh
cargo build              # ELF de depuração em target/x86_64-unknown-none/debug/
cargo build --release    # ELF otimizado usado na ISO
```

O `make run` usa:

```sh
qemu-system-x86_64 -cdrom vga-kernel.iso -boot order=d -m 128M -serial stdio
```

Para limpar:

```sh
make clean
```

Se aparecer `can't find crate for core` / `the x86_64-unknown-none target
may not be installed`, o `rustc` usado não é o do `rustup`. Confirme que o
alvo está instalado:

```sh
rustup target add x86_64-unknown-none
```

O projeto já contorna o `PATH` com `scripts/rustc` (usado pelo
`.cargo/config.toml`) e com o `Makefile`. Se ainda assim falhar, rode:

```sh
rustup run stable cargo build --release --target x86_64-unknown-none
```

Ou coloque `$HOME/.cargo/bin` antes de `/usr/bin` no `PATH`.

### Por que `-boot order=d`

Em algumas versões do SeaBIOS o CD não é tentado antes do disco. Por isso o
`make run` já usa `-boot order=d`. Se a máquina reclamar de "Boot failed",
confirme que a ISO está sendo escolhida como dispositivo de boot.

## Comandos do terminal

Os applets são digitados diretamente no prompt `rinux>`. A linha aceita aspas
simples (`'...'`), aspas duplas (`"..."`) e `\` para escapar um caractere.
Não há pipes (`|`), redirecionamento (`>`, `>>`, `<`) nem variáveis.

| Comando                         | Efeito                                      |
|---------------------------------|---------------------------------------------|
| `help [comando]`                | lista os applets ou mostra a ajuda de um    |
| `echo [-n] [texto...]`          | imprime o texto separado por espaços        |
| `clear`                         | limpa a tela com `ESC[2J` e `ESC[H`         |
| `pwd`                           | mostra o diretório corrente                 |
| `cd [diretório]`                | troca o diretório corrente (padrão: `/`)    |
| `ls [-l] [caminho]`             | lista arquivos e diretórios                 |
| `cat [-n] <arquivo>...`         | mostra o conteúdo de arquivos               |
| `mkdir <diretório>...`          | cria diretórios                             |
| `touch <arquivo>...`            | cria arquivos vazios                        |
| `write [-a] <arquivo> <texto...>` | grava ou anexa texto a um arquivo         |
| `rm [-r] <caminho>...`          | remove arquivos e diretórios                |
| `rmdir <diretório>...`          | remove diretórios vazios                    |
| `cp <origem> <destino>`         | copia um arquivo                            |
| `mv <origem> <destino>`         | move ou renomeia um arquivo/diretório       |
| `tree [diretório]`              | mostra a árvore do ramfs                    |
| `uname [-asnrm]`                | mostra informações do sistema               |
| `uptime`                        | tempo desde o boot e ticks do timer         |
| `free`                          | frames físicos livres e usados              |
| `whoami`                        | mostra o usuário da sessão (`root`)         |
| `hostname`                      | mostra o nome do host (`rinux`)             |
| `info`                          | informações do kernel, console e ramfs      |
| `tty`                           | mostra o console virtual ativo              |
| `tty [0..7]`                    | troca para o console virtual indicado       |
| `getty`                         | roda o login em ring 3 no console ativo     |
| `getty [0..7]`                  | troca de console e roda o login nele        |
| `demand`                        | testa demand paging na região do kernel     |
| `usertest`                      | executa demo em ring 3 com page fault       |
| `userprot`                      | tenta acessar o kernel em ring 3 (deve falhar) |
| `mem`                           | frames, page faults e bits U/S              |
| `reboot`                        | reinicia o PC (reset via 8042)              |
| `halt`                          | para a CPU                                  |

O ramfs começa com `/README`, `/etc/motd`, `/home/root/notes.txt` e os
diretórios `/etc`, `/home` e `/home/root`. O diretório corrente é global
porque ainda há um único laço de shell compartilhado por todos os consoles.

Credenciais do `getty` de demonstração: usuário `root`, senha `rinux`.

A troca de console também funciona pelo teclado com `Alt+F1`..`Alt+F8`
(`Alt+F1` seleciona `tty0`). Como a entrada serial é um único fluxo, ao rodar
`getty n` em um teste automatizado sem teclado, envie as credenciais somente
depois que o prompt de login do console `n` aparecer.

## Documentação

A documentação técnica completa (arquitetura, boot, memória, drivers, shell,
build, testes e comparação com o Linux) está em `docs/`. A versão em inglês é
a principal; a versão em português é mantida em paralelo.

- `docs/main.pdf` — documento compilado em inglês (principal);
- `docs/main.tex` — fonte LaTeX em inglês;
- `docs/sections/` — capítulos em inglês;
- `docs/pt/main.pdf` — documento compilado em português;
- `docs/pt/main.tex` — fonte LaTeX em português;
- `docs/pt/sections/` — capítulos em português.

Para regerar os PDFs:

```sh
cd docs
make        # inglês (principal)
make pt     # português
make both   # ambos
```

## Cadeia de boot

```
BIOS (SeaBIOS)
  └─ El Torito / CD
       └─ GRUB (i386-pc)
            └─ multiboot2 /boot/kernel.elf
                 └─ _start (32 bits, 1 MiB)
                      ├─ salva magic/info do multiboot2
                      ├─ monta PML4/PDPT/PD (identidade 0..1 GiB)
                      ├─ mapeia 0xFFFFFFFF80000000..0xFFFFFFFFC0000000
                      ├─ carrega GDT de 64 bits
                      ├─ ativa PAE, LME e paginação
                      └─ ljmp para o stub de 64 bits
                           └─ kernel_main() na metade alta
                                 ├─ vga::init()
                                 ├─ serial::init()
                                 ├─ tty::init()      (tty0..tty7)
                                 ├─ paging::init()   (frames do Multiboot2)
                                 ├─ ramfs::init()    (árvore inicial)
                                  ├─ gdt::init()      (ring 3 + TSS)
                                 ├─ keyboard::init()
                                 ├─ idt::init()      (PIC + PIT, IRQ0)
                                 ├─ sti
                                 └─ shell::run()
```

O cabeçalho multiboot2 fica em `.multiboot_header`, no início do arquivo,
como exige a especificação. O stub de 32 bits e as tabelas de páginas ficam
em 1 MiB (endereços baixos); o código Rust é ligado em
`0xFFFFFFFF80000000`, que é o modelo de código padrão do alvo
`x86_64-unknown-none`.

## Correspondência com o Linux

| Linux (7.x)                                      | Aqui                    |
|--------------------------------------------------|-------------------------|
| `arch/x86/boot/header.S`, `arch/x86/boot/pm.c`   | `src/boot.rs`           |
| `arch/x86/kernel/head_64.S`                      | `src/boot.rs`           |
| `arch/x86/kernel/cpu/common.c` (GDT/TSS)         | `src/gdt.rs`            |
| `arch/x86/kernel/idt.c`, `arch/x86/kernel/traps.c` | `src/idt.rs`          |
| `arch/x86/mm/fault.c` (demand paging/proteção)   | `src/paging.rs`         |
| `arch/x86/mm/init_64.c` (paginação)              | `src/boot.rs`, `src/paging.rs` |
| `kernel/entry/common.c` (retorno ao usuário)     | `src/user.rs`           |
| `drivers/tty/tty_io.c`, `drivers/tty/vt/vt.c`    | `src/tty.rs`            |
| `drivers/video/console/vgacon.c`                 | `src/vga.rs`            |
| `drivers/input/serio/i8042.c`                    | `src/keyboard.rs`       |
| `drivers/input/keyboard/atkbd.c`                 | `src/keyboard.rs`       |
| `drivers/tty/serial/8250/8250_port.c`            | `src/serial.rs`         |
| `fs/ramfs/inode.c` (ramfs didático)              | `src/ramfs.rs`          |
| `arch/x86/entry/syscalls/syscall_64.tbl`         | `src/syscall.rs`        |
| `kernel/printk/printk.c`                         | macros + `src/tty.rs`   |
| `kernel/panic.c`                                 | panic handler em `main.rs` |
| `init/main.c`                                    | `kernel_main` em `main.rs` |

## Limitações didáticas

- Uma única CPU; IRQ0 (timer) habilitada, mas IRQ1 do teclado continua
  mascarada porque o driver usa polling.
- Sem heap, processos, escalonador, sistema de arquivos ou rede.
- O "processo" de usuário é um programa de máquina copiado para uma página e
  executado uma vez; não há `fork`, `exec` nem troca de contexto preemptiva.
- Os oito consoles virtuais compartilham o mesmo shell: não há um `getty`
  residente por terminal nem um escalonador para alternar entre eles.
- As syscalls são `exit`, `write`, `read` e `get_tty`; não há cópia segura
  como `copy_from_user`, apenas validação da região e materialização das
  páginas do usuário antes do acesso.
- Apenas o primeiro GiB é mapeado por identidade; o kernel usa a metade alta.
  As regiões de demand paging têm 2 MiB cada e ficam fora desse GiB.
- Modo texto VGA depende de BIOS/CSM. Em PCs UEFI sem CSM, o vídeo VGA
  legado pode não existir; nesse caso seria necessário um driver GOP/framebuffer.

## Próximos passos naturais

1. Criar um escalonador simples e processos de verdade com espaços de endereço
   separados, permitindo um `getty` residente em cada `tty`.
2. Adicionar `fork`, `exec` e um mini shell em modo usuário.
3. Adicionar um alocador de heap (`alloc` + linked list allocator).
4. Habilitar IRQ1 e transformar o teclado em entrada por interrupção.
5. Implementar um driver de framebuffer (GOP/VBE) para PCs UEFI.
