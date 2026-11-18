---
name: release-rmycli
description: Prepare, tag, and push an rmycli release to GitHub when a maintainer requests a release. Crates.io publication is a manual follow-up after GitHub checks pass.
---

# Release rmycli

## Select the release

Use the requested version or resolve the requested SemVer bump against the last
release. If the target is ambiguous, ask which version to release. For a first
release the manifest may already contain the requested version.

Read `AGENTS.md`. Check the working tree, release branch, and push URLs.
Select the GitHub remote for the repository named in `Cargo.toml`. The branch
can track a different remote. Fetch the GitHub branch and inspect the commits
that would be pushed. Preserve unrelated local edits. Check for an existing
`v<version>` tag locally and on GitHub before creating release changes.

## Prepare the release commit

1. Set `[package].version` in `Cargo.toml`. Run
   `cargo metadata --format-version 1 > /dev/null` to refresh `Cargo.lock`.
   Check the diff for unrelated dependency changes and verify
   `cargo run --locked -- --version` reports the target version.
2. Review changes since the preceding release (the full history for the first
   release). Check README installation, examples, compatibility, and relevant
   documentation against shipped behavior. Correct stale claims and keep
   planned features and pending registry publication clearly identified.
3. In `CHANGELOG.md`, consolidate duplicate headings and overlapping entries
   under `Unreleased`. Describe the net effect for users and contributors,
   preserving distinct fixes, breaking changes, and migration details. Move
   the result under `## [<version>] - YYYY-MM-DD` using the release date.
   Put a fresh, empty `## [Unreleased]` above it. Keep older releases in order
   and use each applicable Keep a Changelog category once per release.
4. Run the required checks against the final changes:

   ```sh
   cargo fmt --all -- --check
   cargo clippy --all-targets --all-features -- -D warnings
   cargo test --all-targets --all-features
   ```

   All three must pass before committing. Review and stage the release files
   and commit them together with the subject `Release v<version>`.

## Tag and push

Write a tag message with the subject `rmycli v<version>` and a body drawn from
the finalized release notes. Explain major changes, important fixes, and any
upgrade steps. Save the message outside the worktree. With `tag` set to
`v<version>`, create the annotated tag on the verified release commit:

```sh
git tag -a "$tag" -F "$message_file" "$release_commit"
git show --no-patch "$tag"
```

Push the release commit and this single tag together. Set `github_remote` and
`branch` to the verified GitHub remote and destination branch:

```sh
git push --atomic "$github_remote" \
  "${release_commit}:refs/heads/${branch}" "refs/tags/${tag}:refs/tags/${tag}"
```

On a retry or an uncertain push result, inspect the remote branch and peeled
tag with `git ls-remote`. Reuse a matching annotated tag on the intended
release commit. Stop on a conflicting tag or a rejected branch update and
report the conflicting refs. Never force-push or replace a release tag.

Verify the pushed refs and report the commit, tag, and GitHub Actions runs for
that commit. The branch push starts CI and the `v*` tag starts Distribution
binaries. Report each run's observed status or link to Actions if it is still
pending. The maintainer handles crates.io publication after both runs succeed.
