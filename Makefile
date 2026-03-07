.PHONY: help build test clean run check

CARGO := cargo
TARGET := target/release/tdorr

help:
	@echo "╔════════════════════════════════════════════════════════════════════════╗"
	@echo "║                    tdorr - Rust Tdarr Alternative                      ║"
	@echo "║                     GPU-accelerated video transcoding                   ║"
	@echo "╚════════════════════════════════════════════════════════════════════════╝"
	@echo ""
	@echo "USAGE: make [target]"
	@echo ""
	@echo "TARGETS:"
	@echo "  build              Compile release binary (creates ./tdorr symlink)"
	@echo "  test               Run all tests with output"
	@echo "  check              Run clippy linter and fmt checks"
	@echo "  clean              Remove build artifacts"
	@echo "  help               Show this message"
	@echo ""
	@echo "EXAMPLES:"
	@echo "  make build         # Compile and link binary"
	@echo "  make test          # Run tests"
	@echo "  make check         # Lint before committing"
	@echo ""

build:
	$(CARGO) build --release
	ln -sf $(TARGET) tdorr

test:
	$(CARGO) test -- --nocapture

clean:
	$(CARGO) clean

check:
	$(CARGO) clippy -- -D warnings
	$(CARGO) fmt --check
