.PHONY: build test clean run

CARGO := cargo
TARGET := target/release/hvecuum

build:
	$(CARGO) build --release
	ln -sf $(TARGET) hvecuum

test:
	$(CARGO) test -- --nocapture

clean:
	$(CARGO) clean

run: build
	./$(TARGET) --config config.yaml /mnt/media/dumb-tv

check:
	$(CARGO) clippy -- -D warnings
	$(CARGO) fmt --check
