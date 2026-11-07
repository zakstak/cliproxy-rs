# Packaging

Package files for the published release binaries. Nothing here is submitted automatically. This fork supports Linux only, so it keeps no Homebrew, winget or other Windows or macOS packages.

- `aur/`: `cliproxy-rs-bin` for Arch Linux, x86_64 and aarch64.

The AUR files are for v0.4.1. Their hashes are the release's `SHA256SUMS` entries.

## AUR

You need an AUR account with an SSH key. The first push creates the package.

```sh
git clone ssh://aur@aur.archlinux.org/cliproxy-rs-bin.git
cp packaging/aur/PKGBUILD packaging/aur/.SRCINFO cliproxy-rs-bin/
cd cliproxy-rs-bin
git add PKGBUILD .SRCINFO
git commit -m "cliproxy-rs-bin 0.4.1"
git push
```

For a new release, set `pkgver`, set `pkgrel=1`, and copy the two `unknown-linux-gnu` hashes from the release's `SHA256SUMS`. Then, on Arch, check and regenerate:

```sh
makepkg -f && namcap PKGBUILD *.pkg.tar.zst
makepkg --printsrcinfo > .SRCINFO
```
