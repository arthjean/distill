## Summary

<!-- What changed and why? -->

## User-visible behavior

<!-- Tool output, CLI behavior, or API surface users will notice. Write "None" for internal-only changes. -->

## Validation

<!-- Check every command that applies to the changed surface. -->

- [ ] `bun run check:native`
- [ ] `bun run knip`
- [ ] `bun run build`
- [ ] `bun run package:native` and checksum verification, if distribution changed

## Compatibility

- [ ] Source bytes still commit before any omitting projection
- [ ] Host adapters contain no projection, persistence, tokenization, or
      preservation policy
- [ ] Configured-root, artifact-integrity, resource-limit, and zero-network
      guarantees are preserved
- [ ] No new runtime dependency is added without a current requirement and
      explicit rationale

## Notes for reviewers

<!-- Anything non-obvious: a workaround, an invariant, a gotcha, or a follow-up. -->
