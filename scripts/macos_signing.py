"""Print a local Apple Development certificate fingerprint or '-' for ad-hoc signing."""

import re
import subprocess

SECURITY_COMMAND = ["/usr/bin/security", "find-identity", "-v", "-p", "codesigning"]
IDENTITY_PATTERN = re.compile(
    r'^\s*\d+\)\s+([0-9a-fA-F]{40})\s+"Apple Development:',
    re.MULTILINE,
)


def parse_signing_identity(listing: str) -> str:
    match = IDENTITY_PATTERN.search(listing)
    return match.group(1) if match else "-"


def resolve_signing_identity(run_command=None) -> str:
    if run_command is None:
        run_command = subprocess.check_output
    try:
        listing = run_command(
            SECURITY_COMMAND,
            encoding="utf-8",
            errors="replace",
        )
    except (OSError, subprocess.SubprocessError):
        return "-"
    return parse_signing_identity(listing or "")


def main() -> None:
    print(resolve_signing_identity())


if __name__ == "__main__":
    main()
