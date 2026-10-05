import unittest
from pathlib import Path
import subprocess
import sys
import tempfile

from render import render

TARGETS = (
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-gnu",
)


class RenderTest(unittest.TestCase):
    def setUp(self):
        self.template = Path(__file__).with_name("cliproxy-rs.rb").read_text()
        self.checksums = "\n".join(
            f"{str(index) * 64}  cliproxy-1.2.3-{target}.tar.gz"
            for index, target in enumerate(TARGETS, 1)
        )

    def test_each_platform_gets_its_own_checksum(self):
        formula = render("v1.2.3", self.checksums, self.template)
        self.assertNotIn("@VERSION@", formula)
        for index, target in enumerate(TARGETS, 1):
            self.assertIn(f'cliproxy-1.2.3-{target}.tar.gz"\n      sha256 "{str(index) * 64}"', formula)

    def test_rejects_bad_release_metadata(self):
        for tag, checksums in (
            ("v1.2.3-rc1", self.checksums),
            ("1.2.3", self.checksums),
            ("v1.2.3", self.checksums.split("\n", 1)[1]),
            ("v1.2.3", self.checksums + "\n" + self.checksums),
            ("v1.2.3", self.checksums.replace("1" * 64, "z" * 64)),
        ):
            with self.subTest(tag=tag, checksums=checksums):
                with self.assertRaises(ValueError):
                    render(tag, checksums, self.template)

    def test_existing_formula_never_downgrades_and_equal_versions_retry(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "cliproxy-rs.rb"
            sums = Path(directory) / "SHA256SUMS"
            sums.write_text(self.checksums)
            for installed in ("1.10.0", "2.0.0", "1.2.4", "1.2.3", "1.2.2"):
                with self.subTest(installed=installed):
                    # Different checksums distinguish an equal-version retry from a no-op.
                    current = render("v1.2.3", self.checksums, self.template).replace("1.2.3", installed)
                    current = current.replace("1" * 64, "a" * 64)
                    output.write_text(current)
                    result = subprocess.run(
                        [sys.executable, str(Path(__file__).with_name("render.py")), "v1.2.3", str(sums), str(output)],
                        capture_output=True, text=True,
                    )
                    self.assertEqual(result.returncode, 0, result.stderr)
                    expected = current if installed in ("1.10.0", "2.0.0", "1.2.4") else render("v1.2.3", self.checksums, self.template)
                    self.assertEqual(output.read_text(), expected)


if __name__ == "__main__":
    unittest.main()
