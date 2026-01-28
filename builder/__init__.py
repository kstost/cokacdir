"""
COKACDIR Rust Build System

A Python-based build system for cross-compiling Rust projects
to Linux and macOS platforms.
"""

from .config import BuildConfig, RUST_TARGETS, TARGET_NAMES
from .logger import Logger
from .tools import ToolInstaller
from .targets import Target, TargetManager
from .executor import BuildExecutor, BuildResult, run_build

__all__ = [
    "BuildConfig",
    "RUST_TARGETS",
    "TARGET_NAMES",
    "Logger",
    "ToolInstaller",
    "Target",
    "TargetManager",
    "BuildExecutor",
    "BuildResult",
    "run_build",
]

__version__ = "1.0.0"
