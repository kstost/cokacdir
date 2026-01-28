"""
Build executor for Rust projects with cross-compilation support.
"""
import os
import shutil
import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import List, Optional, Tuple

from .config import BuildConfig
from .logger import Logger
from .targets import Target, TargetManager
from .tools import ToolInstaller


@dataclass
class BuildResult:
    """Result of a build operation."""

    target: Target
    success: bool
    binary_path: Optional[Path] = None
    error_message: Optional[str] = None


class BuildExecutor:
    """Executes Rust builds with cross-compilation support."""

    def __init__(
        self,
        config: BuildConfig,
        project_root: Path,
        tool_installer: ToolInstaller,
        target_manager: TargetManager,
        logger: Logger,
    ):
        self.config = config
        self.project_root = project_root
        self.tool_installer = tool_installer
        self.target_manager = target_manager
        self.logger = logger

        self.dist_dir = project_root / config.dist_dir
        self.target_dir = project_root / "target"

    def clean(self) -> bool:
        """Clean build artifacts."""
        self.logger.info("Cleaning build artifacts...")

        try:
            # Run cargo clean with proper environment
            env = self.tool_installer.get_env()
            result = subprocess.run(
                ["cargo", "clean"],
                cwd=self.project_root,
                capture_output=True,
                text=True,
                env=env,
            )

            if result.returncode != 0:
                self.logger.warning(f"cargo clean failed: {result.stderr}")

            # Remove dist directory
            if self.dist_dir.exists():
                shutil.rmtree(self.dist_dir)
                self.logger.info(f"Removed {self.dist_dir}")

            self.logger.success("Clean complete")
            return True

        except Exception as e:
            self.logger.error(f"Clean failed: {e}")
            return False

    def build_target(self, target: Target) -> BuildResult:
        """Build for a specific target."""
        self.logger.info(f"Building for {target.friendly_name}...")

        # Determine build command
        if target.needs_zigbuild:
            cmd = ["cargo", "zigbuild"]
        else:
            cmd = ["cargo", "build"]

        # Add release flag
        if self.config.release:
            cmd.append("--release")

        # Add target
        if not target.is_native:
            cmd.extend(["--target", target.rust_target])

        # Get environment
        env = self.tool_installer.get_env()

        self.logger.debug(f"Running: {' '.join(cmd)}")

        try:
            result = subprocess.run(
                cmd,
                cwd=self.project_root,
                env=env,
                capture_output=True,
                text=True,
            )

            if result.returncode == 0:
                # Find the built binary
                binary_path = self._find_binary(target)
                self.logger.success(f"Built: {target.friendly_name}")

                return BuildResult(
                    target=target,
                    success=True,
                    binary_path=binary_path,
                )
            else:
                self.logger.error(f"Build failed for {target.friendly_name}")
                # Print stderr for debugging
                if result.stderr:
                    for line in result.stderr.split("\n")[:20]:
                        if line.strip():
                            self.logger.debug(f"  {line}")

                return BuildResult(
                    target=target,
                    success=False,
                    error_message=result.stderr,
                )

        except Exception as e:
            self.logger.error(f"Build failed: {e}")
            return BuildResult(
                target=target,
                success=False,
                error_message=str(e),
            )

    def _find_binary(self, target: Target) -> Optional[Path]:
        """Find the built binary."""
        profile = "release" if self.config.release else "debug"

        # Determine binary name (could be different on Windows)
        binary_name = "cokacdir"

        if target.is_native:
            binary_path = self.target_dir / profile / binary_name
        else:
            binary_path = self.target_dir / target.rust_target / profile / binary_name

        if binary_path.exists():
            return binary_path
        return None

    def copy_to_dist(self, results: List[BuildResult]) -> List[Tuple[Path, str]]:
        """Copy built binaries to dist directory."""
        self.dist_dir.mkdir(parents=True, exist_ok=True)

        copied: List[Tuple[Path, str]] = []

        for result in results:
            if not result.success or not result.binary_path:
                continue

            # Determine destination name
            dest_name = f"cokacdir-{result.target.friendly_name}"
            dest_path = self.dist_dir / dest_name

            try:
                shutil.copy2(result.binary_path, dest_path)
                dest_path.chmod(0o755)

                # Get file size
                size = dest_path.stat().st_size
                size_str = self._format_size(size)

                copied.append((dest_path, size_str))
                self.logger.debug(f"Copied {dest_path.name} ({size_str})")

            except Exception as e:
                self.logger.error(f"Failed to copy {result.binary_path}: {e}")

        return copied

    def _format_size(self, size: int) -> str:
        """Format file size in human-readable format."""
        for unit in ["B", "KB", "MB", "GB"]:
            if size < 1024:
                return f"{size:.1f}{unit}"
            size /= 1024
        return f"{size:.1f}TB"

    def build_all(self, targets: List[Target]) -> List[BuildResult]:
        """Build all specified targets."""
        results: List[BuildResult] = []

        # Ensure all targets are installed
        if not self.target_manager.ensure_targets(targets):
            self.logger.warning("Some targets could not be installed")

        # Check if we need cross-compilation tools
        needs_zigbuild = any(t.needs_zigbuild for t in targets)
        if needs_zigbuild:
            if not self.tool_installer.is_zig_installed():
                self.logger.error(
                    "Zig is required for macOS cross-compilation. Run with --setup first."
                )
                return []

            if not self.tool_installer.is_cargo_zigbuild_installed():
                self.logger.error(
                    "cargo-zigbuild is required for macOS cross-compilation. Run with --setup first."
                )
                return []

        # Build each target
        total = len(targets)
        for i, target in enumerate(targets, 1):
            self.logger.step(i, total, f"Building {target.friendly_name}")
            result = self.build_target(target)
            results.append(result)

        return results


def run_build(
    config: BuildConfig,
    project_root: Path,
    targets: List[str],
    logger: Logger,
) -> bool:
    """
    Main entry point for running builds.

    Args:
        config: Build configuration
        project_root: Path to project root
        targets: List of target specifications
        logger: Logger instance

    Returns:
        True if all builds succeeded
    """
    tool_installer = ToolInstaller(config, project_root, logger)
    # Pass environment to target manager so rustup uses correct paths
    target_manager = TargetManager(config, logger, env=tool_installer.get_env())
    executor = BuildExecutor(
        config, project_root, tool_installer, target_manager, logger
    )

    # Clean if requested
    if config.clean:
        executor.clean()

    # Resolve targets
    resolved_targets = target_manager.resolve_targets(targets)

    if not resolved_targets:
        logger.error("No valid targets specified")
        return False

    logger.info(f"Building for {len(resolved_targets)} target(s):")
    for target in resolved_targets:
        logger.target(target.friendly_name, target.rust_target)
    logger.newline()

    # Check if cross-compilation setup is needed
    needs_setup = any(t.needs_zigbuild for t in resolved_targets)
    if needs_setup:
        if not tool_installer.is_zig_installed() or not tool_installer.is_macos_sdk_installed():
            logger.header("Cross-compilation Setup Required")
            if not tool_installer.setup_all():
                return False
            logger.newline()

    # Build all targets
    results = executor.build_all(resolved_targets)

    # Copy to dist
    if any(r.success for r in results):
        copied = executor.copy_to_dist(results)
        logger.results(copied)

    # Return success if all builds passed
    return all(r.success for r in results)
