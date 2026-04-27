# Releasing

makina releases are tag-driven. `release.yml` does all the heavy lifting; you only push a tag.

## Versioning

Semantic versioning. Pre-release tags use the form `vX.Y.Z-rc.N` and ship as GitHub pre-releases.

## Pre-flight checklist

1. `develop` is green on CI.
2. The release-tracking issue (`chore: release v…`) lists every issue scheduled for the release as
   closed.
3. `develop` was merged into `main` via PR (squash or merge — your call; both keep the changelog
   clean since Conventional Commits drives it).
4. You have `Releases: write` permission on the repo (project owner: yes).

## Cut the release

```bash
git checkout main
git pull --ff-only
git tag v0.1.0
git push origin v0.1.0
```

That's it. `release.yml` will:

1. Re-run `deno task ci` against the tagged commit.
2. Generate the changelog with `git-cliff` (using `cliff.toml` and the Conventional Commits log
   between the previous tag and this one).
3. Build standalone binaries for `aarch64-apple-darwin`, `x86_64-unknown-linux-gnu`, and
   `aarch64-unknown-linux-gnu`. (Intel macOS is intentionally not built — see ADR-008.)
4. Compute SHA-256 checksums for each binary into `SHASUMS256.txt`.
5. Publish a GitHub Release with the generated notes + the binaries + checksums.
6. Print the release URL into the workflow run summary.

## If the release fails

- Identify the failing job in the workflow run.
- If the failure is recoverable (flaky CI, transient API), re-run the workflow.
- If the failure is in the code, delete the tag
  (`git tag -d vX.Y.Z && git push origin :refs/tags/vX.Y.Z`), land a fix on `main` via PR, then
  re-tag.
- Never edit a published release's binaries — cut a `vX.Y.(Z+1)` instead.
