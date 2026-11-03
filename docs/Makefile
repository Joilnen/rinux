.PHONY: all pdf pt both clean

# English is the primary documentation; Portuguese is kept as a translation.
LATEXMK ?= latexmk
MAIN    := main
PDF     := $(MAIN).pdf
PT_DIR  := pt

all: pdf

pdf: $(PDF)

$(PDF): $(MAIN).tex $(wildcard sections/*.tex)
	$(LATEXMK) -pdf -interaction=nonstopmode -halt-on-error $(MAIN).tex

# Build the Portuguese version in docs/pt/ (output: docs/pt/main.pdf).
pt:
	$(LATEXMK) -cd -pdf -interaction=nonstopmode -halt-on-error $(PT_DIR)/$(MAIN).tex

# Build both language versions.
both: pdf pt

clean:
	$(LATEXMK) -C $(MAIN).tex
	$(LATEXMK) -cd -C $(PT_DIR)/$(MAIN).tex
	rm -f $(PDF) $(PT_DIR)/$(PDF)
