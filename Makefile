SHELL := /bin/sh
.PHONY: all build debug release test clean install help

all: release

help:
	@printf '%s\n' \
	  'targets:' \
	  '  make release   - cargo build --release  (default; produces target/release/libstryke_app.{dylib,so})' \
	  '  make debug     - cargo build' \
	  '  make test      - cargo test' \
	  '  make install   - `s pkg install -g .` (copies source + cdylib into ~/.stryke/store/stryke-app@<ver>/) so `use App` resolves' \
	  '  make clean     - cargo clean'

release:
	cargo build --release

debug build:
	cargo build

test:
	cargo test

install: release
	s pkg install -g .

clean:
	cargo clean
