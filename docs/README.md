# Rust VGA Kernel Documentation

Complete technical documentation for the kernel, written in LaTeX and compiled
to PDF. The content covers the architecture, boot chain, virtual memory, demand
paging, IDT/PIC/PIT, ring 3, the VGA/PS-2/serial drivers, the shell, the build
process, tests and a quantitative analysis of the similarity with the Linux
kernel.

**English is the primary version.** The Portuguese translation is kept under
`pt/`.

## Files

```
docs/
|-- main.tex                  English preamble, packages and chapter includes
|-- Makefile                  PDF build with latexmk (English default, pt target)
|-- README.md                 this file
|-- main.pdf                  generated English PDF
|-- sections/                 English chapters
|   |-- 01-introducao.tex
|   |-- 02-visao-geral.tex
|   |-- 03-arquitetura.tex
|   |-- 04-boot.tex
|   |-- 05-base.tex
|   |-- 06-vga.tex
|   |-- 07-keyboard.tex
|   |-- 08-serial.tex
|   |-- 09-shell.tex
|   |-- 09a-memoria-idt.tex
|   |-- 10-build.tex
|   |-- 11-testes.tex
|   |-- 12-similaridade.tex
|   |-- 13-limitacoes.tex
|   `-- 14-apendices.tex
`-- pt/                       Portuguese translation
    |-- main.tex              Portuguese preamble and chapter includes
    |-- main.pdf              generated Portuguese PDF
    `-- sections/             Portuguese chapters (same file names)
```

## Build

Requirements: TeX Live with `pdflatex`, `latexmk` and the usual packages
(`geometry`, `hyperref`, `booktabs`, `listings`, `tikz`, `tcolorbox`,
`babel-english`, `babel-portuguese`).

```sh
cd docs
make            # English version -> docs/main.pdf
make pt         # Portuguese version -> docs/pt/main.pdf
make both       # both versions
make clean      # remove PDFs and auxiliary files
```

Or directly:

```sh
latexmk -pdf -interaction=nonstopmode -halt-on-error main.tex
latexmk -cd -pdf -interaction=nonstopmode -halt-on-error pt/main.tex
```

The kernel itself builds with `cargo build` (or `cargo build --release`). The
`.cargo/config.toml` fixes the `x86_64-unknown-none` target and uses the
`../scripts/rustc` wrapper, which calls the `rustup` `rustc`; this way the
command works even when the system `cargo` is first in `PATH`.

## Contents

1. Introduction, scope and project status.
2. Overview, features, requirements and metrics.
3. Layered architecture, execution model, dependencies and decisions.
4. Multiboot2 boot, GDT, paging, long mode and the linker script.
5. Base modules: `main.rs`, `arch.rs` and `sync.rs`.
6. VGA 80x25 console.
7. PS/2 8042 keyboard.
8. COM1 serial (UART 16550).
9. Shell and commands.
10. Virtual memory, frame allocator, demand paging, GDT/TSS, IDT, PIC/PIT and
    ring 3 demos.
10. Build, ISO, GRUB and execution in QEMU.
11. Tests and execution evidence.
12. Similarity with Linux, with weighted scoring and LOC comparison.
13. Limitations, risks and roadmap.
14. Appendices: catalog of the 276 functions, port/memory map, glossary and
    references.

## Note on similarity

The analysis in Chapter 12 estimates **≈25.5%** weighted functional similarity
across ten dimensions (boot, long mode, paging, console, keyboard, serial,
interrupts, memory, processes and missing services). Compared with the complete
Linux kernel, coverage is much smaller; in lines of code, the project has about
**0.0179%** of the Linux tree used as reference. No code was copied from Linux:
the implementation is independent and written in Rust.
