import io
import os
import re
import subprocess
import tarfile
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

from build import (
    collect_targets,
    create_parser,
    needs_cross_compilation,
    needs_macos_cross,
    needs_windows_cross,
)
from builder.config import BuildConfig
from builder.executor import (
    DISTRIBUTION_NOTICE_PATHS,
    BuildExecutor,
    BuildResult,
    run_build,
)
from builder.targets import Target
from builder.tools import ToolInstaller
from diffpr import _snapshot_lock, download_and_extract, update_snapshots


class BuildExecutorTests(unittest.TestCase):
    def setUp(self):
        self.temp_dir = tempfile.TemporaryDirectory()
        self.root = Path(self.temp_dir.name)
        self.config = BuildConfig()
        self.logger = mock.Mock()
        self.tools = mock.Mock()
        self.tools.get_env.return_value = {}
        self.manager = mock.Mock()
        for relative_path in DISTRIBUTION_NOTICE_PATHS:
            notice_path = self.root / relative_path
            notice_path.parent.mkdir(parents=True, exist_ok=True)
            notice_path.write_text(f"notice for {relative_path}\n", encoding="utf-8")
        self.executor = BuildExecutor(
            self.config, self.root, self.tools, self.manager, self.logger
        )
        self.target = Target(
            rust_target="x86_64-unknown-linux-gnu",
            friendly_name="linux-x86_64",
            platform="linux",
            arch="x86_64",
            is_native=True,
        )

    def tearDown(self):
        self.temp_dir.cleanup()

    @mock.patch("builder.executor.subprocess.run")
    def test_successful_command_without_binary_is_a_failed_build(self, run):
        run.return_value = SimpleNamespace(returncode=0, stdout="", stderr="")
        self.executor._find_binary = mock.Mock(return_value=None)

        result = self.executor.build_target(self.target)

        self.assertFalse(result.success)
        self.assertIn("no binary", result.error_message)

    @mock.patch("builder.executor.subprocess.run")
    def test_release_build_uses_committed_lockfile(self, run):
        run.return_value = SimpleNamespace(returncode=1, stdout="", stderr="failed")

        self.executor.build_target(self.target)

        self.assertIn("--locked", run.call_args.args[0])

    @mock.patch("builder.executor.subprocess.run")
    def test_xwin_build_removes_broken_cached_clang_cl_symlink(self, run):
        cache_dir = self.root / "xwin-cache"
        cache_dir.mkdir()
        clang_cl = cache_dir / "clang-cl"
        clang_cl.symlink_to(self.root / "missing-clang-wrapper")
        self.tools.get_env.return_value = {"XWIN_CACHE_DIR": str(cache_dir)}
        run.return_value = SimpleNamespace(returncode=1, stdout="", stderr="failed")
        target = Target(
            rust_target="x86_64-pc-windows-msvc",
            friendly_name="windows-x86_64",
            platform="windows",
            arch="x86_64",
            needs_xwin=True,
        )

        self.executor.build_target(target)

        self.assertFalse(clang_cl.is_symlink())

    @mock.patch("builder.executor.subprocess.run")
    def test_xwin_arm_build_cleans_cached_temporary_wrapper_symlink(self, run):
        cache_dir = self.root / "xwin-cache"
        cache_dir.mkdir()
        wrapper_dir = self.root / "clang-wrapper"
        wrapper_dir.mkdir()
        wrapper = wrapper_dir / "clang"
        wrapper.write_text("#!/bin/sh\n", encoding="utf-8")
        clang_cl = cache_dir / "clang-cl"
        self.tools.get_env.return_value = {"XWIN_CACHE_DIR": str(cache_dir)}
        target = Target(
            rust_target="aarch64-pc-windows-msvc",
            friendly_name="windows-aarch64",
            platform="windows",
            arch="aarch64",
            needs_xwin=True,
        )
        self.executor._create_clang_wrapper = mock.Mock(
            return_value=str(wrapper_dir)
        )

        def leave_wrapper_symlink(*_args, **_kwargs):
            clang_cl.symlink_to(wrapper)
            return SimpleNamespace(returncode=1, stdout="", stderr="failed")

        run.side_effect = leave_wrapper_symlink

        self.executor.build_target(target)

        self.assertFalse(clang_cl.is_symlink())
        self.assertFalse(wrapper_dir.exists())

    def test_arm_clang_wrapper_preprocesses_assembly_and_translates_imsvc(self):
        fake_clang = self.root / "real clang"
        fake_clang.write_text(
            '#!/bin/sh\nprintf "%s\\n" "$@"\n', encoding="utf-8"
        )
        fake_clang.chmod(0o755)

        with mock.patch(
            "builder.executor.shutil.which", return_value=str(fake_clang)
        ):
            wrapper_dir = self.executor._create_clang_wrapper()

        self.assertIsNotNone(wrapper_dir)
        try:
            result = subprocess.run(
                [
                    str(Path(wrapper_dir) / "clang-cl"),
                    "/imsvc",
                    "/sdk/include",
                    "source.s",
                ],
                capture_output=True,
                text=True,
                check=True,
            )
            args = result.stdout.splitlines()
            self.assertNotIn("/imsvc", args)
            self.assertIn("-isystem", args)
            self.assertIn("/sdk/include", args)
            self.assertIn("assembler-with-cpp", args)
            self.assertIn("-DCFG_TARGET_OS_windows", args)
            self.assertIn("-DCFG_TARGET_ARCH_aarch64", args)
            self.assertIn("-DCFG_TARGET_ENV_msvc", args)
        finally:
            if wrapper_dir is not None:
                import shutil

                shutil.rmtree(wrapper_dir, ignore_errors=True)

    @mock.patch("builder.executor.subprocess.run")
    def test_cargo_clean_failure_is_reported(self, run):
        run.return_value = SimpleNamespace(returncode=1, stdout="", stderr="failed")
        self.assertFalse(self.executor.clean())

    @mock.patch("builder.executor.BuildExecutor")
    @mock.patch("builder.executor.TargetManager")
    @mock.patch("builder.executor.ToolInstaller")
    def test_empty_build_results_are_not_success(
        self, tool_installer_cls, target_manager_cls, executor_cls
    ):
        tool_installer = tool_installer_cls.return_value
        tool_installer.get_env.return_value = {}
        target_manager_cls.return_value.resolve_targets.return_value = [self.target]
        executor_cls.return_value.build_all.return_value = []

        self.assertFalse(
            run_build(self.config, self.root, ["native"], self.logger)
        )

    @mock.patch("builder.executor.BuildExecutor")
    @mock.patch("builder.executor.TargetManager")
    @mock.patch("builder.executor.ToolInstaller")
    def test_distribution_copy_failure_is_not_success(
        self, tool_installer_cls, target_manager_cls, executor_cls
    ):
        tool_installer = tool_installer_cls.return_value
        tool_installer.get_env.return_value = {}
        target_manager_cls.return_value.resolve_targets.return_value = [self.target]
        result = BuildResult(self.target, True, self.root / "cokacdir")
        executor_cls.return_value.build_all.return_value = [result]
        executor_cls.return_value.copy_to_dist.return_value = []

        self.assertFalse(
            run_build(self.config, self.root, ["native"], self.logger)
        )

    @mock.patch("builder.executor.BuildExecutor")
    @mock.patch("builder.executor.TargetManager")
    @mock.patch("builder.executor.ToolInstaller")
    def test_requested_clean_failure_stops_build(
        self, tool_installer_cls, target_manager_cls, executor_cls
    ):
        self.config.clean = True
        tool_installer_cls.return_value.get_env.return_value = {}
        executor_cls.return_value.clean.return_value = False

        self.assertFalse(
            run_build(self.config, self.root, ["native"], self.logger)
        )
        executor_cls.return_value.build_all.assert_not_called()

    def test_distribution_copy_failure_preserves_existing_binary(self):
        source = self.root / "new-cokacdir"
        source.write_bytes(b"new binary")
        self.executor.dist_dir.mkdir()
        destination = self.executor.dist_dir / "cokacdir-linux-x86_64"
        destination.write_bytes(b"known good binary")
        result = BuildResult(self.target, True, source)

        def fail_after_partial_write(_source, output):
            output.write(b"partial")
            raise OSError("disk full")

        with mock.patch(
            "builder.executor.shutil.copyfileobj", side_effect=fail_after_partial_write
        ), mock.patch.object(
            self.executor, "_copy_distribution_notices", return_value=True
        ):
            self.assertEqual(self.executor.copy_to_dist([result]), [])

        self.assertEqual(destination.read_bytes(), b"known good binary")
        self.assertEqual(list(self.executor.dist_dir.glob("*.tmp")), [])

    def test_distribution_contains_required_license_material(self):
        source = self.root / "new-cokacdir"
        source.write_bytes(b"new binary")
        result = BuildResult(self.target, True, source)

        copied = self.executor.copy_to_dist([result])

        self.assertEqual(len(copied), 1)
        for relative_path in DISTRIBUTION_NOTICE_PATHS:
            self.assertEqual(
                (self.executor.dist_dir / relative_path).read_bytes(),
                (self.root / relative_path).read_bytes(),
            )
            self.assertEqual(
                (self.executor.dist_dir / relative_path).stat().st_mode & 0o777,
                0o644,
            )

    def test_missing_required_notice_prevents_binary_publication(self):
        (self.root / "LICENSES/OpenSSL-3.6.3.txt").unlink()
        source = self.root / "new-cokacdir"
        source.write_bytes(b"new binary")
        result = BuildResult(self.target, True, source)

        self.assertEqual(self.executor.copy_to_dist([result]), [])
        self.assertFalse(
            (self.executor.dist_dir / "cokacdir-linux-x86_64").exists()
        )

    def test_empty_required_notice_prevents_binary_publication(self):
        (self.root / "LICENSES/OpenSSL-3.6.3.txt").write_bytes(b"")
        source = self.root / "new-cokacdir"
        source.write_bytes(b"new binary")
        result = BuildResult(self.target, True, source)

        self.assertEqual(self.executor.copy_to_dist([result]), [])
        self.assertFalse(
            (self.executor.dist_dir / "cokacdir-linux-x86_64").exists()
        )

    def test_openssl_notice_version_matches_committed_lockfile(self):
        project_root = Path(__file__).resolve().parents[1]
        lockfile = (project_root / "Cargo.lock").read_text(encoding="utf-8")
        match = re.search(
            r'\[\[package\]\]\nname = "openssl-src"\nversion = "([^"]+)"',
            lockfile,
        )
        self.assertIsNotNone(match)
        crate_version = match.group(1)
        openssl_version = crate_version.split("+", 1)[1]
        notices = (project_root / "THIRD_PARTY_NOTICES.md").read_text(
            encoding="utf-8"
        )
        license_path = Path(f"LICENSES/OpenSSL-{openssl_version}.txt")

        self.assertIn(f"`openssl-src` {crate_version}", notices)
        self.assertIn(f"- Version: {openssl_version}", notices)
        self.assertIn(license_path, DISTRIBUTION_NOTICE_PATHS)
        self.assertIn(
            "Apache License",
            (project_root / license_path).read_text(encoding="utf-8"),
        )


