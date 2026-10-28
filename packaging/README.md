# Packaging

Package files for the published release binaries. Nothing here is submitted automatically.

- `homebrew/`: the tap formula, rendered from each release's `SHA256SUMS` and pushed to the tap by `.github/workflows/release.yml`.
- `aur/`: `cliproxy-rs-bin` for Arch Linux, x86_64 and aarch64.
- `winget/`: `VayunGodara.cliproxy-rs` for Windows, x64.

The AUR and winget files are for v0.4.0. Their hashes are the release's `SHA256SUMS` entries.

## AUR

You need an AUR account with an SSH key. The first push creates the package.

```sh
git clone ssh://aur@aur.archlinux.org/cliproxy-rs-bin.git
cp packaging/aur/PKGBUILD packaging/aur/.SRCINFO cliproxy-rs-bin/
cd cliproxy-rs-bin
git add PKGBUILD .SRCINFO
git commit -m "cliproxy-rs-bin 0.4.0"
git push
```

For a new release, set `pkgver`, set `pkgrel=1`, and copy the two `unknown-linux-gnu` hashes from the release's `SHA256SUMS`. Then, on Arch, check and regenerate:

```sh
makepkg -f && namcap PKGBUILD *.pkg.tar.zst
makepkg --printsrcinfo > .SRCINFO
```

## winget

Submissions are pull requests to [microsoft/winget-pkgs](https://github.com/microsoft/winget-pkgs). On Windows:

```powershell
winget validate --manifest packaging\winget\manifests\v\VayunGodara\cliproxy-rs\0.4.0
winget install --manifest packaging\winget\manifests\v\VayunGodara\cliproxy-rs\0.4.0
wingetcreate submit packaging\winget\manifests\v\VayunGodara\cliproxy-rs\0.4.0
```

`winget install --manifest` needs `winget settings --enable LocalManifestFiles` in an administrator shell. Without `wingetcreate`, fork winget-pkgs, copy the folder to the same path under `manifests/` and open the pull request.

Later versions can be submitted once the package exists:

```powershell
wingetcreate update VayunGodara.cliproxy-rs --version <version> --urls https://github.com/vayungodara/cliproxy-rs/releases/download/v<version>/cliproxy-<version>-x86_64-pc-windows-msvc.zip --submit
```

The archive's folder name contains the version, so check that `RelativeFilePath` in the installer manifest names the new folder.
