# Release process

Tagging `v<X.Y.Z>` on `main` triggers `.github/workflows/release.yml`, which:

1. Builds 4 release binaries (linux x86_64/aarch64, macos x86_64/aarch64) +
   2 `.deb` packages (linux x86_64/aarch64).
2. Publishes the GitHub Release with all artifacts + their `.sha256` sidecars.
3. Publishes the crate to crates.io.
4. Updates the Homebrew formula in [`JackDanger/homebrew-tap`](https://github.com/JackDanger/homebrew-tap).
5. Updates the apt repository on the `gh-pages` branch (served at
   `https://jackdanger.github.io/HVAC`).
6. Publishes the AUR package — *if* `AUR_SSH_PRIVATE_KEY` is set; skipped
   otherwise so the release as a whole still passes.

## Repository secrets

| Secret | Used by | Required for |
| --- | --- | --- |
| `CARGO_REGISTRY_TOKEN` | `crates-io` job | crates.io publish |
| `HOMEBREW_TAP_TOKEN` | `homebrew` job | Push to homebrew-tap repo |
| `APT_SIGNING_KEY` | `apt-repo` job | Sign Release / InRelease |
| `AUR_SSH_PRIVATE_KEY` | `aur` job | AUR push (job skips if missing) |
| `PROMPTLOG_TOKEN` | CI `publish-promptlog` job | promptlog publish (optional) |

## AUR setup (one-time)

The AUR job is gated on `AUR_SSH_PRIVATE_KEY`. To enable it:

1. Create an account at <https://aur.archlinux.org/register>.
2. Add a dedicated SSH key under
   <https://aur.archlinux.org/account/<username>/edit> → "SSH Public Key".
3. Save the private half to repo Settings → Secrets → Actions as
   `AUR_SSH_PRIVATE_KEY`.
4. First-release pre-step: `git clone ssh://aur@aur.archlinux.org/hvac.git`
   and push a seed `PKGBUILD` + `.SRCINFO` — the action expects the AUR
   repo to already exist.

## Tagging a release

```sh
# From main, after the changes you want to ship have landed:
git tag v5.3.0
git push origin v5.3.0
```

`workflow_dispatch` is also wired up — Actions → Release → "Run workflow"
lets you re-run against an existing tag without re-pushing it.

## Verifying a release

After the workflow finishes:

- GitHub Release page lists all 12 artifacts (4 tarballs + 4 sha256 +
  2 debs + 2 deb sha256). The release notes are auto-generated from PR
  titles.
- `brew upgrade JackDanger/tap/hvac` should pick up the new version.
- `https://jackdanger.github.io/HVAC/dists/stable/main/binary-amd64/Packages`
  lists the new `.deb`.
- `hvac --version` after install matches the tag (sans the `v`).
