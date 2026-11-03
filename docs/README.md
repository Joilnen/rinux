# Documentação do Rust VGA Kernel

Documentação técnica completa do kernel, escrita em LaTeX e compilada para
PDF. O conteúdo cobre arquitetura, cadeia de boot, memória, drivers VGA/PS-2/
serial, shell, build, testes e uma análise quantitativa da similaridade com o
kernel Linux 7.3.1.

## Arquivos

```
docs/
|-- main.tex                  preâmbulo, pacotes e inclusão dos capítulos
|-- Makefile                  build do PDF com latexmk
|-- README.md                 este arquivo
|-- main.pdf                  PDF gerado
`-- sections/
    |-- 01-introducao.tex
    |-- 02-visao-geral.tex
    |-- 03-arquitetura.tex
    |-- 04-boot.tex
    |-- 05-base.tex
    |-- 06-vga.tex
    |-- 07-keyboard.tex
    |-- 08-serial.tex
    |-- 09-shell.tex
    |-- 10-build.tex
    |-- 11-testes.tex
    |-- 12-similaridade.tex
    |-- 13-limitacoes.tex
    `-- 14-apendices.tex
```

## Compilar

Requisitos: TeX Live com `pdflatex`, `latexmk` e os pacotes usuais
(`geometry`, `hyperref`, `booktabs`, `listings`, `tikz`, `tcolorbox`,
`babel-portuges`).

```sh
cd docs
make            # gera main.pdf
make clean      # remove PDF e artefatos auxiliares
```

Ou diretamente:

```sh
latexmk -pdf -interaction=nonstopmode -halt-on-error main.tex
```

## Conteúdo

1. Introdução, escopo e estado do projeto.
2. Visão geral, funcionalidades, requisitos e métricas.
3. Arquitetura em camadas, modelo de execução, dependências e decisões.
4. Boot Multiboot2, GDT, paginação, long mode e linker script.
5. Módulos de base: `main.rs`, `arch.rs` e `sync.rs`.
6. Console VGA 80x25.
7. Teclado PS/2 8042.
8. Serial COM1 (UART 16550).
9. Shell e comandos.
10. Build, ISO, GRUB e execução no QEMU.
11. Testes e evidências de execução.
12. Similaridade com o Linux, com pontuação ponderada e comparação de LOC.
13. Limitações, riscos e roadmap.
14. Apêndices: catálogo das 43 funções, mapa de portas/memória, glossário e
    referências.

## Nota sobre a similaridade

A análise do Capítulo 12 estima **≈13,5%** de similaridade funcional
ponderada no recorte de dez dimensões (boot, modo longo, paginação, console,
teclado, serial e serviços ausentes). Em relação ao kernel Linux completo, a
cobertura é muito menor; em linhas de código, o projeto tem cerca de
**0,0027%** da árvore Linux usada como referência. Não há código copiado do
Linux: a implementação é independente, em Rust.