class BuildArgumentTests(unittest.TestCase):
    def test_all_and_windows_flags_are_combined(self):
        args = create_parser().parse_args(["--all", "--windows"])
        self.assertEqual(collect_targets(args), ["all", "windows"])

    @mock.patch("build.platform.system", return_value="Linux")
    def test_direct_rust_target_triples_trigger_required_tool_checks(self, _system):
        self.assertTrue(needs_cross_compilation(["aarch64-apple-darwin"]))
        self.assertTrue(needs_macos_cross(["aarch64-apple-darwin"]))
        self.assertTrue(needs_windows_cross(["x86_64-pc-windows-msvc"]))


class TargetTests(unittest.TestCase):
    @mock.patch("builder.config.platform.system", return_value="Darwin")
    @mock.patch("builder.config.platform.machine", return_value="arm64")
    def test_macos_host_uses_zigbuild_for_linux_targets(self, _machine, _system):
        config = BuildConfig()
        target = Target.from_rust_target("x86_64-unknown-linux-gnu", config)
        self.assertTrue(target.needs_zigbuild)


class ToolExtractionTests(unittest.TestCase):
    @staticmethod
    def write_xz_tar(path, entries):
        with tarfile.open(path, mode="w:xz") as archive:
            for info, content in entries:
                archive.addfile(info, io.BytesIO(content) if content is not None else None)

    def test_safe_extractor_supports_internal_symlinks(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            archive = root / "safe.tar.xz"
            content = b"tool"
            file_info = tarfile.TarInfo("package/tool")
            file_info.size = len(content)
            link_info = tarfile.TarInfo("package/tool-link")
            link_info.type = tarfile.SYMTYPE
            link_info.linkname = "tool"
            self.write_xz_tar(archive, [(file_info, content), (link_info, None)])
            destination = root / "tools"
            installer = ToolInstaller(BuildConfig(), root, mock.Mock())

            self.assertTrue(installer.extract_tar_xz(archive, destination))
            self.assertEqual((destination / "package/tool-link").read_bytes(), content)

    def test_safe_extractor_rejects_symlink_write_through(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            archive = root / "unsafe.tar.xz"
            link_info = tarfile.TarInfo("package/link")
            link_info.type = tarfile.SYMTYPE
            link_info.linkname = "../../outside"
            payload = b"escape"
            payload_info = tarfile.TarInfo("package/link/payload")
            payload_info.size = len(payload)
            self.write_xz_tar(
                archive, [(link_info, None), (payload_info, payload)]
            )
            destination = root / "tools"
            installer = ToolInstaller(BuildConfig(), root, mock.Mock())

            self.assertFalse(installer.extract_tar_xz(archive, destination))
            self.assertFalse((root / "outside").exists())

    def test_safe_extractor_rejects_windows_separator_in_link_target(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            archive = root / "unsafe-windows-link.tar.xz"
            link_info = tarfile.TarInfo("package/link")
            link_info.type = tarfile.SYMTYPE
            link_info.linkname = r"..\..\outside"
            self.write_xz_tar(archive, [(link_info, None)])
            destination = root / "tools"
            installer = ToolInstaller(BuildConfig(), root, mock.Mock())

            self.assertFalse(installer.extract_tar_xz(archive, destination))
            self.assertFalse((root / "outside").exists())

    def test_windows_drive_and_ads_archive_names_are_rejected(self):
        with mock.patch("builder.tools.os.name", "nt"):
            self.assertIsNone(ToolInstaller._archive_parts("C:/outside"))
            self.assertIsNone(ToolInstaller._archive_parts("package/file:stream"))


class ToolDownloadTests(unittest.TestCase):
    @mock.patch("builder.tools.urllib.request.urlopen")
    def test_short_download_preserves_existing_archive(self, urlopen):
        class ShortResponse(io.BytesIO):
            headers = {"content-length": "10"}

        urlopen.return_value = ShortResponse(b"short")
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            destination = root / "tools/archive.tar.xz"
            destination.parent.mkdir()
            destination.write_bytes(b"known-good-cache")
            installer = ToolInstaller(BuildConfig(), root, mock.Mock())

            self.assertFalse(
                installer.download_file(
                    "https://example.invalid/archive", destination, "archive"
                )
            )
            self.assertEqual(destination.read_bytes(), b"known-good-cache")
            self.assertEqual(list(destination.parent.glob("*.part")), [])

    @mock.patch("builder.tools.subprocess.run")
    def test_rust_installer_success_without_tools_is_failure(self, run):
        run.return_value = SimpleNamespace(returncode=0, stdout="", stderr="")
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            installer = ToolInstaller(BuildConfig(), root, mock.Mock())
            installer.is_rust_installed = mock.Mock(return_value=False)
            installer.download_file = mock.Mock(return_value=True)

            self.assertFalse(installer.install_rust())

    def test_empty_sdk_directory_is_not_an_installed_sdk(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            installer = ToolInstaller(BuildConfig(), root, mock.Mock())
            installer.sdk_dir.mkdir(parents=True)

            self.assertFalse(installer.is_macos_sdk_installed())

    def test_failed_cached_copy_preserves_existing_archive(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            source = root / "cache.tar.xz"
            source.write_bytes(b"new archive")
            destination = root / "tools/archive.tar.xz"
            destination.parent.mkdir()
            destination.write_bytes(b"known-good-archive")
            installer = ToolInstaller(BuildConfig(), root, mock.Mock())

            def fail_after_partial_write(_source, output):
                output.write(b"partial")
                raise OSError("disk full")

            with mock.patch(
                "builder.tools.shutil.copyfileobj", side_effect=fail_after_partial_write
            ):
                self.assertFalse(
                    installer._copy_file_atomically(source, destination)
                )

            self.assertEqual(destination.read_bytes(), b"known-good-archive")
            self.assertEqual(list(destination.parent.glob("*.part")), [])

    @mock.patch("builder.tools.subprocess.run")
    def test_cargo_plugin_install_success_without_executable_is_failure(self, run):
        run.return_value = SimpleNamespace(returncode=0, stdout="", stderr="")
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            installer = ToolInstaller(BuildConfig(), root, mock.Mock())
            installer.is_rust_installed = mock.Mock(return_value=True)
            installer._ensure_default_toolchain = mock.Mock(return_value=True)
            installer.is_cargo_zigbuild_installed = mock.Mock(return_value=False)
            installer.is_cargo_xwin_installed = mock.Mock(return_value=False)

            self.assertFalse(installer.install_cargo_zigbuild())
            self.assertFalse(installer.install_cargo_xwin())


class ShellInstallerTests(unittest.TestCase):
    @staticmethod
    def _write_executable(path, content):
        path.write_text(content, encoding="utf-8")
        path.chmod(0o755)

    def _installer_fixture(self, root, script_name, curl_body):
        project_root = Path(__file__).resolve().parents[1]
        script = root / script_name
        script_text = (project_root / script_name).read_text(encoding="utf-8")
        script_text = script_text.replace(
            '"/usr/local/bin"', '"$INSTALL_TEST_SYSTEM_BIN"'
        )
        self._write_executable(script, script_text)

        fake_bin = root / "fake-bin"
        fake_bin.mkdir()
        self._write_executable(
            fake_bin / "uname",
            "#!/bin/bash\n"
            "case \"${1:-}\" in\n"
            "  -s) printf '%s\\n' Linux ;;\n"
            "  -m) printf '%s\\n' x86_64 ;;\n"
            "  *) exec /usr/bin/uname \"$@\" ;;\n"
            "esac\n",
        )
        self._write_executable(fake_bin / "curl", curl_body)

        home = root / "home"
        home.mkdir()
        env = os.environ.copy()
        env.update(
            {
                "HOME": str(home),
                "SHELL": "/bin/bash",
                "INSTALL_TEST_SYSTEM_BIN": str(root / "unavailable-system-bin"),
                "PATH": f"{fake_bin}:/usr/bin:/bin",
            }
        )
        return script, home, env

    def test_documented_windows_dependency_installer_is_executable(self):
        project_root = Path(__file__).resolve().parents[1]
        self.assertTrue(
            os.access(project_root / "install_windows_build_deps.sh", os.X_OK)
        )

    def test_installers_publish_mode_755_in_an_isolated_home(self):
        curl_body = (
            "#!/bin/bash\n"
            "url=''\n"
            "dest=''\n"
            "while [ \"$#\" -gt 0 ]; do\n"
            "  case \"$1\" in\n"
            "    http*) url=$1 ;;\n"
            "    -o|-O) shift; dest=$1 ;;\n"
            "  esac\n"
            "  shift\n"
            "done\n"
            "case \"$url\" in\n"
            "  */cokacdir-linux-x86_64|*/cokacctl-linux-x86_64) "
            "printf '\\177ELFtest-binary' > \"$dest\" ;;\n"
            "  */THIRD_PARTY_NOTICES.md) "
            "printf '# Third-Party Notices\\nOpenSSL 3.6.3\\n' > \"$dest\" ;;\n"
            "  */OpenSSL-3.6.3.txt) "
            "printf 'Apache License\\nVersion 2.0, January 2004\\n' > \"$dest\" ;;\n"
            "  */LICENSE) printf 'MIT License\\n' > \"$dest\" ;;\n"
            "  *) exit 22 ;;\n"
            "esac\n"
        )
        for script_name, binary_name in (
            ("install.sh", "cokacdir"),
            ("manage.sh", "cokacctl"),
        ):
            with self.subTest(script=script_name), tempfile.TemporaryDirectory() as temp:
                root = Path(temp)
                script, home, env = self._installer_fixture(
                    root, script_name, curl_body
                )
                result = subprocess.run(
                    [str(script)], capture_output=True, text=True, env=env
                )

                self.assertEqual(result.returncode, 0, result.stderr)
                installed = home / ".local/bin" / binary_name
                self.assertTrue(installed.is_file())
                self.assertEqual(installed.stat().st_mode & 0o777, 0o755)
                if script_name == "install.sh":
                    notice_dir = home / ".local/share/doc/cokacdir"
                    self.assertEqual(notice_dir.stat().st_mode & 0o777, 0o755)
                    self.assertEqual(
                        (notice_dir / "LICENSES").stat().st_mode & 0o777,
                        0o755,
                    )
                    for relative_path in DISTRIBUTION_NOTICE_PATHS:
                        installed_notice = notice_dir / relative_path
                        self.assertTrue(installed_notice.is_file())
                        self.assertEqual(
                            installed_notice.stat().st_mode & 0o777, 0o644
                        )

    def test_failed_download_preserves_installed_binary(self):
        curl_body = (
            "#!/bin/bash\n"
            "dest=''\n"
            "while [ \"$#\" -gt 0 ]; do\n"
            "  if [ \"$1\" = -o ] || [ \"$1\" = -O ]; then shift; dest=$1; fi\n"
            "  shift\n"
            "done\n"
            "printf partial > \"$dest\"\n"
            "exit 22\n"
        )
        for script_name, binary_name in (
            ("install.sh", "cokacdir"),
            ("manage.sh", "cokacctl"),
        ):
            with self.subTest(script=script_name), tempfile.TemporaryDirectory() as temp:
                root = Path(temp)
                script, home, env = self._installer_fixture(
                    root, script_name, curl_body
                )
                installed = home / ".local/bin" / binary_name
                installed.parent.mkdir(parents=True)
                installed.write_bytes(b"known-good-binary")
                installed.chmod(0o755)

                result = subprocess.run(
                    [str(script)], capture_output=True, text=True, env=env
                )

                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(installed.read_bytes(), b"known-good-binary")

    def test_required_notice_download_failure_preserves_installed_binary(self):
        curl_body = (
            "#!/bin/bash\n"
            "url=''\n"
            "dest=''\n"
            "while [ \"$#\" -gt 0 ]; do\n"
            "  case \"$1\" in\n"
            "    http*) url=$1 ;;\n"
            "    -o|-O) shift; dest=$1 ;;\n"
            "  esac\n"
            "  shift\n"
            "done\n"
            "case \"$url\" in\n"
            "  */cokacdir-linux-x86_64) printf '\\177ELFnew-binary' > \"$dest\" ;;\n"
            "  *) exit 22 ;;\n"
            "esac\n"
        )
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            script, home, env = self._installer_fixture(
                root, "install.sh", curl_body
            )
            installed = home / ".local/bin/cokacdir"
            installed.parent.mkdir(parents=True)
            installed.write_bytes(b"known-good-binary")
            installed.chmod(0o755)

            result = subprocess.run(
                [str(script)], capture_output=True, text=True, env=env
            )

            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(installed.read_bytes(), b"known-good-binary")

    def test_non_binary_response_preserves_installed_binary(self):
        curl_body = (
            "#!/bin/bash\n"
            "dest=''\n"
            "while [ \"$#\" -gt 0 ]; do\n"
            "  if [ \"$1\" = -o ] || [ \"$1\" = -O ]; then shift; dest=$1; fi\n"
            "  shift\n"
            "done\n"
            "printf '<html>not a binary</html>' > \"$dest\"\n"
        )
        for script_name, binary_name in (
            ("install.sh", "cokacdir"),
            ("manage.sh", "cokacctl"),
        ):
            with self.subTest(script=script_name), tempfile.TemporaryDirectory() as temp:
                root = Path(temp)
                script, home, env = self._installer_fixture(
                    root, script_name, curl_body
                )
                installed = home / ".local/bin" / binary_name
                installed.parent.mkdir(parents=True)
                installed.write_bytes(b"known-good-binary")
                installed.chmod(0o755)

                result = subprocess.run(
                    [str(script)], capture_output=True, text=True, env=env
                )

                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(installed.read_bytes(), b"known-good-binary")

    def test_installers_refuse_directory_at_binary_path(self):
        curl_body = (
            "#!/bin/bash\n"
            "dest=''\n"
            "while [ \"$#\" -gt 0 ]; do\n"
            "  if [ \"$1\" = -o ] || [ \"$1\" = -O ]; then shift; dest=$1; fi\n"
            "  shift\n"
            "done\n"
            "printf payload > \"$dest\"\n"
        )
        for script_name, binary_name in (
            ("install.sh", "cokacdir"),
            ("manage.sh", "cokacctl"),
        ):
            with self.subTest(script=script_name), tempfile.TemporaryDirectory() as temp:
                root = Path(temp)
                script, home, env = self._installer_fixture(
                    root, script_name, curl_body
                )
                install_path = home / ".local/bin" / binary_name
                install_path.mkdir(parents=True)

                result = subprocess.run(
                    [str(script)], capture_output=True, text=True, env=env
                )

                self.assertNotEqual(result.returncode, 0)
                self.assertTrue(install_path.is_dir())
                self.assertEqual(list(install_path.iterdir()), [])


class BuildWebScriptTests(unittest.TestCase):
    @staticmethod
    def _write_executable(path, content):
        path.write_text(content, encoding="utf-8")
        path.chmod(0o755)

    def _fixture(self, root):
        project_root = Path(__file__).resolve().parents[1]
        script = root / "buildweb.sh"
        script.write_bytes((project_root / "buildweb.sh").read_bytes())
        script.chmod(0o755)
        (root / "website").mkdir()
        fake_bin = root / "fake-bin"
        fake_bin.mkdir()
        marker = root / "npm-invoked"
        self._write_executable(
            fake_bin / "npm",
            "#!/bin/bash\n"
            f"touch {marker!s}\n"
            "if [ \"${1:-}\" = run ] && [ \"${2:-}\" = build ]; then\n"
            "  rm -rf dist\n"
            "  mkdir -p dist/assets\n"
            "  printf new-index > dist/index.html\n"
            "  printf new-asset > dist/assets/new.js\n"
            "  printf new-og > dist/ogimg.jpg\n"
            "fi\n",
        )
        env = os.environ.copy()
        env["PATH"] = f"{fake_bin}:/usr/bin:/bin"
        return script, fake_bin, marker, env

    def test_existing_build_lock_rejects_run_before_npm(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            script, _fake_bin, marker, env = self._fixture(root)
            (root / ".buildweb.lock").mkdir()

            result = subprocess.run(
                [str(script)], capture_output=True, text=True, env=env
            )

            self.assertEqual(result.returncode, 75)
            self.assertFalse(marker.exists())

    def test_publish_failure_restores_previous_site(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            script, fake_bin, _marker, env = self._fixture(root)
            (root / "assets").mkdir()
            (root / "assets/old.js").write_text("old-asset", encoding="utf-8")
            (root / "index.html").write_text("old-index", encoding="utf-8")
            (root / "ogimg.jpg").write_text("old-og", encoding="utf-8")
            self._write_executable(
                fake_bin / "mv",
                "#!/bin/bash\n"
                "args=(\"$@\")\n"
                "count=${#args[@]}\n"
                "src=${args[$((count-2))]}\n"
                "dest=${args[$((count-1))]}\n"
                "if [[ $src == */site/index.html && $dest == */index.html ]]; then\n"
                "  exit 1\n"
                "fi\n"
                "exec /bin/mv \"$@\"\n",
            )

            result = subprocess.run(
                [str(script)], capture_output=True, text=True, env=env
            )

            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(
                (root / "assets/old.js").read_text(encoding="utf-8"), "old-asset"
            )
            self.assertEqual(
                (root / "index.html").read_text(encoding="utf-8"), "old-index"
            )
            self.assertEqual(
                (root / "ogimg.jpg").read_text(encoding="utf-8"), "old-og"
            )
            self.assertFalse((root / ".buildweb.lock").exists())
            self.assertEqual(list(root.glob(".web-stage.*")), [])

    def test_success_keeps_previous_assets_for_cached_indexes(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            script, _fake_bin, _marker, env = self._fixture(root)
            (root / "assets").mkdir()
            (root / "assets/old.js").write_text("old-asset", encoding="utf-8")
            (root / "index.html").write_text("old-index", encoding="utf-8")

            result = subprocess.run(
                [str(script)], capture_output=True, text=True, env=env
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(
                (root / "assets/old.js").read_text(encoding="utf-8"), "old-asset"
            )
            self.assertEqual(
                (root / "assets/new.js").read_text(encoding="utf-8"), "new-asset"
            )
            self.assertEqual(
                (root / "index.html").read_text(encoding="utf-8"), "new-index"
            )


class DiffPrExtractionTests(unittest.TestCase):
    @staticmethod
    def make_tar(entries):
        buffer = io.BytesIO()
        with tarfile.open(fileobj=buffer, mode="w:gz") as archive:
            for info, content in entries:
                archive.addfile(info, io.BytesIO(content) if content is not None else None)
        return buffer.getvalue()

    @mock.patch("diffpr.urllib.request.urlopen")
    def test_extracts_regular_files_and_strips_top_level_directory(self, urlopen):
        info = tarfile.TarInfo("owner-repo-sha/src/main.txt")
        content = b"safe content"
        info.size = len(content)
        urlopen.return_value = io.BytesIO(self.make_tar([(info, content)]))
        with tempfile.TemporaryDirectory() as temp:
            destination = Path(temp) / "output"
            download_and_extract("https://example.invalid/archive", destination)
            self.assertEqual((destination / "src/main.txt").read_bytes(), content)

    @mock.patch("diffpr.urllib.request.urlopen")
    def test_rejects_symlink_escape_before_writing_any_destination(self, urlopen):
        link = tarfile.TarInfo("owner-repo-sha/link")
        link.type = tarfile.SYMTYPE
        link.linkname = "../../outside"
        payload = tarfile.TarInfo("owner-repo-sha/link/payload.txt")
        content = b"must not escape"
        payload.size = len(content)
        urlopen.return_value = io.BytesIO(
            self.make_tar([(link, None), (payload, content)])
        )
        with tempfile.TemporaryDirectory() as temp:
            destination = Path(temp) / "output"
            with self.assertRaises(ValueError):
                download_and_extract("https://example.invalid/archive", destination)
            self.assertFalse(destination.exists())

    @mock.patch("diffpr.urllib.request.urlopen")
    def test_rejects_hardlinks_and_special_files(self, urlopen):
        hardlink = tarfile.TarInfo("owner-repo-sha/hardlink")
        hardlink.type = tarfile.LNKTYPE
        hardlink.linkname = "owner-repo-sha/target"
        urlopen.return_value = io.BytesIO(self.make_tar([(hardlink, None)]))
        with tempfile.TemporaryDirectory() as temp:
            with self.assertRaises(ValueError):
                download_and_extract(
                    "https://example.invalid/archive", Path(temp) / "hardlink-output"
                )

        device = tarfile.TarInfo("owner-repo-sha/device")
        device.type = tarfile.CHRTYPE
        urlopen.return_value = io.BytesIO(self.make_tar([(device, None)]))
        with tempfile.TemporaryDirectory() as temp:
            with self.assertRaises(ValueError):
                download_and_extract(
                    "https://example.invalid/archive", Path(temp) / "device-output"
                )

    @mock.patch("diffpr.urllib.request.urlopen")
    def test_directory_modes_are_applied_children_first(self, urlopen):
        child = tarfile.TarInfo("owner-repo-sha/parent/child")
        child.type = tarfile.DIRTYPE
        child.mode = 0o700
        parent = tarfile.TarInfo("owner-repo-sha/parent")
        parent.type = tarfile.DIRTYPE
        parent.mode = 0
        urlopen.return_value = io.BytesIO(
            self.make_tar([(child, None), (parent, None)])
        )
        with tempfile.TemporaryDirectory() as temp, mock.patch(
            "diffpr.os.chmod"
        ) as chmod:
            download_and_extract(
                "https://example.invalid/archive", Path(temp) / "output"
            )

        chmod_paths = [Path(call.args[0]).name for call in chmod.call_args_list]
        self.assertEqual(chmod_paths, ["child", "parent"])

    @mock.patch("diffpr.download_and_extract")
    def test_concurrent_snapshot_update_is_rejected_before_download(self, extract):
        with tempfile.TemporaryDirectory() as temp:
            output = Path(temp) / "review"
            output.mkdir()
            with _snapshot_lock(output):
                with self.assertRaisesRegex(RuntimeError, "already using"):
                    update_snapshots("base", "head", output)
            extract.assert_not_called()

    @mock.patch("diffpr.download_and_extract")
    def test_snapshot_download_failure_preserves_both_existing_trees(self, extract):
        def side_effect(_url, destination):
            destination = Path(destination)
            if destination.name == "after":
                raise OSError("head download failed")
            destination.mkdir()
            (destination / "new.txt").write_text("new", encoding="utf-8")

        extract.side_effect = side_effect
        with tempfile.TemporaryDirectory() as temp:
            output = Path(temp) / "review"
            (output / "before").mkdir(parents=True)
            (output / "after").mkdir()
            (output / "before/old.txt").write_text("old-before", encoding="utf-8")
            (output / "after/old.txt").write_text("old-after", encoding="utf-8")

            with self.assertRaises(OSError):
                update_snapshots("base", "head", output)

            self.assertEqual(
                (output / "before/old.txt").read_text(encoding="utf-8"), "old-before"
            )
            self.assertEqual(
                (output / "after/old.txt").read_text(encoding="utf-8"), "old-after"
            )
            self.assertFalse(any(output.glob(".diffpr-snapshots-*")))

    @mock.patch("diffpr.download_and_extract")
    def test_snapshot_rollback_failure_preserves_recovery_backup(self, extract):
        def stage_snapshot(_url, destination):
            destination = Path(destination)
            destination.mkdir()
            (destination / "new.txt").write_text("new", encoding="utf-8")

        extract.side_effect = stage_snapshot
        with tempfile.TemporaryDirectory() as temp:
            output = Path(temp) / "review"
            (output / "before").mkdir(parents=True)
            (output / "after").mkdir()
            (output / "before/old.txt").write_text("old-before", encoding="utf-8")
            (output / "after/old.txt").write_text("old-after", encoding="utf-8")

            real_replace = __import__("os").replace

            def fail_publish_and_restore(source, destination):
                source_path = Path(source)
                destination_path = Path(destination)
                if source_path.name == "after" and source_path.parent.name.startswith(
                    ".diffpr-snapshots-"
                ):
                    raise OSError("after publish failed")
                if (
                    source_path.name == "after"
                    and source_path.parent.name == "backups"
                    and destination_path == output / "after"
                ):
                    raise OSError("after restore failed")
                return real_replace(source, destination)

            with mock.patch("diffpr.os.replace", side_effect=fail_publish_and_restore):
                with self.assertRaisesRegex(RuntimeError, "Recovery data is preserved"):
                    update_snapshots("base", "head", output)

            recovery_dirs = list(output.glob(".diffpr-snapshots-*"))
            self.assertEqual(len(recovery_dirs), 1)
            self.assertEqual(
                (recovery_dirs[0] / "backups/after/old.txt").read_text(
                    encoding="utf-8"
                ),
                "old-after",
            )
            self.assertEqual(
                (output / "before/old.txt").read_text(encoding="utf-8"),
                "old-before",
            )


if __name__ == "__main__":
    unittest.main()
