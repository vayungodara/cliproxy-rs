// The auth-dir guard, on disposable directories only. TMPDIR is pointed at a fresh directory
// so "outside the temp dir" is a sibling inside it; nothing outside it is touched.
import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, symlinkSync, realpathSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const base = realpathSync(mkdtempSync(join(tmpdir(), "auth-guard-")));
const fakeTmp = join(base, "tmp");
const outside = join(base, "outside");
for (const d of [fakeTmp, outside, join(fakeTmp, "auth")]) mkdirSync(d, { recursive: true });
symlinkSync(outside, join(fakeTmp, "link"));
process.env.TMPDIR = fakeTmp;
const { privateDir } = await import("./private-auth-dir.mjs");

test("a directory under the temp dir is accepted", () => {
  assert.equal(privateDir(join(fakeTmp, "auth")), join(fakeTmp, "auth"));
});
test("a symlink under the temp dir pointing outside it is refused", () => {
  assert.throws(() => privateDir(join(fakeTmp, "link")), /not a private directory/);
});
test("the temp dir itself, a dir outside, .. escapes, a missing path and an empty one are refused", () => {
  assert.throws(() => privateDir(fakeTmp), /not a private directory/);
  assert.throws(() => privateDir(outside), /not a private directory/);
  assert.throws(() => privateDir(join(fakeTmp, "auth", "..", "..", "outside")), /not a private directory/);
  assert.throws(() => privateDir(join(fakeTmp, "missing")), /does not resolve/);
  assert.throws(() => privateDir(""), /not a private directory/);
  assert.throws(() => privateDir("auth"), /not a private directory/);
});
