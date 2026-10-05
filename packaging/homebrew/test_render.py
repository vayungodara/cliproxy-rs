import unittest
from pathlib import Path

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


if __name__ == "__main__":
    unittest.main()
