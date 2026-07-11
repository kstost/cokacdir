"""
Tool installation and management for cross-compilation.
Installs Rust, zig, cargo-zigbuild, and macOS SDK into the builder/tools directory.
"""
import os
import posixpath
import shutil
import subprocess
import tarfile
import tempfile
from pathlib import Path
from pathlib import PurePosixPath
from typing import Optional, Tuple
import urllib.request
import ssl

from .config import BuildConfig
from .logger import Logger


class ToolInstaller:
    """Manages installation of build tools."""

    def __init__(self, config: BuildConfig, project_root: Path, logger: Logger):
        self.config = config
        self.project_root = project_root
        self.tools_dir = project_root / config.tools_dir
        self.logger = logger

        # Rust directories (local installation)
        self.cargo_home = self.tools_dir / "cargo"
        self.rustup_home = self.tools_dir / "rustup"

        # Specific tool directories
        self.zig_dir = self.tools_dir / f"zig-{config.zig_version}"
        self.sdk_dir = self.tools_dir / f"MacOSX{config.macos_sdk_version}.sdk"

    def ensure_tools_dir(self) -> None:
        """Create tools directory if it doesn't exist."""
        self.tools_dir.mkdir(parents=True, exist_ok=True)

    @staticmethod
    def _is_executable_file(path: Path) -> bool:
        return path.is_file() and os.access(path, os.X_OK)

    def _remove_managed_path(self, path: Path) -> bool:
        if not self._is_safe_path_for_deletion(path):
            self.logger.error(f"Refusing to delete unsafe path: {path}")
            return False
        if path.is_symlink() or path.is_file():
            path.unlink()
        else:
            shutil.rmtree(path)
        return True

    # ==================== Rust Installation ====================

    def get_cargo_path(self) -> Optional[Path]:
        """Get path to cargo executable."""
        # Check local installation first
        local_cargo = self.cargo_home / "bin" / "cargo"
        if self._is_executable_file(local_cargo):
            return local_cargo

        # Check system PATH with local env
        env = self._get_rust_env()
        try:
            result = subprocess.run(
                ["which", "cargo"],
                capture_output=True,
                text=True,
                env=env,
            )
            if result.returncode == 0:
                return Path(result.stdout.strip())
        except OSError:
            pass

        # Check system installation
        system_cargo = shutil.which("cargo")
        if system_cargo:
            return Path(system_cargo)

        return None

    def get_rustup_path(self) -> Optional[Path]:
        """Get path to rustup executable."""
        # Check local installation first
        local_rustup = self.cargo_home / "bin" / "rustup"
        if self._is_executable_file(local_rustup):
            return local_rustup

        # Check system installation
        system_rustup = shutil.which("rustup")
        if system_rustup:
            return Path(system_rustup)

        return None

    def is_rust_installed(self) -> bool:
        """Check if Rust is installed."""
        return self.get_cargo_path() is not None and self.get_rustup_path() is not None

    def _get_rust_env(self) -> dict:
        """Get environment variables for Rust operations."""
        env = os.environ.copy()
        env["CARGO_HOME"] = str(self.cargo_home)
        env["RUSTUP_HOME"] = str(self.rustup_home)

        # Add cargo bin to PATH
        cargo_bin = self.cargo_home / "bin"
        current_path = env.get("PATH", "")
        env["PATH"] = f"{cargo_bin}{os.pathsep}{current_path}"

        return env

    def install_rust(self) -> bool:
        """Install Rust toolchain into builder/tools directory."""
        if self.is_rust_installed():
            cargo_path = self.get_cargo_path()
            self.logger.success(f"Rust is already installed at {cargo_path}")
            return True

        self.ensure_tools_dir()
        self.logger.info("Installing Rust toolchain...")

        # Download rustup-init
        rustup_init_url = "https://sh.rustup.rs"
        rustup_init_path = None

        try:
            fd, rustup_init_name = tempfile.mkstemp(
                prefix="rustup-init-", suffix=".sh", dir=self.tools_dir
            )
            os.close(fd)
            rustup_init_path = Path(rustup_init_name)
            if not self.download_file(
                rustup_init_url, rustup_init_path, "rustup installer"
            ):
                return False

            rustup_init_path.chmod(0o755)

            # Prepare environment for installation
            env = self._get_rust_env()

            # Run rustup-init with options:
            # -y: don't prompt
            # --no-modify-path: don't modify shell profiles
            # --default-toolchain stable: install stable toolchain
            self.logger.info("Running rustup installer (this may take a while)...")

            result = subprocess.run(
                [
                    str(rustup_init_path),
                    "-y",
                    "--no-modify-path",
                    "--default-toolchain", "stable",
                ],
                env=env,
                capture_output=True,
                text=True,
            )

            if result.returncode == 0:
                cargo_path = self.cargo_home / "bin" / "cargo"
                rustup_path = self.cargo_home / "bin" / "rustup"
                if not self._is_executable_file(
                    cargo_path
                ) or not self._is_executable_file(rustup_path):
                    self.logger.error(
                        "Rust installer exited successfully but cargo/rustup was not installed"
                    )
                    return False

                cargo_version = subprocess.run(
                    [str(cargo_path), "--version"],
                    capture_output=True,
                    text=True,
                    env=env,
                )
                rustup_version = subprocess.run(
                    [str(rustup_path), "--version"],
                    capture_output=True,
                    text=True,
                    env=env,
                )
                if cargo_version.returncode != 0 or rustup_version.returncode != 0:
                    self.logger.error(
                        "Rust installer exited successfully but the installed tools do not run"
                    )
                    return False

                self.logger.success(f"Rust installed at {self.cargo_home}")
                self.logger.info(f"  {cargo_version.stdout.strip()}")
                return True
            else:
                self.logger.error(f"Rust installation failed: {result.stderr}")
                return False

        except Exception as e:
            self.logger.error(f"Failed to install Rust: {e}")
            return False

        finally:
            # Cleanup installer
            if rustup_init_path is not None and rustup_init_path.exists():
                rustup_init_path.unlink()

    # ==================== Zig Installation ====================

    def get_zig_path(self) -> Optional[Path]:
        """Get path to zig executable."""
        zig_exe = self.zig_dir / "zig"
        if self._is_executable_file(zig_exe):
            return zig_exe

        # Check if zig is in system PATH
        system_zig = shutil.which("zig")
        if system_zig:
            return Path(system_zig)

        return None

    def is_zig_installed(self) -> bool:
        """Check if zig is installed."""
        return self.get_zig_path() is not None

    def install_zig(self) -> bool:
        """Install zig compiler."""
        if self.is_zig_installed():
            zig_path = self.get_zig_path()
            self.logger.success(f"Zig is already installed at {zig_path}")
            return True

        self.ensure_tools_dir()

        # Download zig
        archive_name = f"zig-{self.config.host_os}-{self.config.host_arch}-{self.config.zig_version}.tar.xz"
        archive_path = self.tools_dir / archive_name
        extracted_dir = self.tools_dir / f"zig-{self.config.host_os}-{self.config.host_arch}-{self.config.zig_version}"

        if not archive_path.exists():
            local_cache = Path.home() / ".rustbuilder" / archive_name
            if local_cache.exists():
                self.logger.info(f"Using cached {archive_name} from {local_cache.parent}")
                if not self._copy_file_atomically(local_cache, archive_path):
                    return False
            elif not self.download_file(self.config.zig_url, archive_path, "Zig compiler"):
                return False

        # A process can stop after extraction but before the final rename. Reuse
        # a complete staged tree; replace an incomplete managed tree and retry.
        if extracted_dir.exists() or extracted_dir.is_symlink():
            staged_zig = extracted_dir / "zig"
            if not self._is_executable_file(staged_zig):
                if not self._remove_managed_path(extracted_dir):
                    return False
                if not self.extract_tar_xz(archive_path, self.tools_dir):
                    return False
        elif not self.extract_tar_xz(archive_path, self.tools_dir):
            return False

        # Rename to standard directory name
        if extracted_dir.exists() and extracted_dir != self.zig_dir:
            if self.zig_dir.exists() or self.zig_dir.is_symlink():
                if not self._remove_managed_path(self.zig_dir):
                    return False
            extracted_dir.rename(self.zig_dir)

        # Verify installation
        zig_exe = self.zig_dir / "zig"
        if zig_exe.is_file():
            try:
                zig_exe.chmod(0o755)
                version = subprocess.run(
                    [str(zig_exe), "version"], capture_output=True, text=True
                )
                if version.returncode == 0:
                    self.logger.success(f"Zig installed at {self.zig_dir}")
                    return True
            except OSError:
                pass
        self.logger.error("Zig installation failed - executable is missing or unusable")
        return False

    # ==================== cargo-zigbuild Installation ====================

    def is_cargo_zigbuild_installed(self) -> bool:
        """Check if cargo-zigbuild is installed."""
        # Check if cargo-zigbuild binary exists in cargo bin
        cargo_zigbuild = self.cargo_home / "bin" / "cargo-zigbuild"
        if self._is_executable_file(cargo_zigbuild):
            return True

        # Fallback: check if it's in PATH
        env = self.get_env()
        try:
            result = subprocess.run(
                ["cargo-zigbuild", "--version"],
                capture_output=True,
                text=True,
                env=env,
            )
            return result.returncode == 0
        except FileNotFoundError:
            return False

    def _ensure_default_toolchain(self) -> bool:
        """Ensure rustup has a default toolchain configured."""
        env = self.get_env()
        try:
            result = subprocess.run(
                ["rustup", "default"],
                capture_output=True,
                text=True,
                env=env,
            )
            if result.returncode != 0 or "no default" in result.stderr.lower() or "no default" in result.stdout.lower():
                self.logger.info("No default Rust toolchain configured. Setting up stable...")
                setup_result = subprocess.run(
                    ["rustup", "default", "stable"],
                    capture_output=True,
                    text=True,
                    env=env,
                )
                if setup_result.returncode != 0:
                    self.logger.error(f"Failed to set default toolchain: {setup_result.stderr}")
                    return False
                self.logger.success("Default Rust toolchain set to stable")
            return True
        except FileNotFoundError:
            return False

    def install_cargo_zigbuild(self) -> bool:
        """Install cargo-zigbuild."""
        if self.is_cargo_zigbuild_installed():
            self.logger.success("cargo-zigbuild is already installed")
            return True

        if not self.is_rust_installed():
            self.logger.error("Rust must be installed first")
            return False

        if not self._ensure_default_toolchain():
            return False

        self.logger.info("Installing cargo-zigbuild...")

        env = self.get_env()

        try:
            result = subprocess.run(
                ["cargo", "install", "cargo-zigbuild"],
                capture_output=True,
                text=True,
                env=env,
            )

            if result.returncode == 0 and self.is_cargo_zigbuild_installed():
                self.logger.success("cargo-zigbuild installed successfully")
                return True
            else:
                self.logger.error(
                    "Failed to install cargo-zigbuild or verify its executable: "
                    f"{result.stderr}"
                )
                return False

        except FileNotFoundError:
            self.logger.error("cargo not found. Please install Rust first.")
            return False

    # ==================== cargo-xwin Installation ====================

    def is_cargo_xwin_installed(self) -> bool:
        """Check if cargo-xwin is installed."""
        # Check if cargo-xwin binary exists in cargo bin
        cargo_xwin = self.cargo_home / "bin" / "cargo-xwin"
        if self._is_executable_file(cargo_xwin):
            return True

        # Fallback: check if it's in PATH
        env = self.get_env()
        try:
            result = subprocess.run(
                ["cargo-xwin", "--version"],
                capture_output=True,
                text=True,
                env=env,
            )
            return result.returncode == 0
        except FileNotFoundError:
            return False

    def install_cargo_xwin(self) -> bool:
        """Install cargo-xwin for Windows MSVC cross-compilation."""
        if self.is_cargo_xwin_installed():
            self.logger.success("cargo-xwin is already installed")
            return True

        if not self.is_rust_installed():
            self.logger.error("Rust must be installed first")
            return False

        if not self._ensure_default_toolchain():
            return False

        self.logger.info("Installing cargo-xwin...")

        env = self.get_env()

        try:
            result = subprocess.run(
                ["cargo", "install", "cargo-xwin"],
                capture_output=True,
                text=True,
                env=env,
            )

            if result.returncode == 0 and self.is_cargo_xwin_installed():
                self.logger.success("cargo-xwin installed successfully")
                return True
            else:
                self.logger.error(
                    "Failed to install cargo-xwin or verify its executable: "
                    f"{result.stderr}"
                )
                return False

        except FileNotFoundError:
            self.logger.error("cargo not found. Please install Rust first.")
            return False

    # ==================== clang/lld Detection ====================

    def is_clang_installed(self) -> bool:
        """Check if clang is installed."""
        try:
            result = subprocess.run(
                ["clang", "--version"],
                capture_output=True,
                text=True,
            )
            return result.returncode == 0
        except FileNotFoundError:
            return False

    def is_lld_installed(self) -> bool:
        """Check if lld (LLVM linker) is installed."""
        # Try lld-link first (used by cargo-xwin on some systems)
        for cmd in ["lld-link", "ld.lld", "lld"]:
            try:
                result = subprocess.run(
                    [cmd, "--version"],
                    capture_output=True,
                    text=True,
                )
                if result.returncode == 0:
                    return True
            except FileNotFoundError:
                continue
        return False

    def is_llvm_lib_installed(self) -> bool:
        """Check if llvm-lib is installed (needed by cargo-xwin for .lib generation)."""
        try:
            result = subprocess.run(
                ["llvm-lib", "--version"],
                capture_output=True,
                text=True,
            )
            return result.returncode == 0
        except FileNotFoundError:
            return False

    def is_clang_cl_installed(self) -> bool:
        """Check if clang-cl is installed (needed for Windows ARM64 cross-compilation)."""
        try:
            result = subprocess.run(
                ["clang-cl", "--version"],
                capture_output=True,
                text=True,
            )
            return result.returncode == 0
        except FileNotFoundError:
            return False

    # ==================== macOS SDK Installation ====================

    def is_macos_sdk_installed(self) -> bool:
        """Check if macOS SDK is installed."""
        return self.sdk_dir.is_dir() and (self.sdk_dir / "usr").is_dir()

    def install_macos_sdk(self) -> bool:
        """Install macOS SDK for cross-compilation."""
        if self.is_macos_sdk_installed():
            self.logger.success(f"macOS SDK is already installed at {self.sdk_dir}")
            return True

        # Only needed on Linux
        if self.config.host_os != "linux":
            self.logger.info("macOS SDK not needed on this platform")
            return True

        self.ensure_tools_dir()

        # Download SDK
        archive_name = f"MacOSX{self.config.macos_sdk_version}.sdk.tar.xz"
        archive_path = self.tools_dir / archive_name

        if not archive_path.exists():
            local_cache = Path.home() / ".rustbuilder" / archive_name
            if local_cache.exists():
                self.logger.info(f"Using cached {archive_name} from {local_cache.parent}")
                if not self._copy_file_atomically(local_cache, archive_path):
                    return False
            elif not self.download_file(
                self.config.macos_sdk_url, archive_path, "macOS SDK"
            ):
                return False

        if self.sdk_dir.exists() or self.sdk_dir.is_symlink():
            if not self._remove_managed_path(self.sdk_dir):
                return False

        # Extract SDK
        if not self.extract_tar_xz(archive_path, self.tools_dir):
            return False

        if self.is_macos_sdk_installed():
            self.logger.success(f"macOS SDK installed at {self.sdk_dir}")
            return True
        else:
            self.logger.error("macOS SDK installation failed")
            return False

    # ==================== Utility Methods ====================

    def _copy_file_atomically(self, source: Path, dest: Path) -> bool:
        """Copy a cached file without exposing a partial final path."""
        temp_path = None
        try:
            dest.parent.mkdir(parents=True, exist_ok=True)
            fd, temp_name = tempfile.mkstemp(
                prefix=f".{dest.name}.", suffix=".part", dir=dest.parent
            )
            temp_path = Path(temp_name)
            with os.fdopen(fd, "wb") as output:
                with source.open("rb") as input_file:
                    shutil.copyfileobj(input_file, output)
                    output.flush()
                    os.fsync(output.fileno())
            os.replace(temp_path, dest)
            temp_path = None
            return True
        except Exception as error:
            self.logger.error(f"Failed to copy cached file {source}: {error}")
            return False
        finally:
            if temp_path is not None:
                try:
                    temp_path.unlink()
                except FileNotFoundError:
                    pass

    def download_file(self, url: str, dest: Path, desc: str = "file") -> bool:
        """Download a file with progress indication."""
        self.logger.info(f"Downloading {desc}...")
        self.logger.info(f"  URL: {url}")

        temp_path = None
        temp_fd = None
        try:
            ctx = ssl.create_default_context()
            dest.parent.mkdir(parents=True, exist_ok=True)
            temp_fd, temp_name = tempfile.mkstemp(
                prefix=f".{dest.name}.", suffix=".part", dir=dest.parent
            )
            temp_path = Path(temp_name)

            with urllib.request.urlopen(url, context=ctx, timeout=120) as response:
                total_size = int(response.headers.get("content-length", 0))
                downloaded = 0
                chunk_size = 8192

                with os.fdopen(temp_fd, "wb") as f:
                    temp_fd = None
                    while True:
                        chunk = response.read(chunk_size)
                        if not chunk:
                            break
                        f.write(chunk)
                        downloaded += len(chunk)

                        if total_size > 0:
                            percent = (downloaded / total_size) * 100
                            mb_downloaded = downloaded / (1024 * 1024)
                            mb_total = total_size / (1024 * 1024)
                            print(
                                f"\r  Progress: {mb_downloaded:.1f}/{mb_total:.1f} MB ({percent:.1f}%)",
                                end="",
                                flush=True,
                            )

                    if downloaded == 0:
                        raise OSError("downloaded file is empty")
                    if total_size > 0 and downloaded != total_size:
                        raise OSError(
                            f"incomplete download: expected {total_size} bytes, "
                            f"received {downloaded}"
                        )
                    f.flush()
                    os.fsync(f.fileno())

            os.replace(temp_path, dest)
            temp_path = None

            print()  # New line after progress
            self.logger.success(f"Downloaded {desc}")
            return True

        except Exception as e:
            self.logger.error(f"Failed to download {desc}: {e}")
            return False
        finally:
            if temp_fd is not None:
                os.close(temp_fd)
            if temp_path is not None:
                try:
                    temp_path.unlink()
                except FileNotFoundError:
                    pass

    def _is_safe_path_for_deletion(self, path: Path) -> bool:
        """Check if a path is safe to delete (within tools_dir and not a symlink escape)."""
        try:
            # Resolve symlinks to get the real path
            resolved_path = path.resolve()
            tools_dir_resolved = self.tools_dir.resolve()

            # Ensure the resolved path is within tools_dir
            if not (str(resolved_path).startswith(str(tools_dir_resolved) + os.sep) or
                    resolved_path == tools_dir_resolved):
                return False

            # If the path is a symlink, also check that the target is within tools_dir
            if path.is_symlink():
                link_target = path.readlink()
                if link_target.is_absolute():
                    target_resolved = link_target.resolve()
                else:
                    target_resolved = (path.parent / link_target).resolve()

                if not (str(target_resolved).startswith(str(tools_dir_resolved) + os.sep) or
                        target_resolved == tools_dir_resolved):
                    return False

            return True
        except (ValueError, OSError):
            return False

    def _is_safe_tar_member(self, member: tarfile.TarInfo, dest_dir: Path) -> bool:
        """Check if a tar member is safe to extract (no path traversal)."""
        # Reject absolute paths
        if member.name.startswith('/'):
            return False

        # Reject paths with parent directory references
        if '..' in member.name.split('/'):
            return False

        # Resolve the final path and ensure it's within dest_dir
        try:
            dest_dir_resolved = dest_dir.resolve()
            member_path = (dest_dir / member.name).resolve()
            # Check if the resolved path is within the destination directory
            return str(member_path).startswith(str(dest_dir_resolved) + os.sep) or member_path == dest_dir_resolved
        except (ValueError, OSError):
            return False

    @staticmethod
    def _archive_parts(name: str):
        # Tar names are POSIX paths. Backslashes become separators on Windows
        # and drive/ADS colons have Windows-specific path semantics, so they
        # cannot safely be passed to the host Path implementation.
        if "\0" in name or "\\" in name:
            return None
        path = PurePosixPath(name)
        if path.is_absolute() or not path.parts or any(part == ".." for part in path.parts):
            return None
        if os.name == "nt" and any(":" in part for part in path.parts):
            return None
        return path.parts

    def _extract_members_safely(self, tar: tarfile.TarFile, stage_dir: Path) -> None:
        """Extract without ever writing through an archive-created symlink."""
        members = tar.getmembers()
        validated = []
        for member in members:
            parts = self._archive_parts(member.name)
            if parts is None:
                raise ValueError(f"unsafe archive path: {member.name}")
            if not (member.isdir() or member.isreg() or member.issym() or member.islnk()):
                raise ValueError(f"special archive entry is not allowed: {member.name}")

            if member.issym():
                if "\0" in member.linkname or "\\" in member.linkname:
                    raise ValueError(f"unsafe symlink target: {member.linkname}")
                if PurePosixPath(member.linkname).is_absolute():
                    raise ValueError(f"unsafe symlink target: {member.linkname}")
                combined = posixpath.normpath(
                    posixpath.join(posixpath.dirname(member.name), member.linkname)
                )
                if self._archive_parts(combined) is None:
                    raise ValueError(f"unsafe symlink target: {member.linkname}")
            elif member.islnk():
                if "\0" in member.linkname or "\\" in member.linkname:
                    raise ValueError(f"unsafe hardlink target: {member.linkname}")
                if self._archive_parts(posixpath.normpath(member.linkname)) is None:
                    raise ValueError(f"unsafe hardlink target: {member.linkname}")
            validated.append((member, parts))

        directory_modes = []
        links = []
        for member, parts in validated:
            target = stage_dir.joinpath(*parts)
            if member.isdir():
                target.mkdir(parents=True, exist_ok=True)
                directory_modes.append((target, member.mode & 0o777))
            elif member.isreg():
                target.parent.mkdir(parents=True, exist_ok=True)
                source = tar.extractfile(member)
                if source is None:
                    raise ValueError(f"could not read archive file: {member.name}")
                with source, target.open("xb") as output:
                    shutil.copyfileobj(source, output)
                target.chmod(member.mode & 0o777)
            else:
                links.append((member, target))

        # Links are created only after every regular file, so a later member can
        # never make extraction write through an earlier symlink.
        for member, target in links:
            target.parent.mkdir(parents=True, exist_ok=True)
            if target.exists() or target.is_symlink():
                raise ValueError(f"duplicate archive path: {member.name}")
            if member.issym():
                os.symlink(member.linkname, target)
            else:
                link_target = stage_dir.joinpath(*PurePosixPath(member.linkname).parts)
                resolved_target = link_target.resolve(strict=True)
                if stage_dir.resolve() not in (resolved_target, *resolved_target.parents):
                    raise ValueError(f"unsafe hardlink target: {member.linkname}")
                if not resolved_target.is_file() or link_target.is_symlink():
                    raise ValueError(f"invalid hardlink target: {member.linkname}")
                os.link(resolved_target, target)

        for directory, mode in sorted(
            directory_modes, key=lambda item: len(item[0].parts), reverse=True
        ):
            directory.chmod(mode)

    def extract_tar_xz(self, archive: Path, dest_dir: Path) -> bool:
        """Extract a .tar.xz archive with path traversal protection."""
        self.logger.info(f"Extracting {archive.name}...")
        stage_dir = None
        try:
            dest_dir.mkdir(parents=True, exist_ok=True)
            stage_dir = Path(tempfile.mkdtemp(prefix=".extract-", dir=dest_dir))
            with tarfile.open(archive, "r:xz") as tar:
                self._extract_members_safely(tar, stage_dir)

            children = list(stage_dir.iterdir())
            for child in children:
                destination = dest_dir / child.name
                if destination.exists() or destination.is_symlink():
                    raise FileExistsError(f"extraction destination exists: {destination}")
            moved = []
            try:
                for child in children:
                    destination = dest_dir / child.name
                    child.rename(destination)
                    moved.append((destination, child))
            except Exception:
                for destination, original in reversed(moved):
                    destination.rename(original)
                raise
            self.logger.success("Extraction complete")
            return True
        except Exception as e:
            self.logger.error(f"Failed to extract archive: {e}")
            return False
        finally:
            if stage_dir is not None:
                shutil.rmtree(stage_dir, ignore_errors=True)

    # ==================== Setup Methods ====================

    def setup_rust(self) -> bool:
        """Install Rust toolchain."""
        self.logger.header("Setting up Rust toolchain")
        return self.install_rust()

    def setup_cross_compile(self) -> bool:
        """Install all required tools for cross-compilation (zigbuild + macOS SDK)."""
        self.logger.header("Setting up cross-compilation tools")

        success = True

        if not self.install_zig():
            success = False

        if not self.install_cargo_zigbuild():
            success = False

        if not self.install_macos_sdk():
            success = False

        if success:
            self.logger.success("All cross-compilation tools installed!")
        else:
            self.logger.error("Some tools failed to install")

        return success

    def setup_windows_cross(self) -> bool:
        """Install tools for Windows cross-compilation (cargo-xwin + clang/lld check)."""
        self.logger.header("Setting up Windows cross-compilation tools")

        success = True

        if not self.install_cargo_xwin():
            success = False

        if not self.is_clang_installed():
            self.logger.warning("clang is not installed. Required for cargo-xwin.")
            self.logger.info("  Install with: apt install clang  (or your package manager)")
            success = False

        if not self.is_lld_installed():
            self.logger.warning("lld is not installed. Required for cargo-xwin.")
            self.logger.info("  Install with: apt install lld  (or your package manager)")
            success = False

        if not self.is_llvm_lib_installed():
            self.logger.warning("llvm-lib is not installed. Required for cargo-xwin.")
            self.logger.info("  Install with: apt install llvm  (or your package manager)")
            self.logger.info("  If llvm-lib-XX exists but llvm-lib doesn't, create a symlink:")
            self.logger.info("    sudo ln -s llvm-lib-18 /usr/bin/llvm-lib")
            success = False

        if not self.is_clang_cl_installed():
            self.logger.warning("clang-cl is not installed. Required for Windows ARM64 builds.")
            self.logger.info("  Install with: apt install clang-tools-18  (or matching version)")
            self.logger.info("  If clang-cl-XX exists but clang-cl doesn't, create a symlink:")
            self.logger.info("    sudo ln -s clang-cl-18 /usr/bin/clang-cl")
            success = False

        if success:
            self.logger.success("All Windows cross-compilation tools ready!")
        else:
            self.logger.error("Some Windows cross-compilation tools are missing")
            self.logger.info("  Or run: sudo ./install_windows_build_deps.sh")

        return success

    def setup_all(self) -> bool:
        """Install all required tools (Rust + cross-compilation). Use --setup-windows for Windows tools."""
        success = True

        # Install Rust first
        if not self.setup_rust():
            success = False
            return success  # Can't continue without Rust

        # Install cross-compilation tools (zigbuild + macOS SDK)
        if not self.setup_cross_compile():
            success = False

        return success

    def get_env(self) -> dict:
        """Get environment variables for build process."""
        env = os.environ.copy()

        # Set Rust environment
        env["CARGO_HOME"] = str(self.cargo_home)
        env["RUSTUP_HOME"] = str(self.rustup_home)

        # Build PATH with all tools
        path_parts = []

        # Add cargo bin
        cargo_bin = self.cargo_home / "bin"
        if cargo_bin.exists():
            path_parts.append(str(cargo_bin))

        # Add zig
        zig_path = self.get_zig_path()
        if zig_path:
            path_parts.append(str(zig_path.parent))

        # Add original PATH
        path_parts.append(env.get("PATH", ""))

        env["PATH"] = os.pathsep.join(path_parts)

        # Set SDKROOT for macOS cross-compilation
        if self.sdk_dir.exists():
            env["SDKROOT"] = str(self.sdk_dir)

        return env

    def print_status(self) -> None:
        """Print status of all tools."""
        self.logger.header("Tool Status")

        # Rust
        if self.is_rust_installed():
            cargo_path = self.get_cargo_path()
            self.logger.success(f"Rust: {cargo_path}")
        else:
            self.logger.warning("Rust: Not installed")

        # Zig
        if self.is_zig_installed():
            zig_path = self.get_zig_path()
            self.logger.success(f"Zig: {zig_path}")
        else:
            self.logger.warning("Zig: Not installed")

        # cargo-zigbuild
        if self.is_cargo_zigbuild_installed():
            self.logger.success("cargo-zigbuild: Installed")
        else:
            self.logger.warning("cargo-zigbuild: Not installed")

        # macOS SDK
        if self.is_macos_sdk_installed():
            self.logger.success(f"macOS SDK: {self.sdk_dir}")
        else:
            if self.config.host_os == "linux":
                self.logger.warning("macOS SDK: Not installed")
            else:
                self.logger.info("macOS SDK: Not needed on this platform")

        # cargo-xwin (Windows cross-compilation)
        if self.is_cargo_xwin_installed():
            self.logger.success("cargo-xwin: Installed")
        else:
            self.logger.warning("cargo-xwin: Not installed")

        # clang
        if self.is_clang_installed():
            self.logger.success("clang: Installed")
        else:
            self.logger.warning("clang: Not installed")

        # lld
        if self.is_lld_installed():
            self.logger.success("lld: Installed")
        else:
            self.logger.warning("lld: Not installed")

        # llvm-lib
        if self.is_llvm_lib_installed():
            self.logger.success("llvm-lib: Installed")
        else:
            self.logger.warning("llvm-lib: Not installed")

        # clang-cl
        if self.is_clang_cl_installed():
            self.logger.success("clang-cl: Installed")
        else:
            self.logger.warning("clang-cl: Not installed")
