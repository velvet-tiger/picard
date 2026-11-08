# Releasing Picard

A release is the `chore(release): X.Y.Z` commit on `main`, the tag `vX.Y.Z`, and
the GitHub release that `.github/workflows/release.yml` builds from that tag.

## Steps

1. Find the last tag: `git describe --tags --abbrev=0 --match 'v*'`. A release
   is due only if `git diff --name-only <tag>..origin/main` lists a path under
   `src/` or `src-tauri/`.
2. Pick the version. Any breaking commit since the tag bumps the major
   version. Otherwise any `feat` commit bumps the minor version. Anything
   else bumps the patch.
3. Update `CHANGELOG.md`. Keep the existing `[Unreleased]` entries, add
   entries for functional commits that have none, then rename the content to
   `## [X.Y.Z] - YYYY-MM-DD` (today in Australia/Sydney) under an empty
   `## [Unreleased]`. Add the `[X.Y.Z]` link reference at the bottom.
4. Set the version in `package.json`, `src-tauri/Cargo.toml`,
   `src-tauri/tauri.conf.json` and the `picard` entry in `src-tauri/Cargo.lock`.
   Change nothing else in the lock file.
5. Run `pnpm install --frozen-lockfile`, `pnpm -s check` and `pnpm -s test`,
   and `cargo test` where it builds. `cargo` needs macOS system libraries, so
   on Linux rely on the CI `check` job.
6. Commit the five files as `chore(release): X.Y.Z` and push to `main`. Never
   force-push. If `main` has moved, stop.

## Tagging

Do not push the tag from an agent session. The cloud git gateway refuses tag
pushes with HTTP 403, and retrying does not help.

`.github/workflows/tag-release.yml` makes the tag instead. It runs when a push
to `main` changes `package.json`, reads the version, and does nothing if that
version already has a tag. Otherwise it checks that `package.json`,
`Cargo.toml` and `tauri.conf.json` agree, creates the annotated tag `vX.Y.Z`,
and starts `release.yml` for it. A tag made with `GITHUB_TOKEN` does not start
other workflows, which is why it starts `release.yml` itself.

So after step 6, wait for **Tag release** and then **Release** in the Actions
tab. If **Tag release** did not run, for example because the version commit was
pushed before the workflow was merged, run it by hand from the Actions tab.

A version bump on `main` always tags and builds a release. Do not merge a
version bump you do not intend to release.

## Watching the build

`release.yml` builds, signs and publishes the release. It can take up to about
45 minutes. If it fails, report the failing job, step and log lines. Do not
delete the tag or retry; leave that to the maintainer.
