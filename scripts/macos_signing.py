"""Print a local Apple Development certificate fingerprint or '-' for ad-hoc signing."""

import re
import subprocess


def main():
    listing = subprocess.check_output(
        ["/usr/bin/security", "find-identity", "-v", "-p", "codesigning"], text=True
    )
    match = re.search(r'^\s*\d+\)\s+([0-9a-fA-F]{40})\s+"Apple Development:', listing, re.MULTILINE)
    print(match.group(1) if match else "-")


if __name__ == "__main__":
    main()
