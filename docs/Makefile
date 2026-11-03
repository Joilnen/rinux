.PHONY: all pdf clean

LATEXMK ?= latexmk
MAIN    := main
PDF     := $(MAIN).pdf

all: pdf

pdf: $(PDF)

$(PDF): $(MAIN).tex $(wildcard sections/*.tex)
	$(LATEXMK) -pdf -interaction=nonstopmode -halt-on-error $(MAIN).tex

clean:
	$(LATEXMK) -C $(MAIN).tex
	rm -f $(PDF)
