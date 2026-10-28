# Virtuoso — thin forwarding shell over the canonical CLI (`cargo install --path .`
# provides `virtuoso`). No logic lives here; behavior and contracts are owned by the
# single-package crate (see AGENTS.md and docs/). Bare `make` prints the target list.
# Note: make folds any recipe failure to its own exit status 2; the verdict line and
# verdict.json remain the sole authority (the exit-code contract belongs to `virtuoso`).
#
# Extra flags ride on A (shell-split): make test A="--replay-until-fail 5"
# Named vars: make probe C='uname -a'   |   make kclone URL=<git-url>

.DEFAULT_GOAL := help
.PHONY: help clippy install doctor build test shell probe skill clean kbuild kclone

help:
	@echo "Virtuoso targets (each forwards to the canonical 'virtuoso' CLI):"
	@echo "  make clippy           CI gate: cargo clippy --all-targets -- -D warnings"
	@echo "  make install          refresh the installed binary after Rust source changes"
	@echo "  make doctor           environment health check (A=--verbose)"
	@echo "  make build            rebuild initrd.img / rootfs.img / tools.img (A=--busybox-only)"
	@echo "  make test             build -> launch -> judge -> verdict (A=--timeout 120)"
	@echo "  make shell            interactive VM (A=--kvm | --tcg | --gdb)"
	@echo "  make probe            agent command batch over virtio-serial (C='uname -a')"
	@echo "  make skill            install AI skills into the kernel tree"
	@echo "  make clean            remove generated images and staging directories"
	@echo "  make kbuild           kernel build inside the container"
	@echo "  make kclone URL=...   clone kernel source into a named volume (A=--ref r --full)"
	@echo ""
	@echo "AI verification loop: make clippy doctor test"

clippy:
	cargo clippy --all-targets -- -D warnings

install:
	cargo install --path .

doctor:
	virtuoso doctor $(A)

build:
	virtuoso build $(A)

kclone:
	virtuoso kernel clone $(URL) $(A)

kbuild:
	virtuoso kernel build $(A)

test:
	virtuoso test $(A)

shell:
	virtuoso shell $(A)

probe:
	virtuoso probe --cmd '$(C)' $(A)

skill:
	virtuoso skill install

clean:
	virtuoso clean
