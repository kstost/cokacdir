# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.4.3] - 2026-01-29

### Added
- Production deployment configuration files (rustfmt.toml, .editorconfig)
- Security policy documentation (SECURITY.md)
- This changelog file

### Fixed
- Build script `--debug` flag now works correctly (was always defaulting to release)

### Changed
- Added Cargo.toml lints section for code quality enforcement

## [0.4.2] - 2026-01-28

### Added
- Cross-compilation support for macOS (arm64, x86_64)
- Cross-compilation support for Linux (arm64, x86_64)
- Local tool installation system (Rust, zig, cargo-zigbuild)
- macOS SDK integration for cross-compilation

### Changed
- Build system now uses local tools directory
- Improved build script with better error handling

## [0.4.1] - 2026-01-27

### Added
- AI-powered natural language command interface
- Markdown rendering in AI responses
- Image viewer with terminal-based display

### Fixed
- Various UI rendering issues
- File operation progress display

## [0.4.0] - 2026-01-26

### Added
- Dual-panel file manager interface
- File operations (copy, move, delete, rename)
- Directory navigation with keyboard shortcuts
- File preview functionality
- Search functionality

### Changed
- Complete rewrite in Rust for performance
- TUI implementation using ratatui

## [Unreleased]

### Planned
- GitHub Actions CI/CD workflow
- Additional platform support
- Plugin system
