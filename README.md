# broiler

Brightness Controller: a planned lightweight native Windows monitor utility in Rust.

Status: research and architecture only. No implementation, binaries, or installable Scoop package exist yet.

Architecture and implementation plan prepared by Hermes Agent (Nous Research) at the direction of hrkcz001. Implementation has not started.

See [the design](docs/architecture.md) for decisions, limitations, configuration semantics, the CLI contract, and staged acceptance criteria. Sample TOML files live in `examples/`; these are proposals, not a currently implemented API.

## Scope

External HDMI and DisplayPort monitors through Windows DDC/CI APIs. A user-session tray daemon and a console CLI. No Electron, WebView, Windows service, kernel driver, cloud connection, or vendor-specific control protocol.

## License

Apache License 2.0. Copyright hrkcz001 2026.
