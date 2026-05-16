# HVAC — Claude instructions

See `.claude/rules/` for full project context, git workflow, and Rust conventions.

## Bumping the version

When asked to "bump the patch version" (or major/minor), follow these steps exactly:

1. **Compute the new version** from `VERSION` at the repo root:
   ```sh
   # e.g. for a patch bump: 5.4.1 → 5.4.2
   OLD=$(cat VERSION)
   # increment the rightmost segment
   ```

2. **Write the new version** to `VERSION` (just `X.Y.Z`, no prefix):
   ```sh
   echo "5.4.2" > VERSION
   ```

3. **Run the propagation script** — it updates Cargo.toml, Cargo.lock,
   packaging/aur/PKGBUILD, and packaging/homebrew/hvac.rb, then verifies
   every substitution landed:
   ```sh
   ./scripts/set-version.sh
   ```

4. **Update CHANGELOG.md**:
   - Move any items under `## [Unreleased]` into a new `## [X.Y.Z] — YYYY-MM-DD` section.
   - Open a fresh empty `## [Unreleased]` section above it.
   - Add/update the link definitions at the bottom of the file:
     ```
     [Unreleased]: https://github.com/JackDanger/hvac/compare/vX.Y.Z...HEAD
     [X.Y.Z]: https://github.com/JackDanger/hvac/compare/vPREV...vX.Y.Z
     ```

5. **Commit, push, and open a PR** (all changes in a worktree branch per
   the git workflow in `.claude/rules/git.md`).

6. **After the PR merges**, tag from main with an annotated tag:
   ```sh
   VERSION=$(cat VERSION)
   git tag -a "v$VERSION" -m "v$VERSION"
   git push origin "v$VERSION"
   ```
   This fires `.github/workflows/release.yml` and publishes binaries,
   crates.io, Homebrew, apt, and AUR.
