PREFIX ?= $(HOME)/.local
NU7_ZEBRA_ROOT ?= ../nu7-testnet
ZEBRA_ROOT ?= ../zebra

# Where the Claude Code agent skill is installed. ~/.claude/skills/<name>/ is the
# global (all-projects) location.
SKILLS_DIR ?= $(HOME)/.claude/skills
SKILL_NAME ?= kresko-fleet

.PHONY: build install install-skill install-py uninstall uninstall-skill clean txblast ubuntu

build:
	cargo build --release

txblast:
	@echo "Building the single Ubuntu-compatible kresko binary"
	@$(MAKE) ubuntu

ubuntu:
	NU7_ZEBRA_ROOT="$(NU7_ZEBRA_ROOT)" ZEBRA_ROOT="$(ZEBRA_ROOT)" ./scripts/build-ubuntu.sh --kresko-only --output-dir target/ubuntu

# Install the Rust `kresko` binary and the agent skill. The Python fleet package
# is installed separately with `make install-py`.
install: build install-skill
	install -d $(DESTDIR)$(PREFIX)/bin
	install -m 755 target/release/kresko $(DESTDIR)$(PREFIX)/bin/kresko

# Install the Claude Code agent skill so it's available across projects.
install-skill:
	install -d $(SKILLS_DIR)/$(SKILL_NAME)
	install -m 644 skills/$(SKILL_NAME)/SKILL.md $(SKILLS_DIR)/$(SKILL_NAME)/SKILL.md
	@echo "installed skill -> $(SKILLS_DIR)/$(SKILL_NAME)/SKILL.md"

# Install the Python fleet package + `kresko-fleet` CLI globally (isolated env on
# PATH) so it's usable outside this repo. To import `from kresko import Fleet` in
# another project, add this package as a dependency there instead.
install-py:
	uv tool install --force .
	@echo "installed 'kresko-fleet' console (uv tool). Run: kresko-fleet --help"

uninstall: uninstall-skill
	rm -f $(DESTDIR)$(PREFIX)/bin/kresko

uninstall-skill:
	rm -rf $(SKILLS_DIR)/$(SKILL_NAME)

clean:
	cargo clean
