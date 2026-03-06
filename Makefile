.PHONY: build test clean run

CARGO := cargo
TARGET := target/release/tdorr

build:
	$(CARGO) build --release
	ln -sf $(TARGET) tdorr

test:
	$(CARGO) test -- --nocapture

clean:
	$(CARGO) clean

run: build
	./$(TARGET) --config config.yaml /path/to/media

check:
	$(CARGO) clippy -- -D warnings
	$(CARGO) fmt --check
