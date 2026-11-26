#!/usr/bin/env python3
"""Sign the verified official archive checksums with an Ed25519 key."""

import hashlib
import os
from pathlib import Path
import re
import subprocess
from tempfile import TemporaryDirectory


ARCHIVES = ("TackleCast-Windows-x86_64.zip", "TackleCast-Linux-x86_64.tar.gz")
ED25519_SPKI = bytes.fromhex("302a300506032b6570032100")


def sign_updates(assets, repo, tag, public_key, private_key):
    if not re.fullmatch(r"[a-zA-Z0-9_.-]+/[a-zA-Z0-9_.-]+", repo):
        raise SystemExit("Expected an owner/repo update repository")
    if not re.fullmatch(r"v[0-9]+\.[0-9]+\.[0-9]+(?:-[a-zA-Z0-9.-]+)?", tag):
        raise SystemExit("Expected a version tag")
    if not re.fullmatch(r"[a-fA-F0-9]{64}", public_key) or not private_key:
        raise SystemExit("Configure both the Ed25519 update public key and private signing key")

    with TemporaryDirectory(prefix="tacklecast-sign-") as tmp:
        key = Path(tmp) / "key.pem"
        key.write_text(private_key.strip() + "\n", encoding="ascii")
        public_der = subprocess.check_output(
            ["openssl", "pkey", "-in", key, "-pubout", "-outform", "DER"]
        )
        if public_der != ED25519_SPKI + bytes.fromhex(public_key):
            raise SystemExit("The Ed25519 signing key does not match the configured public key")
        message = Path(tmp) / "message"
        for archive in ARCHIVES:
            checksum = assets / f"{archive}.sha256"
            with (assets / archive).open("rb") as stream:
                digest = hashlib.file_digest(stream, "sha256").hexdigest()
            if checksum.read_text(encoding="ascii").split() != [digest, archive]:
                raise SystemExit(f"Checksum does not match {archive}")
            message.write_bytes(
                f"tacklecast-update-v1\n{repo.lower()}\n{tag}\n{archive}\n{digest}\n".encode("ascii")
            )
            subprocess.run(
                ["openssl", "pkeyutl", "-sign", "-rawin", "-inkey", key, "-in", message,
                 "-out", checksum.with_suffix(".sha256.sig")],
                check=True,
            )


if __name__ == "__main__":
    public_key = os.environ.get("TACKLECAST_UPDATE_PUBLIC_KEY", "").strip()
    private_key = os.environ.get("TACKLECAST_UPDATE_SIGNING_KEY", "")
    if public_key or private_key:
        sign_updates(
            Path("release-assets"),
            os.environ["TACKLECAST_SIGN_REPO"],
            os.environ["TACKLECAST_SIGN_TAG"],
            public_key,
            private_key,
        )
    else:
        print("No update signing key is configured. Releases remain available for manual download.")
