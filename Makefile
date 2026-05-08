PREFIX ?= $(HOME)/.local
NU7_ZEBRA_ROOT ?= ../nu7-testnet
ZEBRA_ROOT ?= ../zebra

.PHONY: build install uninstall clean txblast ubuntu

build:
	cargo build --release

txblast:
	@echo "Building the single Ubuntu-compatible kresko binary"
	@$(MAKE) ubuntu

ubuntu:
	NU7_ZEBRA_ROOT="$(NU7_ZEBRA_ROOT)" ZEBRA_ROOT="$(ZEBRA_ROOT)" ./scripts/build-ubuntu.sh --kresko-only --output-dir target/ubuntu

install: build
	install -d $(DESTDIR)$(PREFIX)/bin
	install -m 755 target/release/kresko $(DESTDIR)$(PREFIX)/bin/kresko

uninstall:
	rm -f $(DESTDIR)$(PREFIX)/bin/kresko

clean:
	cargo clean
