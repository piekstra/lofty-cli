# lofty CLI — task runner: build/test/lint/fmt/clean/install, a cheap `smoke`
# check, and an aggregate `verify` gate. Thin wrappers over cargo so a green
# local run predicts a green CI run.

BIN := lofty
CARGO := cargo

.PHONY: all build release test lint fmt fmt-check clean install deps smoke verify

all: verify

build:
	$(CARGO) build

release:
	$(CARGO) build --release

test:
	$(CARGO) test --all

lint:
	$(CARGO) clippy --all-targets -- -D warnings

fmt:
	$(CARGO) fmt --all

fmt-check:
	$(CARGO) fmt --all -- --check

clean:
	$(CARGO) clean

install:
	$(CARGO) install --path . --force

deps:
	$(CARGO) fetch

# Cheap sanity checks needing no config or network: version + top-level help.
smoke: release
	./target/release/$(BIN) --version
	./target/release/$(BIN) --help > /dev/null
	./target/release/$(BIN) info > /dev/null

verify: fmt-check lint test smoke
