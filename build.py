#!/usr/bin/env python3
"""
COKACDIR Rust Build Script

Cross-compilation build system for Linux, macOS, and Windows platforms.
All build tools are installed locally in the builder/tools directory.

Usage:
    python build.py [options] [targets...]

Examples:
    python build.py                    # Build for current platform
    python build.py --macos            # Cross-compile for macOS
    python build.py --all              # Build for all platforms
    python build.py --setup            # Install all build tools
    python build.py --clean --all      # Clean and build all
"""

import argparse
import platform
import sys
from pathlib import Path

# Add builder directory to path
script_dir = Path(__file__).parent
sys.path.insert(0, str(script_dir))

from builder import BuildConfig, Logger, run_build
from builder.tools import ToolInstaller
from builder.config import RUST_TARGETS


def print_banner():
    """Print the build script banner."""
    print()
    print("=" * 50)
    print("  COKACDIR Rust Build Script")
    print("  Cross-Compilation Support")
    print("=" * 50)
    print()


def create_parser() -> argparse.ArgumentParser:
    """Create argument parser."""
    parser = argparse.ArgumentParser(
        description="COKACDIR Rust Build Script with Cross-Compilation Support",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Examples:
  %(prog)s                    Build for current platform
  %(prog)s --macos            Cross-compile for macOS (both architectures)
  %(prog)s --linux            Build for Linux (both architectures)
  %(prog)s --windows          Cross-compile for Windows (both architectures)
  %(prog)s --all              Build for all platforms (excluding Windows)
  %(prog)s --all --windows    Build for all platforms including Windows
  %(prog)s --setup            Install all build tools (Rust, zig, etc.)
  %(prog)s --status           Show status of installed tools
  %(prog)s --clean --all      Clean and build all platforms

Targets:
  native            Current platform (default)
  macos-arm64       macOS Apple Silicon (aarch64)
  macos-x86_64      macOS Intel (x86_64)
  linux-arm64       Linux ARM64
  linux-x86_64      Linux x86_64
  windows-x86_64    Windows x86_64
  windows-arm64     Windows ARM64

Note: All tools are installed locally in builder/tools/ directory.
""",
    )

    # Build mode
    mode_group = parser.add_argument_group("Build Mode")
    mode_group.add_argument(
        "--debug",
        action="store_true",
        help="Build in debug mode (faster compilation)",
    )
    mode_group.add_argument(
        "--release",
        action="store_true",
        help="Build in release mode (optimized)",
    )
    mode_group.add_argument(
        "--clean",
        action="store_true",
        help="Clean before building",
    )

    # Target selection
    target_group = parser.add_argument_group("Target Selection")
    target_group.add_argument(
        "--native",
        action="store_true",
        help="Build for current platform only (default)",
    )
    target_group.add_argument(
        "--macos",
        action="store_true",
        help="Build for both macOS targets (arm64 + x86_64)",
    )
    target_group.add_argument(
        "--macos-arm64",
        action="store_true",
        help="Build for macOS Apple Silicon",
    )
    target_group.add_argument(
        "--macos-x86_64",
        action="store_true",
        help="Build for macOS Intel",
    )
    target_group.add_argument(
        "--linux",
        action="store_true",
        help="Build for both Linux targets (arm64 + x86_64)",
    )
    target_group.add_argument(
        "--linux-arm64",
        action="store_true",
        help="Build for Linux ARM64",
    )
    target_group.add_argument(
        "--linux-x86_64",
        action="store_true",
        help="Build for Linux x86_64",
    )
    target_group.add_argument(
        "--windows",
        action="store_true",
        help="Build for both Windows targets (x86_64 + arm64)",
    )
    target_group.add_argument(
        "--windows-x86_64",
        action="store_true",
        help="Build for Windows x86_64",
    )
    target_group.add_argument(
        "--windows-arm64",
        action="store_true",
        help="Build for Windows ARM64",
    )
    target_group.add_argument(
        "--all",
        action="store_true",
        help="Build for all supported platforms",
    )

    # Setup
    setup_group = parser.add_argument_group("Setup")
    setup_group.add_argument(
        "--setup",
        action="store_true",
        help="Install all build tools (Rust, zig, cargo-zigbuild, macOS SDK)",
    )
    setup_group.add_argument(
        "--setup-rust",
        action="store_true",
        help="Install Rust toolchain only",
    )
    setup_group.add_argument(
        "--setup-cross",
        action="store_true",
        help="Install cross-compilation tools only (zig, cargo-zigbuild, SDK)",
    )
    setup_group.add_argument(
        "--setup-windows",
        action="store_true",
        help="Install Windows cross-compilation tools only (cargo-xwin)",
    )
    setup_group.add_argument(
        "--status",
        action="store_true",
        help="Show status of installed tools",
    )

    # Other options
    other_group = parser.add_argument_group("Other Options")
    other_group.add_argument(
        "--verbose",
        "-v",
        action="store_true",
        help="Enable verbose output",
    )
    other_group.add_argument(
        "--no-color",
        action="store_true",
        help="Disable colored output",
    )
    other_group.add_argument(
        "--no-auto-setup",
        action="store_true",
        help="Don't automatically install missing tools",
    )

    # Positional targets
    parser.add_argument(
        "targets",
        nargs="*",
        help="Additional targets to build (e.g., macos-arm64 linux-x86_64)",
    )

    return parser


def collect_targets(args: argparse.Namespace) -> list:
    """Collect all target specifications from arguments."""
    targets = []

    # Flag-based targets
    if args.all:
        targets.append("all")
    else:
        if args.macos:
            targets.append("macos")
        else:
            if args.macos_arm64:
                targets.append("macos-arm64")
            if args.macos_x86_64:
                targets.append("macos-x86_64")

        if args.linux:
            targets.append("linux")
        else:
            if args.linux_arm64:
                targets.append("linux-arm64")
            if args.linux_x86_64:
                targets.append("linux-x86_64")

    # ``--all`` intentionally excludes Windows, but the documented
    # ``--all --windows`` combination must still add the Windows targets.
    if args.windows:
        targets.append("windows")
    else:
        if args.windows_x86_64:
            targets.append("windows-x86_64")
        if args.windows_arm64:
            targets.append("windows-arm64")

    # Positional targets
    targets.extend(args.targets)

    # Default to native if nothing specified
    if not targets and not args.native:
        targets.append("native")
    elif args.native:
        targets.insert(0, "native")

    return targets


def ensure_rust_installed(tool_installer: ToolInstaller, logger: Logger, auto_setup: bool) -> bool:
    """Ensure Rust is installed with a default toolchain, install if needed and allowed."""
    if tool_installer.is_rust_installed():
        if not tool_installer._ensure_default_toolchain():
            logger.warning("Failed to configure default Rust toolchain")
        return True

    if not auto_setup:
        logger.error("Rust is not installed. Run with --setup or --setup-rust first.")
        logger.info("Or use --no-auto-setup=false to allow automatic installation.")
        return False

    logger.warning("Rust is not installed. Installing automatically...")
    logger.newline()

    return tool_installer.setup_rust()


def needs_cross_compilation(targets: list) -> bool:
    """Check if any target requires cross-compilation tools (zigbuild)."""
    for target in targets:
        target = target.lower()
        if target == "native":
            # On Linux, native builds also need zigbuild for GLIBC pinning
            if platform.system().lower() == "linux":
                return True
            continue
        if target in ("macos", "macos-arm64", "macos-x86_64", "all"):
            return True
        if target in ("linux", "linux-arm64", "linux-x86_64"):
            return True
        rust_target = RUST_TARGETS.get(target, target)
        if target in RUST_TARGETS or target in RUST_TARGETS.values():
            if "apple-darwin" in rust_target or "linux" in rust_target:
                return True
    return False


def needs_macos_cross(targets: list) -> bool:
    """Check if any target requires macOS SDK."""
    # Native macOS toolchains already contain an SDK. The downloaded SDK is
    # only used by osxcross/zig when producing macOS binaries on Linux.
    if platform.system().lower() != "linux":
        return False
    for target in targets:
        target = target.lower()
        if target in ("macos", "macos-arm64", "macos-x86_64", "all"):
            return True
        rust_target = RUST_TARGETS.get(target, target)
        if (target in RUST_TARGETS or target in RUST_TARGETS.values()) and "apple-darwin" in rust_target:
            return True
    return False


def needs_windows_cross(targets: list) -> bool:
    """Check if any target requires Windows cross-compilation tools (cargo-xwin)."""
    for target in targets:
        target = target.lower()
        if target in ("windows", "windows-x86_64", "windows-arm64"):
            return True
        rust_target = RUST_TARGETS.get(target, target)
        if (target in RUST_TARGETS or target in RUST_TARGETS.values()) and "windows" in rust_target:
            return True
    return False


def main() -> int:
    """Main entry point."""
    parser = create_parser()
    args = parser.parse_args()

    print_banner()

    # Create logger
    logger = Logger(
        use_color=not args.no_color,
        verbose=args.verbose,
    )

    # Get project root (directory containing this script)
    project_root = Path(__file__).parent.resolve()

    # Create config
    config = BuildConfig(
        release=not args.debug,
        clean=args.clean,
    )

    # Create tool installer
    tool_installer = ToolInstaller(config, project_root, logger)

    # Log host info
    logger.info(f"Host: {config.host_os}-{config.host_arch}")
    logger.info(f"Project: {project_root}")
    logger.info(f"Tools: {tool_installer.tools_dir}")
    logger.newline()

    # Status mode
    if args.status:
        tool_installer.print_status()
        return 0

    # Setup modes
    if args.setup:
        success = tool_installer.setup_all()
        return 0 if success else 1

    if args.setup_rust:
        success = tool_installer.setup_rust()
        return 0 if success else 1

    if args.setup_cross:
        # Ensure Rust is installed first
        if not ensure_rust_installed(tool_installer, logger, not args.no_auto_setup):
            return 1
        success = tool_installer.setup_cross_compile()
        return 0 if success else 1

    if args.setup_windows:
        # Ensure Rust is installed first
        if not ensure_rust_installed(tool_installer, logger, not args.no_auto_setup):
            return 1
        success = tool_installer.setup_windows_cross()
        return 0 if success else 1

    # Building mode - ensure Rust is installed
    auto_setup = not args.no_auto_setup
    if not ensure_rust_installed(tool_installer, logger, auto_setup):
        return 1

    # Collect targets
    targets = collect_targets(args)

    if not targets:
        logger.error("No targets specified")
        return 1

    # Check if cross-compilation is needed
    if needs_cross_compilation(targets):
        # Check zigbuild tools (zig + cargo-zigbuild) — needed for Linux and macOS targets
        missing_zig = not tool_installer.is_zig_installed()
        missing_zigbuild = not tool_installer.is_cargo_zigbuild_installed()
        # macOS SDK — only needed for macOS targets
        missing_sdk = needs_macos_cross(targets) and not tool_installer.is_macos_sdk_installed()

        if missing_zig or missing_zigbuild or missing_sdk:
            if auto_setup:
                logger.warning("Cross-compilation tools not installed. Installing...")
                logger.newline()
                if missing_sdk:
                    if not tool_installer.setup_cross_compile():
                        logger.error("Failed to install cross-compilation tools")
                        return 1
                else:
                    # Linux-only: install zig + cargo-zigbuild without macOS SDK
                    success = True
                    if missing_zig and not tool_installer.install_zig():
                        success = False
                    if missing_zigbuild and not tool_installer.install_cargo_zigbuild():
                        success = False
                    if not success:
                        logger.error("Failed to install cross-compilation tools")
                        return 1
            else:
                logger.error("Cross-compilation tools not installed.")
                logger.info("Run with --setup or --setup-cross first.")
                return 1

    # Check if Windows cross-compilation is needed
    if needs_windows_cross(targets):
        if not tool_installer.is_cargo_xwin_installed() or not tool_installer.is_clang_installed() or not tool_installer.is_lld_installed() or not tool_installer.is_llvm_lib_installed() or not tool_installer.is_clang_cl_installed():
            if auto_setup:
                logger.warning("Windows cross-compilation tools not installed. Installing...")
                logger.newline()
                if not tool_installer.setup_windows_cross():
                    logger.error("Failed to install Windows cross-compilation tools")
                    return 1
            else:
                logger.error("Windows cross-compilation tools not installed.")
                logger.info("Run with --setup or --setup-windows first.")
                return 1

    # Run build
    success = run_build(config, project_root, targets, logger)

    return 0 if success else 1


if __name__ == "__main__":
    sys.exit(main())
