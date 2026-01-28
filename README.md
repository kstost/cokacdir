# COKACDIR (Rust Edition)

Norton Commander style dual-panel file manager for terminal - now rewritten in Rust for better performance.

## Features

- **Dual-panel navigation**: Classic Norton Commander style interface
- **Fast file operations**: Copy, move, delete, rename files and directories
- **Built-in file viewer**: View files with search functionality
- **Built-in file editor**: Edit files directly in the terminal
- **Process manager**: View and manage running processes
- **Keyboard-driven**: Efficient navigation with keyboard shortcuts

## Installation

### From Source

```bash
# Clone the repository
git clone https://github.com/kstost/cokacdir.git
cd cokacdir_rust

# Build release version
cargo build --release

# Run
./target/release/cokacdir
```

### Install globally

```bash
cargo install --path .
```

## Keyboard Shortcuts

### Navigation

| Key | Action |
|-----|--------|
| `↑`/`↓` | Move cursor |
| `PgUp`/`PgDn` | Move 10 lines |
| `Home`/`End` | Go to start/end |
| `Enter` | Open directory |
| `Esc` | Go to parent directory |
| `Tab` | Switch panel |

### Selection

| Key | Action |
|-----|--------|
| `Space` | Select/deselect file |
| `*` | Select/deselect all |
| `f` | Quick find by name |
| `/` | Go to path |

### Sorting (toggle asc/desc)

| Key | Action |
|-----|--------|
| `n` | Sort by name |
| `s` | Sort by size |
| `d` | Sort by date |

### Functions

| Key | Action |
|-----|--------|
| `1` | Help |
| `2` | File info |
| `3` | View file |
| `4` | Edit file |
| `5` | Copy |
| `6` | Move |
| `7` | Create directory |
| `8` | Delete |
| `9` | Process manager |
| `0`/`q` | Quit |
| `r`/`R` | Rename |

### File Viewer

| Key | Action |
|-----|--------|
| `q` | Close viewer |
| `/` | Search |
| `n` | Next match |
| `N` | Previous match |
| `↑`/`↓`/`j`/`k` | Scroll |
| `PgUp`/`PgDn` | Page scroll |
| `g`/`G` | Go to start/end |

### File Editor

| Key | Action |
|-----|--------|
| `Ctrl+S` | Save |
| `Ctrl+Q` | Quit (warns if unsaved) |
| `Ctrl+X` | Discard changes and quit |
| `Arrows` | Navigate |
| `Tab` | Insert spaces |

### Process Manager

| Key | Action |
|-----|--------|
| `k` | Kill process (SIGTERM) |
| `9` | Force kill (SIGKILL) |
| `r` | Refresh list |
| `p` | Sort by PID |
| `c` | Sort by CPU |
| `m` | Sort by memory |
| `n` | Sort by command name |
| `Esc` | Close |

## Comparison with TypeScript Version

| Feature | TypeScript | Rust |
|---------|-----------|------|
| Startup time | ~500ms | ~10ms |
| Memory usage | ~50MB | ~5MB |
| Binary size | N/A (requires Node.js) | ~2MB |
| Dependencies | node_modules | Static binary |

## License

MIT License

## Author

cokac <monogatree@gmail.com>

Homepage: https://cokacdir.cokac.com
