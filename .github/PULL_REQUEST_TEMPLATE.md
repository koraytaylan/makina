<!--
Thanks for the PR. Fill in this template so reviewers (and Copilot) have everything they need.
-->

## Summary

<!-- One paragraph: what changed and why. -->

## Linked issue

<!-- Replace N with the issue number. The PR auto-closes the issue on merge. -->

Closes #N

## Definition of Done

- [ ] New exported symbols carry JSDoc; `deno doc --lint` is green.
- [ ] Tests cover the new code; coverage is ≥ 80% lines AND branches; in-memory doubles for any
      external collaborator landed here too.
- [ ] Any new architectural choice ships with its ADR under `docs/adrs/`.
- [ ] User-facing behavior changes updated `README.md` and the relevant `docs/*.md`.
- [ ] Commits follow Conventional Commits (`<type>[(<scope>)][!]: <subject>`).
- [ ] `deno task ci` is green on this branch.

## References

<!-- Plan section(s), ADR(s) introduced or updated, external docs. -->

## Reviewer notes

<!-- Anything that would speed up review: tricky bits, intentional trade-offs, follow-ups. -->
