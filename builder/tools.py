"""
Tool installation and management for cross-compilation.
Installs Rust, zig, cargo-zigbuild, and macOS SDK into the builder/tools directory.
"""
import os
import shutil
import subprocess
import tarfile
from pathlib import Path
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

    # ==================== Rust Installation ====================

    def get_cargo_path(self) -> Optional[Path]:
        """Get path to cargo executable."""
        # Check local installation first
        local_cargo = self.cargo_home / "bin" / "cargo"
        if local_cargo.exists():
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
        except:
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
        if local_rustup.exists():
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
        env["PATH"] = f"{cargo_bin}:{current_path}"

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
        rustup_init_path = self.tools_dir / "rustup-init.sh"

        try:
            self.logger.info("Downloading rustup installer...")
            ctx = ssl.create_default_context()

            with urllib.request.urlopen(rustup_init_url, context=ctx) as response:
                script_content = response.read()
                with open(rustup_init_path, "wb") as f:
                    f.write(script_content)

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
                self.logger.success(f"Rust installed at {self.cargo_home}")

                # Verify installation
                cargo_path = self.cargo_home / "bin" / "cargo"
                if cargo_path.exists():
                    # Get version
                    version_result = subprocess.run(
                        [str(cargo_path), "--version"],
                        capture_output=True,
                        text=True,
                        env=env,
                    )
                    if version_result.returncode == 0:
                        self.logger.info(f"  {version_result.stdout.strip()}")

                return True
            else:
                self.logger.error(f"Rust installation failed: {result.stderr}")
                return False

        except Exception as e:
            self.logger.error(f"Failed to install Rust: {e}")
            return False

        finally:
            # Cleanup installer
            if rustup_init_path.exists():
                rustup_init_path.unlink()

    # ==================== Zig Installation ====================

    def get_zig_path(self) -> Optional[Path]:
        """Get path to zig executable."""
        zig_exe = self.zig_dir / "zig"
        if zig_exe.exists():
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

        if not archive_path.exists():
            if not self.download_file(self.config.zig_url, archive_path, "Zig compiler"):
                return False

        # Extract to tools directory
        if not self.extract_tar_xz(archive_path, self.tools_dir):
            return False

        # Rename to standard directory name
        extracted_dir = self.tools_dir / f"zig-{self.config.host_os}-{self.config.host_arch}-{self.config.zig_version}"
        if extracted_dir.exists() and extracted_dir != self.zig_dir:
            if self.zig_dir.exists():
                shutil.rmtree(self.zig_dir)
            extracted_dir.rename(self.zig_dir)

        # Verify installation
        zig_exe = self.zig_dir / "zig"
        if zig_exe.exists():
            zig_exe.chmod(0o755)
            self.logger.success(f"Zig installed at {self.zig_dir}")
            return True
        else:
            self.logger.error("Zig installation failed - executable not found")
            return False

    # ==================== cargo-zigbuild Installation ====================

    def is_cargo_zigbuild_installed(self) -> bool:
        """Check if cargo-zigbuild is installed."""
        # Check if cargo-zigbuild binary exists in cargo bin
        cargo_zigbuild = self.cargo_home / "bin" / "cargo-zigbuild"
        if cargo_zigbuild.exists():
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

    def install_cargo_zigbuild(self) -> bool:
        """Install cargo-zigbuild."""
        if self.is_cargo_zigbuild_installed():
            self.logger.success("cargo-zigbuild is already installed")
            return True

        if not self.is_rust_installed():
            self.logger.error("Rust must be installed first")
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

            if result.returncode == 0:
                self.logger.success("cargo-zigbuild installed successfully")
                return True
            else:
                self.logger.error(f"Failed to install cargo-zigbuild: {result.stderr}")
                return False

        except FileNotFoundError:
            self.logger.error("cargo not found. Please install Rust first.")
            return False

    # ==================== macOS SDK Installation ====================

    def is_macos_sdk_installed(self) -> bool:
        """Check if macOS SDK is installed."""
        return self.sdk_dir.exists() and self.sdk_dir.is_dir()

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
            if not self.download_file(
                self.config.macos_sdk_url, archive_path, "macOS SDK"
            ):
                return False

        # Extract SDK
        if not self.extract_tar_xz(archive_path, self.tools_dir):
            return False

        if self.sdk_dir.exists():
            self.logger.success(f"macOS SDK installed at {self.sdk_dir}")
            return True
        else:
            self.logger.error("macOS SDK installation failed")
            return False

    # ==================== Utility Methods ====================

    def download_file(self, url: str, dest: Path, desc: str = "file") -> bool:
        """Download a file with progress indication."""
        self.logger.info(f"Downloading {desc}...")
        self.logger.info(f"  URL: {url}")

        try:
            ctx = ssl.create_default_context()

            with urllib.request.urlopen(url, context=ctx) as response:
                total_size = int(response.headers.get("content-length", 0))
                downloaded = 0
                chunk_size = 8192

                with open(dest, "wb") as f:
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

                print()  # New line after progress
                self.logger.success(f"Downloaded {desc}")
                return True

        except Exception as e:
            self.logger.error(f"Failed to download {desc}: {e}")
            if dest.exists():
                dest.unlink()
            return False

    def extract_tar_xz(self, archive: Path, dest_dir: Path) -> bool:
        """Extract a .tar.xz archive."""
        self.logger.info(f"Extracting {archive.name}...")
        try:
            with tarfile.open(archive, "r:xz") as tar:
                tar.extractall(path=dest_dir)
            self.logger.success("Extraction complete")
            return True
        except Exception as e:
            self.logger.error(f"Failed to extract archive: {e}")
            return False

    # ==================== Setup Methods ====================

    def setup_rust(self) -> bool:
        """Install Rust toolchain."""
        self.logger.header("Setting up Rust toolchain")
        return self.install_rust()

    def setup_cross_compile(self) -> bool:
        """Install all required tools for cross-compilation."""
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

    def setup_all(self) -> bool:
        """Install all required tools (Rust + cross-compilation)."""
        success = True

        # Install Rust first
        if not self.setup_rust():
            success = False
            return success  # Can't continue without Rust

        # Install cross-compilation tools
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

        env["PATH"] = ":".join(path_parts)

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
