# Changelog
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

{{ version-heading }}

### Added

- Adds hdk::commit_entry_result() which features: optional argument to include additional provenances. [#1320](https://github.com/holochain/holochain-rust/pull/1320)
- default.nix file added to facilitate `nix-env` based binary installation [#1356](https://github.com/holochain/holochain-rust/pull/1356)

### Changed
- Changes `LinkAdd` and `RemoveEntry` so that they return a hash instead of a null [#1343](https://github.com/holochain/holochain-rust/pull/1343)
- Merged `default.nix` and `shell.nix` to improve `nix-shell` handling [#1371](https://github.com/holochain/holochain-rust/pull/1371)

### Deprecated

### Removed

### Fixed

### Security
