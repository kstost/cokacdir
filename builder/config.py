"""
Build configuration for COKACDIR Rust project.
"""
from dataclasses import dataclass, field
from pathlib import Path
from typing import List, Optional
import platform


@dataclass
class BuildConfig:
    """Build configuration settings."""

    # Tool versions
    zig_version: str = "0.13.0"
    macos_sdk_version: str = "14.0"

    # Paths (relative to project root)
    builder_dir: Path = field(default_factory=lambda: Path("builder"))
    tools_dir: Path = field(default_factory=lambda: Path("builder/tools"))
    dist_dir: Path = field(default_factory=lambda: Path("dist"))

    # Build settings
    release: bool = True
    clean: bool = False

    # Target platforms
    targets: List[str] = field(default_factory=list)
    build_native: bool = True

    # URLs
    @property
    def macos_sdk_url(self) -> str:
        return f"https://github.com/joseluisq/macosx-sdks/releases/download/{self.macos_sdk_version}/MacOSX{self.macos_sdk_version}.sdk.tar.xz"

    @property
    def zig_url(self) -> str:
        arch = self.host_arch
        os_name = "linux" if self.host_os == "linux" else "macos"
        return f"https://ziglang.org/download/{self.zig_version}/zig-{os_name}-{arch}-{self.zig_version}.tar.xz"

    # Host detection
    @property
    def host_arch(self) -> str:
        machine = platform.machine().lower()
        if machine in ("x86_64", "amd64"):
            return "x86_64"
        elif machine in ("aarch64", "arm64"):
            return "aarch64"
        return machine

    @property
    def host_os(self) -> str:
        system = platform.system().lower()
        if system == "darwin":
            return "macos"
        return system

    def __post_init__(self):
        # Convert string paths to Path objects if needed
        if isinstance(self.builder_dir, str):
            self.builder_dir = Path(self.builder_dir)
        if isinstance(self.tools_dir, str):
            self.tools_dir = Path(self.tools_dir)
        if isinstance(self.dist_dir, str):
            self.dist_dir = Path(self.dist_dir)


# Available Rust targets
RUST_TARGETS = {
    "macos-arm64": "aarch64-apple-darwin",
    "macos-x86_64": "x86_64-apple-darwin",
    "linux-arm64": "aarch64-unknown-linux-gnu",
    "linux-x86_64": "x86_64-unknown-linux-gnu",
}

# Friendly names for targets
TARGET_NAMES = {
    "aarch64-apple-darwin": "macos-aarch64",
    "x86_64-apple-darwin": "macos-x86_64",
    "aarch64-unknown-linux-gnu": "linux-aarch64",
    "x86_64-unknown-linux-gnu": "linux-x86_64",
}
