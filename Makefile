PREFIX ?= $(HOME)/.local
UBUNTU_TARGET = x86_64-unknown-linux-musl

.PHONY: build install uninstall clean txblast

build:
	cargo build --release

txblast:
	cargo build --release --target $(UBUNTU_TARGET)
	mkdir -p target/ubuntu
	cp target/$(UBUNTU_TARGET)/release/kresko target/ubuntu/txblast

install: build
	install -d $(DESTDIR)$(PREFIX)/bin
	install -m 755 target/release/kresko $(DESTDIR)$(PREFIX)/bin/kresko

uninstall:
	rm -f $(DESTDIR)$(PREFIX)/bin/kresko

clean:
	cargo clean
