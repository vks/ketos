# Ketos

Ketos is a Lisp-like functional programming language.

The primary goal of Ketos is to serve as a scripting and extension language for
programs written in the [Rust programming language](https://www.rust-lang.org).

Ketos is compiled to bytecode and interpreted by pure Rust code.

[API Documentation](https://murarth.github.io/ketos/ketos/index.html)

[Language Documentation](docs/README.md)

## Building the library

To build Ketos into your Rust project, add the following to your `Cargo.toml`:

```toml
[dependencies]
ketos = { git = "https://github.com/murarth/ketos" }
```

## Building the REPL

The Ketos REPL requires GNU Readline.

Build and run tests:

    cargo test

Build optimized executable:

    cargo build --release

## Usage

`ketos` can be run as an interpreter to execute Ketos code files (`.kts`)
or run as an interactive read-eval-print loop.

## License

Ketos is distributed under the terms of both the MIT license and the
Apache License (Version 2.0).

See LICENSE-APACHE and LICENSE-MIT for details.
