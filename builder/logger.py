"""
Colored logging for build output.
"""
import sys
from enum import Enum
from typing import Optional


class Color(Enum):
    """ANSI color codes."""
    RED = "\033[0;31m"
    GREEN = "\033[0;32m"
    YELLOW = "\033[1;33m"
    BLUE = "\033[0;34m"
    MAGENTA = "\033[0;35m"
    CYAN = "\033[0;36m"
    WHITE = "\033[0;37m"
    RESET = "\033[0m"
    BOLD = "\033[1m"


class Logger:
    """Colored logger for build output."""

    def __init__(self, use_color: bool = True, verbose: bool = False):
        self.use_color = use_color and sys.stdout.isatty()
        self.verbose = verbose

    def _colorize(self, text: str, color: Color) -> str:
        """Apply color to text if colors are enabled."""
        if self.use_color:
            return f"{color.value}{text}{Color.RESET.value}"
        return text

    def _print(self, prefix: str, message: str, color: Color) -> None:
        """Print a formatted message."""
        colored_prefix = self._colorize(prefix, color)
        print(f"{colored_prefix} {message}")

    def header(self, title: str) -> None:
        """Print a header section."""
        line = "=" * 50
        print()
        print(self._colorize(line, Color.GREEN))
        print(self._colorize(f"  {title}", Color.GREEN))
        print(self._colorize(line, Color.GREEN))
        print()

    def info(self, message: str) -> None:
        """Print an info message."""
        self._print("→", message, Color.BLUE)

    def success(self, message: str) -> None:
        """Print a success message."""
        self._print("✓", message, Color.GREEN)

    def warning(self, message: str) -> None:
        """Print a warning message."""
        self._print("!", message, Color.YELLOW)

    def error(self, message: str) -> None:
        """Print an error message."""
        self._print("✗", message, Color.RED)

    def debug(self, message: str) -> None:
        """Print a debug message (only in verbose mode)."""
        if self.verbose:
            self._print("·", message, Color.CYAN)

    def step(self, number: int, total: int, message: str) -> None:
        """Print a step in a sequence."""
        prefix = f"[{number}/{total}]"
        self._print(prefix, message, Color.MAGENTA)

    def target(self, target: str, status: str = "") -> None:
        """Print target build status."""
        target_str = self._colorize(target, Color.YELLOW)
        if status:
            status_str = self._colorize(f"({status})", Color.CYAN)
            print(f"  → {target_str} {status_str}")
        else:
            print(f"  → {target_str}")

    def binary(self, name: str, size: str) -> None:
        """Print binary information."""
        name_str = self._colorize(name, Color.GREEN)
        size_str = self._colorize(f"({size})", Color.CYAN)
        print(f"    {name_str} {size_str}")

    def newline(self) -> None:
        """Print an empty line."""
        print()

    def results(self, binaries: list) -> None:
        """Print build results summary."""
        self.header("Build Complete!")

        if binaries:
            self.info("Built binaries:")
            for binary_path, size in binaries:
                self.binary(binary_path.name, size)
        else:
            self.warning("No binaries were built")
