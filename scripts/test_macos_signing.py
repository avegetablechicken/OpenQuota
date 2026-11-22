"""Unit tests for scripts/macos_signing.py."""

import contextlib
import io
from pathlib import Path
import subprocess
import sys
import unittest
from unittest import mock

SCRIPTS_DIR = str(Path(__file__).resolve().parent)
if SCRIPTS_DIR not in sys.path:
    sys.path.insert(0, SCRIPTS_DIR)

import macos_signing  # noqa: E402

SAMPLE_DEV_ID = "0123456789ABCDEF0123456789ABCDEF01234567"
SECOND_DEV_ID = "FEDCBA9876543210FEDCBA9876543210FEDCBA98"
OTHER_ID = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"


class MacosSigningTests(unittest.TestCase):
    def test_returns_matched_apple_development_identity(self) -> None:
        listing = (
            f'  1) {SAMPLE_DEV_ID} "Apple Development: Jane Doe (TEAM12345)"\n'
            "     1 valid identities found\n"
        )
        self.assertEqual(macos_signing.parse_signing_identity(listing), SAMPLE_DEV_ID)

    def test_ignores_non_matching_and_malformed_identities_and_falls_back(self) -> None:
        mixed_listing = (
            f'  1) {OTHER_ID} "Developer ID Application: Example Corp (TEAM12345)"\n'
            f'  2) {OTHER_ID} "Apple Distribution: Example Corp (TEAM12345)"\n'
            '  3) 0123456789ABCDEF "Apple Development: Short Hex (TEAM12345)"\n'
            f'  4) {SAMPLE_DEV_ID} "Apple Development: Valid User (TEAM12345)"\n'
        )
        self.assertEqual(macos_signing.parse_signing_identity(mixed_listing), SAMPLE_DEV_ID)

        non_matching_only = (
            f'  1) {OTHER_ID} "Developer ID Application: Example Corp (TEAM12345)"\n'
            f'  2) {OTHER_ID} "Apple Distribution: Example Corp (TEAM12345)"\n'
            '  3) 0123456789ABCDEF "Apple Development: Too Short (TEAM12345)"\n'
            '  4) ZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZ "Apple Development: Non Hex (TEAM12345)"\n'
        )
        self.assertEqual(macos_signing.parse_signing_identity(non_matching_only), "-")

    def test_multiple_apple_development_identities_selects_first(self) -> None:
        listing = (
            f'  1) {SAMPLE_DEV_ID} "Apple Development: First User (TEAM12345)"\n'
            f'  2) {SECOND_DEV_ID} "Apple Development: Second User (TEAM67890)"\n'
        )
        self.assertEqual(macos_signing.parse_signing_identity(listing), SAMPLE_DEV_ID)

    def test_empty_or_zero_identities_output_returns_dash(self) -> None:
        self.assertEqual(macos_signing.parse_signing_identity(""), "-")
        self.assertEqual(
            macos_signing.parse_signing_identity("     0 valid identities found\n"),
            "-",
        )

    def test_replacement_characters_in_certificate_name_still_match(self) -> None:
        listing = f'  1) {SAMPLE_DEV_ID} "Apple Development: Jane \ufffd Doe (TEAM12345)"\n'
        self.assertEqual(macos_signing.parse_signing_identity(listing), SAMPLE_DEV_ID)

    def test_called_process_error_falls_back_to_dash(self) -> None:
        def failing_runner(*_args, **_kwargs):
            raise subprocess.CalledProcessError(returncode=1, cmd=macos_signing.SECURITY_COMMAND)

        self.assertEqual(macos_signing.resolve_signing_identity(run_command=failing_runner), "-")

    def test_timeout_expired_falls_back_to_dash(self) -> None:
        def timing_out_runner(*_args, **_kwargs):
            raise subprocess.TimeoutExpired(cmd=macos_signing.SECURITY_COMMAND, timeout=5)

        self.assertEqual(macos_signing.resolve_signing_identity(run_command=timing_out_runner), "-")

    def test_os_error_falls_back_to_dash(self) -> None:
        def missing_binary_runner(*_args, **_kwargs):
            raise FileNotFoundError("No such file or directory: '/usr/bin/security'")

        self.assertEqual(macos_signing.resolve_signing_identity(run_command=missing_binary_runner), "-")

    def test_main_prints_dash_on_error_and_fingerprint_on_success(self) -> None:
        stdout_err = io.StringIO()
        with mock.patch(
            "subprocess.check_output",
            side_effect=subprocess.CalledProcessError(1, macos_signing.SECURITY_COMMAND),
        ) as patched_check_output:
            with contextlib.redirect_stdout(stdout_err):
                result = macos_signing.main()

        self.assertIsNone(result)
        self.assertEqual(stdout_err.getvalue(), "-\n")
        patched_check_output.assert_called_once_with(
            ["/usr/bin/security", "find-identity", "-v", "-p", "codesigning"],
            encoding="utf-8",
            errors="replace",
        )

        stdout_ok = io.StringIO()
        with mock.patch(
            "subprocess.check_output",
            return_value=f'  1) {SAMPLE_DEV_ID} "Apple Development: Jane Doe (TEAM12345)"\n',
        ) as patched_check_output:
            with contextlib.redirect_stdout(stdout_ok):
                result = macos_signing.main()

        self.assertIsNone(result)
        self.assertEqual(stdout_ok.getvalue(), f"{SAMPLE_DEV_ID}\n")
        patched_check_output.assert_called_once_with(
            ["/usr/bin/security", "find-identity", "-v", "-p", "codesigning"],
            encoding="utf-8",
            errors="replace",
        )


if __name__ == "__main__":
    unittest.main()
