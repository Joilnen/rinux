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
- Console VGA em modo texto (80x25, buffer em `0xB8000`), com rolagem,
  cursor de hardware e cores.
- Teclado PS/2 (controlador 8042) por polling, com Shift e Backspace.
- Saída de depuração em COM1 (38400 8N1).
- Shell mínimo: `help`, `clear`, `echo`, `info`, `reboot`, `halt`.
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
    ├── boot.rs               # multiboot2, GDT, paginação, modo longo
    ├── arch.rs               # inb/outb, halt, reboot
    ├── vga.rs                # driver do console VGA (vgacon)
    ├── keyboard.rs           # driver do teclado PS/2 (atkbd/i8042)
    ├── serial.rs             # UART 16550 / COM1
    ├── shell.rs              # prompt e comandos
    └── sync.rs               # spinlock do console
```

## Requisitos

- Rust com `rustup` (o `rust-toolchain.toml` já pede o alvo
  `x86_64-unknown-none`). O `Makefile` chama `rustup run stable cargo` quando
  o `rustup` existe, porque em algumas distros o `cargo` do sistema não tem
  esse alvo instalado.
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

O `make run` usa:

```sh
qemu-system-x86_64 -cdrom vga-kernel.iso -boot order=d -m 128M -serial stdio
```

Para limpar:

```sh
make clean
```

Se aparecer `can't find crate for core` / `the x86_64-unknown-none target
may not be installed`, o `cargo` usado não é o do `rustup`. O `Makefile` já
contorna isso, mas você também pode rodar diretamente:

```sh
rustup target add x86_64-unknown-none
rustup run stable cargo build --release --target x86_64-unknown-none
```

Ou colocar `$HOME/.cargo/bin` antes de `/usr/bin` no `PATH`.

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
                                ├─ keyboard::init()
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
| `arch/x86/kernel/cpu/common.c` (GDT)             | `src/boot.rs`           |
| `arch/x86/mm/init_64.c` (paginação)              | `src/boot.rs`           |
| `drivers/video/console/vgacon.c`                 | `src/vga.rs`            |
| `drivers/input/serio/i8042.c`                    | `src/keyboard.rs`       |
| `drivers/input/keyboard/atkbd.c`                 | `src/keyboard.rs`       |
| `drivers/tty/serial/8250/8250_port.c`            | `src/serial.rs`         |
| `kernel/printk/printk.c`                         | macros + `src/vga.rs`   |
| `kernel/panic.c`                                 | panic handler em `main.rs` |
| `init/main.c`                                    | `kernel_main` em `main.rs` |

## Limitações didáticas

- Uma única CPU; interrupções desligadas.
- Teclado por polling, sem IRQ1/IDT.
- Sem heap, processos, syscalls, sistema de arquivos ou rede.
- Sem tratamento de exceções: uma falha grave gera triple fault.
- Apenas o primeiro GiB é mapeado por identidade; o kernel usa a metade alta.
- Modo texto VGA depende de BIOS/CSM. Em PCs UEFI sem CSM, o vídeo VGA
  legado pode não existir; nesse caso seria necessário um driver GOP/framebuffer.

## Próximos passos naturais

1. Criar IDT e handlers para exceções (evita triple fault).
2. Programar o PIC/PIT e habilitar IRQ1 para teclado com interrupções.
3. Adicionar um alocador de heap (`alloc` + linked list allocator).
4. Implementar um driver de framebuffer (GOP/VBE) para PCs UEFI.
5. Implementar um pequeno `init` e chamadas de sistema.
