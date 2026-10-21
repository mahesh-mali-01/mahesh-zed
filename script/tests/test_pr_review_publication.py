#!/usr/bin/env python3
"""Isolated Git history regressions for the publication guard."""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SCANNER = Path(__file__).resolve().parents[1] / "check-pr-review-publication"


class PublicationTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.repository = Path(self.temporary.name)
        self.environment = dict(os.environ)
        self.environment.update({
            "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": os.devnull,
            "GIT_AUTHOR_NAME": "Contributor", "GIT_AUTHOR_EMAIL": "developer@example.test",
            "GIT_COMMITTER_NAME": "Contributor", "GIT_COMMITTER_EMAIL": "developer@example.test",
        })
        self.git("init", "-q")
        self.write("README.md", b"Upstream baseline\n")
        self.commit("Base")
        self.base = self.git("rev-parse", "HEAD").strip().decode()
        self.denylist = self.repository / "denylist.json"
        self.denylist.write_text(json.dumps([{"name": "private-context", "pattern": "CONFIDENTIAL_CONTEXT"}]))

    def git(self, *arguments):
        return subprocess.check_output(["git", *arguments], cwd=self.repository, env=self.environment, stderr=subprocess.PIPE)

    def write(self, path, content):
        target = self.repository / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(content)

    def commit(self, message):
        # Keep the external denylist outside the tracked publication tree.
        self.git("add", "-A", "--", ".", ":!denylist.json")
        self.git("commit", "-q", "-m", message)

    def scan(self, *extra):
        result = subprocess.run(
            [str(SCANNER), "--base", self.base, "--head", "HEAD", "--denylist", str(self.denylist), *extra],
            cwd=self.repository, env=self.environment, capture_output=True, text=True,
        )
        self.assertNotIn("CONFIDENTIAL_CONTEXT", result.stdout + result.stderr)
        return result

    def test_safe_tree(self):
        self.write("src/feature.rs", b"pub fn feature() {}\n")
        self.commit("Add feature")
        self.assertEqual(self.scan().returncode, 0)

    def test_deleted_historical_leak(self):
        self.write("notes.txt", b"CONFIDENTIAL_CONTEXT")
        self.commit("Temporary notes")
        (self.repository / "notes.txt").unlink()
        self.commit("Remove notes")
        self.assertEqual(self.scan().returncode, 1)

    def test_identity_allowlist_is_header_only(self):
        self.environment["GIT_AUTHOR_EMAIL"] = "CONFIDENTIAL_CONTEXT@example.test"
        self.environment["GIT_COMMITTER_EMAIL"] = "CONFIDENTIAL_CONTEXT@example.test"
        self.write("feature", b"safe")
        self.commit("Add feature")
        self.assertEqual(self.scan().returncode, 1)
        self.assertEqual(self.scan("--allow-email", "CONFIDENTIAL_CONTEXT@example.test").returncode, 0)
        self.write("feature", b"CONFIDENTIAL_CONTEXT@example.test")
        self.commit("Update feature")
        self.assertEqual(self.scan("--allow-email", "CONFIDENTIAL_CONTEXT@example.test").returncode, 1)

    def test_allowed_email_does_not_hide_author_name(self):
        self.environment["GIT_AUTHOR_NAME"] = "CONFIDENTIAL_CONTEXT"
        self.write("feature", b"safe")
        self.commit("Add feature")
        self.assertEqual(self.scan("--allow-email", "developer@example.test").returncode, 1)

    def test_new_alias_of_upstream_blob_still_checks_path(self):
        self.git("mv", "README.md", "CONFIDENTIAL_CONTEXT")
        self.commit("Move upstream content")
        self.assertEqual(self.scan().returncode, 1)

    def test_commit_body_and_trailers_not_allowlisted(self):
        self.write("feature", b"safe")
        self.commit("Add feature\n\nCo-authored-by: Person <CONFIDENTIAL_CONTEXT@example.test>")
        self.assertEqual(self.scan("--allow-email", "CONFIDENTIAL_CONTEXT@example.test").returncode, 1)

    def test_binary_and_non_utf8_bytes(self):
        self.write("fixture.bin", b"\x00\xffCONFIDENTIAL_CONTEXT\xfe\x00")
        self.commit("Add binary")
        self.assertEqual(self.scan().returncode, 1)

    def test_deleted_historical_path_alias_is_checked(self):
        self.write("safe", b"same bytes")
        self.commit("Add safe")
        self.git("mv", "safe", "CONFIDENTIAL_CONTEXT")
        self.commit("Rename")
        self.git("mv", "CONFIDENTIAL_CONTEXT", "safe")
        self.commit("Restore path")
        self.assertEqual(self.scan().returncode, 1)

    def test_local_evidence_rejected_even_if_later_deleted(self):
        self.write(".local/evidence.txt", b"safe bytes")
        self.commit("Add evidence")
        (self.repository / ".local/evidence.txt").unlink()
        self.commit("Remove evidence")
        result = self.scan()
        self.assertEqual(result.returncode, 1)
        self.assertIn("private-evidence-path", result.stdout)

    def test_secret_signature(self):
        self.write("key", b"-----BEGIN " + b"OPENSSH PRIVATE KEY-----\n")
        self.commit("Add key")
        self.assertEqual(self.scan().returncode, 1)

    def test_upstream_unchanged_fixture_exemption(self):
        self.write("upstream-fixture", b"-----BEGIN " + b"PRIVATE KEY-----\n")
        self.commit("Upstream fixture")
        self.base = self.git("rev-parse", "HEAD").strip().decode()
        self.write("feature", b"safe")
        self.commit("Add feature")
        self.assertEqual(self.scan().returncode, 0)

    def test_invalid_ref_and_nonancestor_range(self):
        self.assertEqual(self.scan("--head", "missing-ref").returncode, 2)
        self.git("checkout", "--orphan", "unrelated")
        self.write("other", b"safe")
        self.commit("Unrelated root")
        self.assertEqual(self.scan().returncode, 2)


if __name__ == "__main__":
    unittest.main()
