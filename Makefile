.PHONY: check fmt build

check:
	cargo clippy --quiet -- -Dwarnings -Dclippy::all -Dclippy::pedantic
	cargo fmt --all -- --check

fmt:
	cargo fmt --all

build:
	bin/build
