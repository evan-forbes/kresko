PREFIX ?= $(HOME)/.local
UBUNTU_TARGET = x86_64-unknown-linux-musl
ZEBRA_DIR ?= ../zebra

.PHONY: build install uninstall clean txblast ubuntu

build:
	cargo build --release

txblast:
	@echo "Building the single Ubuntu-compatible kresko binary"
	@$(MAKE) ubuntu

ubuntu:
	cargo build --release --target $(UBUNTU_TARGET)
	mkdir -p target/ubuntu
	rm -f target/ubuntu/txblast
	cp target/$(UBUNTU_TARGET)/release/kresko target/ubuntu/kresko

install: build
	install -d $(DESTDIR)$(PREFIX)/bin
	install -m 755 target/release/kresko $(DESTDIR)$(PREFIX)/bin/kresko

uninstall:
	rm -f $(DESTDIR)$(PREFIX)/bin/kresko

clean:
	cargo clean
