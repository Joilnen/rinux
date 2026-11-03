TARGET  := x86_64-unknown-none
KERNEL  := target/$(TARGET)/release/vga-kernel
ISO     := vga-kernel.iso
ISODIR  := build/iso

# O alvo x86_64-unknown-none e instalado pelo rustup. Em distros como Arch,
# /usr/bin/cargo e /usr/bin/rustc podem ser os do sistema (sem esse alvo).
# Preferimos o cargo do rustup e fixamos RUSTC para que o cargo nao encontre
# o rustc do sistema pelo PATH.
RUSTC   ?= $(shell command -v rustup >/dev/null 2>&1 && rustup which rustc)
ifneq ($(RUSTC),)
export RUSTC
endif

CARGO   ?= $(shell command -v rustup >/dev/null 2>&1 && echo "rustup run stable cargo" || echo cargo)

.PHONY: all kernel iso run clean

all: iso

kernel:
	$(CARGO) build --release --target $(TARGET)

iso: kernel
	rm -rf $(ISODIR)
	mkdir -p $(ISODIR)/boot/grub
	cp $(KERNEL) $(ISODIR)/boot/kernel.elf
	cp boot/grub/grub.cfg $(ISODIR)/boot/grub/grub.cfg
	grub-mkrescue -o $(ISO) $(ISODIR)

# Janela VGA do QEMU + serial no terminal. -boot order=d forca o CD,
# porque alguns SeaBIOS tentam o disco primeiro.
run: iso
	qemu-system-x86_64 -cdrom $(ISO) -boot order=d -m 128M -serial stdio

clean:
	$(CARGO) clean
	rm -rf build $(ISO)
