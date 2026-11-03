# Rust VGA Kernel

Kernel mínimo escrito em Rust (`#![no_std]`) que inicializa um PC padrão em
BIOS/x86_64, entra em modo longo e abre um terminal em **modo texto VGA 80x25**
com teclado PS/2 e alguns comandos de shell.

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
- Teclado PS/2 (controlador 8042) por polling, com Shift e Backspace.
- Saída de depuração em COM1 (38400 8N1).
- Shell mínimo: `help`, `clear`, `echo`, `info`, `demand`, `usertest`,
  `userprot`, `mem`, `reboot`, `halt`.
- Panic handler que imprime a mensagem no console e para a CPU.

Testado em QEMU 11.1.3 (SeaBIOS): boot, tela VGA, teclado e comandos funcionam.

## Estrutura

```
output/
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
    ├── idt.rs                # IDT, stubs de exceção/IRQ, PIC e PIT
    ├── paging.rs             # frames, tabelas de 4 KiB e demand paging
    ├── user.rs               # entrada em ring 3 e demos de proteção
    ├── arch.rs               # inb/outb, halt, reboot, sti
    ├── vga.rs                # driver do console VGA (vgacon)
    ├── keyboard.rs           # driver do teclado PS/2 (atkbd/i8042)
    ├── serial.rs             # UART 16550 / COM1
    ├── shell.rs              # prompt e comandos
    └── sync.rs               # spinlock do console
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
cd output
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

| Comando         | Efeito                                      |
|-----------------|---------------------------------------------|
| `help`          | lista os comandos                           |
| `clear`         | limpa a tela VGA                            |
| `echo <texto>`  | imprime o texto                             |
| `info`          | informações do kernel                       |
| `demand`        | testa demand paging na região do kernel     |
| `usertest`      | executa demo em ring 3 com page fault       |
| `userprot`      | tenta acessar o kernel em ring 3 (deve falhar) |
| `mem`           | frames, page faults e bits U/S              |
| `reboot`        | reinicia o PC (reset via 8042)              |
| `halt`          | para a CPU                                  |

## Documentação

A documentação técnica completa (arquitetura, boot, memória, drivers, shell,
build, testes e comparação com o Linux) está em `docs/`:

- `docs/main.pdf` — documento compilado;
- `docs/main.tex` — fonte LaTeX;
- `docs/sections/` — capítulos.

Para regerar o PDF:

```sh
cd docs
make
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
                                 ├─ paging::init()   (frames do Multiboot2)
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
| `drivers/video/console/vgacon.c`                 | `src/vga.rs`            |
| `drivers/input/serio/i8042.c`                    | `src/keyboard.rs`       |
| `drivers/input/keyboard/atkbd.c`                 | `src/keyboard.rs`       |
| `drivers/tty/serial/8250/8250_port.c`            | `src/serial.rs`         |
| `kernel/printk/printk.c`                         | macros + `src/vga.rs`   |
| `kernel/panic.c`                                 | panic handler em `main.rs` |
| `init/main.c`                                    | `kernel_main` em `main.rs` |

## Limitações didáticas

- Uma única CPU; IRQ0 (timer) habilitada, mas IRQ1 do teclado continua
  mascarada porque o driver usa polling.
- Sem heap, processos, escalonador, sistema de arquivos ou rede.
- O "processo" de usuário é um programa de máquina copiado para uma página e
  executado uma vez; não há `fork`, `exec` nem troca de contexto preemptiva.
- A única chamada de sistema é `int 0x80` com `rax = 0` (sair); não há
  validação de ponteiros nem cópia segura entre espaços de endereço.
- Apenas o primeiro GiB é mapeado por identidade; o kernel usa a metade alta.
  As regiões de demand paging têm 2 MiB cada e ficam fora desse GiB.
- Modo texto VGA depende de BIOS/CSM. Em PCs UEFI sem CSM, o vídeo VGA
  legado pode não existir; nesse caso seria necessário um driver GOP/framebuffer.

## Próximos passos naturais

1. Criar um escalonador simples e processos de verdade com espaços de endereço
   separados.
2. Implementar mais syscalls (`read`, `write`, `exit`) com validação de
   ponteiros do usuário.
3. Adicionar um alocador de heap (`alloc` + linked list allocator).
4. Habilitar IRQ1 e transformar o teclado em entrada por interrupção.
5. Implementar um driver de framebuffer (GOP/VBE) para PCs UEFI.
