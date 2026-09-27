# Optional SDK + Engine integration

This fixture implements SDK `Reasoner` and `ExternalRuntime` with a single Engine adapter. Trusted setup binds the dispatch-assigned worker ID and ticket ID to an Engine grant. Engine uses its gateway control route to choose a sanctioned semantic capability, then sends that capability to a fixture unified gateway. Neither implementation crate depends on the other.

Clone `Seam`, `Seam-SDK`, and `Seam-Engine` as sibling directories, then run:

```sh
cargo run --manifest-path Seam/examples/combined/Cargo.toml
```

Expected output: `Fixture issue inspected`.

The fixture stands in for gateway infrastructure. A deployment should replace it and the in-memory grant source with its trusted services. The adapter stays outside both core crates.
