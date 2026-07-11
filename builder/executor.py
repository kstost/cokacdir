"""
Build executor for Rust projects with cross-compilation support.
"""
import os
import shlex
import shutil
import stat
import subprocess
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import List, Optional, Tuple

from .config import BuildConfig
from .logger import Logger
from .targets import Target, TargetManager
from .tools import ToolInstaller


DISTRIBUTION_NOTICE_PATHS = (
    Path("LICENSE"),
    Path("THIRD_PARTY_NOTICES.md"),
    Path("LICENSES/OpenSSL-3.6.3.txt"),
)


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
                self.logger.error(f"cargo clean failed: {result.stderr}")
                return False

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
        if target.needs_xwin:
            cmd = ["cargo", "xwin", "build"]
        elif target.needs_zigbuild:
            cmd = ["cargo", "zigbuild"]
        else:
            cmd = ["cargo", "build"]

        # This is an application release build.  Resolve exactly the graph
        # reviewed and committed in Cargo.lock instead of silently updating it
        # (or selecting newer transitive crates) on a builder machine.
        cmd.append("--locked")

        # Add release flag
        if self.config.release:
            cmd.append("--release")

        # Add target (zigbuild Linux targets use .2.17 suffix for GLIBC compatibility)
        if target.needs_zigbuild and target.platform == "linux":
            cmd.extend(["--target", f"{target.rust_target}.2.17"])
        elif not target.is_native:
            cmd.extend(["--target", target.rust_target])

        # Get environment
        env = self.tool_installer.get_env()

        # cargo-xwin 0.21.x checks Path::exists() before replacing its cached
        # clang-cl symlink. A previous ARM64 build can leave that symlink
        # pointing at our now-removed temporary clang wrapper, and a broken
        # symlink reports exists() == false while still causing symlink() to
        # fail with EEXIST. Remove only that stale managed link up front.
        if target.needs_xwin:
            self._cleanup_xwin_clang_cl_symlink(env)

        # For Windows ARM64 cross-compilation, cargo-xwin passes /imsvc flags
        # (clang-cl syntax) via CFLAGS, but the ring crate uses plain clang
        # which doesn't understand /imsvc. A clang wrapper converts /imsvc to
        # -isystem so plain clang can process the MSVC include paths.
        clang_wrapper_dir = None
        if target.needs_xwin and "aarch64" in target.rust_target:
            clang_wrapper_dir = self._create_clang_wrapper()
            if clang_wrapper_dir:
                env["PATH"] = clang_wrapper_dir + os.pathsep + env.get("PATH", "")

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
                if binary_path is None:
                    message = (
                        f"Build command succeeded but no binary was found for "
                        f"{target.friendly_name}"
                    )
                    self.logger.error(message)
                    return BuildResult(
                        target=target,
                        success=False,
                        error_message=message,
                    )
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
        finally:
            if clang_wrapper_dir:
                # cargo-xwin may have cached clang-cl as a symlink to this
                # temporary wrapper. Remove the managed link before deleting
                # the wrapper so the next Windows build never sees it broken.
                self._cleanup_xwin_clang_cl_symlink(env, clang_wrapper_dir)
                shutil.rmtree(clang_wrapper_dir, ignore_errors=True)

    def _xwin_cache_dir(self, env: dict) -> Path:
        """Return cargo-xwin's cache directory and make it explicit in env."""
        configured = env.get("XWIN_CACHE_DIR")
        if configured:
            cache_dir = Path(configured).expanduser()
            if not cache_dir.is_absolute():
                cache_dir = self.project_root / cache_dir
        else:
            home = Path(env.get("HOME") or Path.home())
            if self.config.host_os == "macos":
                cache_root = home / "Library" / "Caches"
            else:
                cache_root = Path(env.get("XDG_CACHE_HOME") or home / ".cache")
            cache_dir = cache_root / "cargo-xwin"

        cache_dir = cache_dir.resolve(strict=False)
        env["XWIN_CACHE_DIR"] = str(cache_dir)
        return cache_dir

    def _cleanup_xwin_clang_cl_symlink(
        self, env: dict, wrapper_dir: Optional[str] = None
    ) -> None:
        """Remove a broken or temporary-wrapper cargo-xwin clang-cl symlink."""
        symlink = self._xwin_cache_dir(env) / "clang-cl"
        try:
            if not symlink.is_symlink():
                return

            should_remove = not symlink.exists()
            if wrapper_dir is not None and not should_remove:
                link_target = Path(os.readlink(symlink))
                if not link_target.is_absolute():
                    link_target = symlink.parent / link_target
                link_target = link_target.resolve(strict=False)
                wrapper_path = Path(wrapper_dir).resolve(strict=False)
                should_remove = (
                    link_target == wrapper_path or wrapper_path in link_target.parents
                )

            if should_remove:
                symlink.unlink()
        except OSError as error:
            self.logger.debug(
                f"Could not clean cargo-xwin clang-cl symlink {symlink}: {error}"
            )

    def _create_clang_wrapper(self) -> Optional[str]:
        """Create a clang wrapper that converts /imsvc to -isystem for plain clang."""
        try:
            clang_path = shutil.which("clang")
            if not clang_path:
                return None
            wrapper_dir = tempfile.mkdtemp(prefix="cokacdir-clang-xwin-")
            wrapper_path = os.path.join(wrapper_dir, "clang")
            clang_cl_wrapper_path = os.path.join(wrapper_dir, "clang-cl")

            wrapper_script = f"""#!/bin/bash
args=()
assembly_source=false
skip_next=false
for arg in "$@"; do
    case "$arg" in
        *.s|*.S) assembly_source=true ;;
    esac
    if $skip_next; then
        args+=("-isystem" "$arg")
        skip_next=false
    elif [ "$arg" = "/imsvc" ]; then
        skip_next=true
    else
        args+=("$arg")
    fi
done
extra_args=()
if $assembly_source; then
    # psm selects its preprocessed GNU AArch64 source while cross-compiling
    # Windows with clang-cl, but cc-rs omits the preprocessing flags because
    # it identifies the compiler as MSVC-like. Supply the equivalent target
    # defines so the source produces a Windows ARM64 COFF object.
    extra_args+=(
        "-x" "assembler-with-cpp"
        "-DCFG_TARGET_OS_windows"
        "-DCFG_TARGET_ARCH_aarch64"
        "-DCFG_TARGET_ENV_msvc"
    )
fi
exec {shlex.quote(clang_path)} "${{extra_args[@]}}" "${{args[@]}}"
"""
            with open(wrapper_path, "w") as f:
                f.write(wrapper_script)
            os.chmod(wrapper_path, stat.S_IRWXU)
            # cargo-xwin invokes `clang-cl`, and a system clang-cl earlier in
            # PATH would otherwise bypass the argument conversion entirely.
            os.symlink("clang", clang_cl_wrapper_path)
            return wrapper_dir
        except Exception:
            if "wrapper_dir" in locals():
                shutil.rmtree(wrapper_dir, ignore_errors=True)
            return None

    def _find_binary(self, target: Target) -> Optional[Path]:
        """Find the built binary."""
        profile = "release" if self.config.release else "debug"

        # Determine binary name (Windows targets produce .exe)
        binary_name = "cokacdir.exe" if target.platform == "windows" else "cokacdir"

        if target.is_native:
            binary_path = self.target_dir / profile / binary_name
        else:
            binary_path = self.target_dir / target.rust_target / profile / binary_name

        if binary_path.exists():
            return binary_path
        return None

    def copy_to_dist(self, results: List[BuildResult]) -> List[Tuple[Path, str]]:
        """Copy required notices and built binaries to the dist directory."""
        self.dist_dir.mkdir(parents=True, exist_ok=True)

        # A release binary must never be newly published without the license
        # material required by its statically bundled OpenSSL dependency.
        if not self._copy_distribution_notices():
            return []

        copied: List[Tuple[Path, str]] = []

        for result in results:
            if not result.success or not result.binary_path:
                continue

            # Determine destination name (Windows binaries keep .exe extension)
            if result.target.platform == "windows":
                dest_name = f"cokacdir-{result.target.friendly_name}.exe"
            else:
                dest_name = f"cokacdir-{result.target.friendly_name}"
            dest_path = self.dist_dir / dest_name
            temp_path = None

            try:
                if not result.binary_path.is_file():
                    raise FileNotFoundError(
                        f"built binary is not a regular file: {result.binary_path}"
                    )

                # Never copy directly over a previously published binary. A
                # short write (for example, a full disk) must leave it intact.
                fd, temp_name = tempfile.mkstemp(
                    prefix=f".{dest_name}.", suffix=".tmp", dir=self.dist_dir
                )
                temp_path = Path(temp_name)
                with os.fdopen(fd, "wb") as output:
                    with result.binary_path.open("rb") as source:
                        shutil.copyfileobj(source, output)
                        output.flush()
                        os.fsync(output.fileno())
                temp_path.chmod(0o755)
                os.replace(temp_path, dest_path)
                temp_path = None

                # Get file size
                size = dest_path.stat().st_size
                size_str = self._format_size(size)

                copied.append((dest_path, size_str))
                self.logger.debug(f"Copied {dest_path.name} ({size_str})")

            except Exception as e:
                self.logger.error(f"Failed to copy {result.binary_path}: {e}")
            finally:
                if temp_path is not None:
                    try:
                        temp_path.unlink()
                    except FileNotFoundError:
                        pass

        return copied

    def _copy_distribution_notices(self) -> bool:
        """Atomically copy required license material into the release tree."""
        for relative_path in DISTRIBUTION_NOTICE_PATHS:
            source_path = self.project_root / relative_path
            destination_path = self.dist_dir / relative_path
            temp_path = None

            try:
                if not source_path.is_file():
                    raise FileNotFoundError(
                        f"required distribution notice is missing: {source_path}"
                    )
                if source_path.stat().st_size == 0:
                    raise ValueError(
                        f"required distribution notice is empty: {source_path}"
                    )

                destination_path.parent.mkdir(parents=True, exist_ok=True)
                fd, temp_name = tempfile.mkstemp(
                    prefix=f".{destination_path.name}.",
                    suffix=".tmp",
                    dir=destination_path.parent,
                )
                temp_path = Path(temp_name)
                with os.fdopen(fd, "wb") as output:
                    with source_path.open("rb") as source:
                        shutil.copyfileobj(source, output)
                        output.flush()
                        os.fsync(output.fileno())
                temp_path.chmod(0o644)
                os.replace(temp_path, destination_path)
                temp_path = None
                self.logger.debug(f"Copied required notice: {relative_path}")
            except Exception as e:
                self.logger.error(f"Failed to copy required notice {relative_path}: {e}")
                return False
            finally:
                if temp_path is not None:
                    try:
                        temp_path.unlink()
                    except FileNotFoundError:
                        pass

        return True

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
                    "Zig is required for cross-compilation. Run with --setup first."
                )
                return []

            if not self.tool_installer.is_cargo_zigbuild_installed():
                self.logger.error(
                    "cargo-zigbuild is required for cross-compilation. Run with --setup first."
                )
                return []

        # Check if we need Windows cross-compilation tools
        needs_xwin = any(t.needs_xwin for t in targets)
        if needs_xwin:
            if not self.tool_installer.is_cargo_xwin_installed():
                self.logger.error(
                    "cargo-xwin is required for Windows cross-compilation. Run with --setup-windows first."
                )
                return []

            if not self.tool_installer.is_clang_installed():
                self.logger.error(
                    "clang is required for Windows cross-compilation. Install with: apt install clang"
                )
                return []

            if not self.tool_installer.is_lld_installed():
                self.logger.error(
                    "lld is required for Windows cross-compilation. Install with: apt install lld"
                )
                return []

            if not self.tool_installer.is_llvm_lib_installed():
                self.logger.error(
                    "llvm-lib is required for Windows cross-compilation. Install with: apt install llvm"
                )
                return []

            if not self.tool_installer.is_clang_cl_installed():
                self.logger.error(
                    "clang-cl is required for Windows ARM64 cross-compilation. Install with: apt install clang-tools-18"
                )
                return []

            self.logger.info(
                "Note: cargo-xwin will download MSVC CRT/SDK on first build (requires internet)"
            )

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
        if not executor.clean():
            return False

    # Resolve targets
    resolved_targets = target_manager.resolve_targets(targets)

    if not resolved_targets:
        logger.error("No valid targets specified")
        return False

    logger.info(f"Building for {len(resolved_targets)} target(s):")
    for target in resolved_targets:
        logger.target(target.friendly_name, target.rust_target)
    logger.newline()

    # Check if cross-compilation setup is needed (zigbuild for macOS/Linux)
    needs_zigbuild_setup = any(t.needs_zigbuild for t in resolved_targets)
    needs_macos = any(t.platform == "macos" for t in resolved_targets)
    if needs_zigbuild_setup:
        missing_zig = not tool_installer.is_zig_installed()
        missing_zigbuild = not tool_installer.is_cargo_zigbuild_installed()
        missing_sdk = needs_macos and not tool_installer.is_macos_sdk_installed()
        if missing_zig or missing_zigbuild or missing_sdk:
            logger.header("Cross-compilation Setup Required")
            if missing_sdk:
                if not tool_installer.setup_cross_compile():
                    return False
            else:
                success = True
                if missing_zig and not tool_installer.install_zig():
                    success = False
                if missing_zigbuild and not tool_installer.install_cargo_zigbuild():
                    success = False
                if not success:
                    return False
            logger.newline()

    # Check if Windows cross-compilation setup is needed
    needs_xwin_setup = any(t.needs_xwin for t in resolved_targets)
    if needs_xwin_setup:
        if not tool_installer.is_cargo_xwin_installed() or not tool_installer.is_clang_installed():
            logger.header("Windows Cross-compilation Setup Required")
            if not tool_installer.setup_windows_cross():
                return False
            logger.newline()

    # Build all targets
    results = executor.build_all(resolved_targets)

    # Copy to dist
    copied = []
    if any(r.success for r in results):
        copied = executor.copy_to_dist(results)
        logger.results(copied)

    # Empty/incomplete result sets and failed distribution copies are failures,
    # even though all([]) would otherwise report success.
    successful_results = [r for r in results if r.success]
    return (
        len(results) == len(resolved_targets)
        and bool(results)
        and all(r.success for r in results)
        and len(copied) == len(successful_results)
    )
