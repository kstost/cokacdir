"""
Rust target management for cross-compilation.
"""
import subprocess
from dataclasses import dataclass
from typing import List, Set, Optional, Dict
from pathlib import Path

from .config import BuildConfig, RUST_TARGETS, TARGET_NAMES
from .logger import Logger


@dataclass
class Target:
    """Represents a build target."""

    rust_target: str  # e.g., "aarch64-apple-darwin"
    friendly_name: str  # e.g., "macos-aarch64"
    platform: str  # "macos" or "linux"
    arch: str  # "aarch64" or "x86_64"
    needs_zigbuild: bool = False  # True for cross-platform macOS builds
    is_native: bool = False  # True if this is the native target

    @classmethod
    def from_rust_target(cls, rust_target: str, config: BuildConfig) -> "Target":
        """Create a Target from a Rust target triple."""
        friendly_name = TARGET_NAMES.get(rust_target, rust_target)

        # Parse platform and arch
        if "apple-darwin" in rust_target:
            platform = "macos"
        elif "linux" in rust_target:
            platform = "linux"
        else:
            platform = "unknown"

        if "aarch64" in rust_target:
            arch = "aarch64"
        elif "x86_64" in rust_target:
            arch = "x86_64"
        else:
            arch = "unknown"

        # Determine if zigbuild is needed
        # 1. macOS targets when building on Linux
        # 2. Linux cross-architecture builds (e.g., aarch64 -> x86_64)
        needs_zigbuild = (
            (platform == "macos" and config.host_os == "linux") or
            (platform == "linux" and config.host_os == "linux" and arch != config.host_arch)
        )

        # Check if native
        is_native = (platform == config.host_os) and (arch == config.host_arch)

        return cls(
            rust_target=rust_target,
            friendly_name=friendly_name,
            platform=platform,
            arch=arch,
            needs_zigbuild=needs_zigbuild,
            is_native=is_native,
        )


class TargetManager:
    """Manages Rust targets and rustup operations."""

    def __init__(self, config: BuildConfig, logger: Logger, env: Optional[Dict[str, str]] = None):
        self.config = config
        self.logger = logger
        self.env = env  # Environment for running rustup commands
        self._installed_targets: Optional[Set[str]] = None

    def get_installed_targets(self) -> Set[str]:
        """Get list of installed Rust targets."""
        if self._installed_targets is not None:
            return self._installed_targets

        try:
            result = subprocess.run(
                ["rustup", "target", "list", "--installed"],
                capture_output=True,
                text=True,
                env=self.env,
            )

            if result.returncode == 0:
                targets = result.stdout.strip().split("\n")
                self._installed_targets = set(t for t in targets if t)
            else:
                self._installed_targets = set()

        except FileNotFoundError:
            self.logger.error("rustup not found. Please run --setup first.")
            self._installed_targets = set()

        return self._installed_targets

    def is_target_installed(self, rust_target: str) -> bool:
        """Check if a Rust target is installed."""
        return rust_target in self.get_installed_targets()

    def add_target(self, rust_target: str) -> bool:
        """Add a Rust target using rustup."""
        if self.is_target_installed(rust_target):
            self.logger.debug(f"Target {rust_target} is already installed")
            return True

        self.logger.info(f"Adding Rust target: {rust_target}")

        try:
            result = subprocess.run(
                ["rustup", "target", "add", rust_target],
                capture_output=True,
                text=True,
                env=self.env,
            )

            if result.returncode == 0:
                self.logger.success(f"Target {rust_target} added")
                # Invalidate cache
                self._installed_targets = None
                return True
            else:
                self.logger.error(f"Failed to add target: {result.stderr}")
                return False

        except FileNotFoundError:
            self.logger.error("rustup not found. Please run --setup first.")
            return False

    def resolve_targets(self, target_specs: List[str]) -> List[Target]:
        """
        Resolve target specifications to Target objects.

        Handles special values like:
        - "native" - current platform
        - "macos" - both macOS targets
        - "linux" - both Linux targets
        - "all" - all targets
        - "macos-arm64", "linux-x86_64" etc. - specific targets
        """
        resolved: List[Target] = []
        seen: Set[str] = set()

        for spec in target_specs:
            spec = spec.lower().strip()

            if spec == "native":
                # Add native target
                native_target = self._get_native_target()
                if native_target and native_target.rust_target not in seen:
                    resolved.append(native_target)
                    seen.add(native_target.rust_target)

            elif spec == "all":
                # Add all targets
                for rust_target in RUST_TARGETS.values():
                    if rust_target not in seen:
                        target = Target.from_rust_target(rust_target, self.config)
                        resolved.append(target)
                        seen.add(rust_target)

            elif spec == "macos":
                # Add both macOS targets
                for name, rust_target in RUST_TARGETS.items():
                    if "macos" in name and rust_target not in seen:
                        target = Target.from_rust_target(rust_target, self.config)
                        resolved.append(target)
                        seen.add(rust_target)

            elif spec == "linux":
                # Add both Linux targets
                for name, rust_target in RUST_TARGETS.items():
                    if "linux" in name and rust_target not in seen:
                        target = Target.from_rust_target(rust_target, self.config)
                        resolved.append(target)
                        seen.add(rust_target)

            elif spec in RUST_TARGETS:
                # Direct friendly name (e.g., "macos-arm64")
                rust_target = RUST_TARGETS[spec]
                if rust_target not in seen:
                    target = Target.from_rust_target(rust_target, self.config)
                    resolved.append(target)
                    seen.add(rust_target)

            elif spec in RUST_TARGETS.values():
                # Direct Rust target (e.g., "aarch64-apple-darwin")
                if spec not in seen:
                    target = Target.from_rust_target(spec, self.config)
                    resolved.append(target)
                    seen.add(spec)

            else:
                self.logger.warning(f"Unknown target specification: {spec}")

        return resolved

    def _get_native_target(self) -> Optional[Target]:
        """Get the native target for the current platform."""
        host_os = self.config.host_os
        host_arch = self.config.host_arch

        # Map architecture names (aarch64 <-> arm64)
        arch_aliases = {
            "aarch64": ["aarch64", "arm64"],
            "arm64": ["aarch64", "arm64"],
            "x86_64": ["x86_64"],
        }

        # Try direct match first
        native_key = f"{host_os}-{host_arch}"
        if native_key in RUST_TARGETS:
            return Target.from_rust_target(
                RUST_TARGETS[native_key], self.config
            )

        # Try with arch aliases
        for arch_name in arch_aliases.get(host_arch, [host_arch]):
            alias_key = f"{host_os}-{arch_name}"
            if alias_key in RUST_TARGETS:
                return Target.from_rust_target(
                    RUST_TARGETS[alias_key], self.config
                )

        # Try searching by components
        for name, rust_target in RUST_TARGETS.items():
            if host_os in name:
                for arch_name in arch_aliases.get(host_arch, [host_arch]):
                    if arch_name in name:
                        return Target.from_rust_target(rust_target, self.config)

        return None

    def ensure_targets(self, targets: List[Target]) -> bool:
        """Ensure all specified targets are installed."""
        success = True

        for target in targets:
            if not self.add_target(target.rust_target):
                success = False

        return success
