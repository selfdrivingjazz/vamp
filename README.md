# VAMP protocol

VAMP is an experimental media-catalog interoperability protocol. This repository
contains the MIT-licensed contract, Rust data types, and generated JSON schemas.
It does not contain or license the Vampire application.

## Public surface

- `crates/vamp-protocol/src/lib.rs`: serialized types and `schema()`.
- `crates/vamp-protocol/protocol/contract.json`: the canonical semantic and
  operation contract, bundled into the crate rather than read from another project.
- `crates/vamp-protocol/examples/schema.rs`: standalone schema-bundle output.

The Rust package version is `0.1.0`; the wire protocol version is `0.1`. The
contract's status remains experimental. The schema bundle has 15 generated schema
roots plus the semantic contract; it does not cover every operation payload.
Existing implementation notes describe current Vampire behavior, not a requirement
to reproduce its internals. Wire names and serialized types are unchanged by the
project separation.

## Use and verify

```sh
cargo run -p vamp-protocol --example schema
cargo package -p vamp-protocol --allow-dirty
```

The example emits the same bundle as Vampire's `vamp schema` command, without
building the app or engine. Cargo package verification compiles the bundled
contract from the package itself. Review `cargo package -p vamp-protocol --list`
before publishing. Only this protocol crate belongs in a public source release.

Rust consumers can depend on `vamp-protocol` from a checkout. A crates.io dependency
requires publishing the package first; this local separation does not publish it.

## Vampire is separate

Vampire is the proprietary application implementing VAMP, maintained separately
in the private sibling `../vampire` project. Its UI, engine, native bridge, server,
CLI, processors, and release tooling are not included here. Protocol use and
independent implementations do not require a Vampire purchase. A Vampire purchase
does not grant rights to its source code.

This project has a new Git repository with no inherited application history and
no configured remote. Do not import the former mixed repository's history into a
public VAMP repository. Configure a protocol-only destination before pushing;
public hosting and package publication are separate release actions.

## License

MIT; see `LICENSE`. Vampire and its dependencies have their own licenses.
