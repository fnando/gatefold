.PHONY: check fmt build

check:
	cargo clippy --all-targets --quiet -- -Dwarnings -Dclippy::all -Dclippy::pedantic
	cargo fmt --all -- --check

fmt:
	cargo fmt --all

build:
	bin/build
