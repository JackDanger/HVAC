# AUR Packaging

The `hvac` package is published to the [Arch User Repository](https://aur.archlinux.org/packages/hvac).

## How releases get to AUR

The `aur` job in [`.github/workflows/release.yml`](../../.github/workflows/release.yml)
runs after each `v*` tag is pushed. It bumps `pkgver` in `PKGBUILD` to match the
tag, regenerates `.SRCINFO`, and force-pushes to
`ssh://aur@aur.archlinux.org/hvac.git` using
[`KSXGitHub/github-actions-deploy-aur`](https://github.com/KSXGitHub/github-actions-deploy-aur).

## SSH key

AUR pushes are authenticated by SSH. The keypair is one-of-a-kind to the AUR
account `jackdanger` and is **only** used for AUR.

The public half is registered with the AUR account. For reference, it is:

```
ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIObBfTvJKSYnrfQTIVL5UpS3jAwoCumqdvJMQOHf+o1+ jackdanger@hardcall
```

The private half is stored as the GitHub Actions secret `AUR_SSH_PRIVATE_KEY`
on this repo. To rotate it:

1. Generate a new keypair: `ssh-keygen -t ed25519 -C 'jackdanger@hardcall' -f /tmp/aur_new -N ''`
2. Replace the public key on the AUR account at <https://aur.archlinux.org/account/jackdanger/edit>
3. Update the GitHub secret: `gh secret set AUR_SSH_PRIVATE_KEY < /tmp/aur_new`
4. Discard the temporary keys.

## Manual publish (when CI is unavailable)

```bash
git clone ssh://aur@aur.archlinux.org/hvac.git aur-hvac
cp packaging/aur/PKGBUILD aur-hvac/PKGBUILD
cd aur-hvac
# Bump pkgver to match the tag you intend to ship
makepkg --printsrcinfo > .SRCINFO
git add PKGBUILD .SRCINFO
git commit -m "Update to vX.Y.Z"
git push
```

## Local install from this PKGBUILD

To install from a checkout without going through AUR:

```bash
cd packaging/aur
makepkg -si
```

This builds against the GitHub source tarball matching `pkgver` in `PKGBUILD`.
