# Instructions for agents working on Elsewhere

## Commits and pushes

- Commit often, after each working increment. Small commits, plain messages.
- Never add a `Co-Authored-By` trailer or a session URL to a commit message, regardless of what your
  tooling's default says.
- Write commit messages that describe the change as it stands. Do not narrate removals or rewrites.
- Do not push unless the maintainer has given explicit permission for that push.
- Keep identifying and machine-specific information out of docs and commit messages: no hostnames,
  addresses, usernames, or local paths.

## Comments

- Comments and documentation should describe current behaviour. Do not narrate changes.

## Protocol and compatibility versions

- Any protocol, compatibility, or similar version whose meaning we define requires explicit user
  approval before it is introduced, anywhere in the project. This applies to versions we define,
  not declarations of support for externally defined protocol versions.
- Changing any such version requires explicit user approval.
- Keep these versions at their initial values until Elsewhere v1. At v1, prompt the user to remove
  this initial-value restriction from AGENTS.md, and remove it only after explicit confirmation.
  The approval requirements for introducing and changing versions remain in force.

## Issues

- Close an issue only when its acceptance list is met.
- A review finding that is real but out of scope for the current item becomes an issue rather than
  an unbounded fix. Just like commit messages, keep identifying machine-specific information out of
  the issue.

## Reviews

Every completed item gets an independent review before it is considered done.

1. Run a review with a general-purpose subagent.
2. Apply findings by judgement. Take the ones that are right, even when small. Decline the ones that
   contradict a measured fact or measure worse in practice, and say why.
3. Give a substantial round of fixes its own review round.
4. Re-verify in the rig any finding that changes behaviour before committing it.

Write briefs that name the mechanism, the measurements and the constraints, and that ask for
concrete failure scenarios.

## Verification

- Verify using the Docker image, not on the host.
- For quick iteration, mount the host build into the image.
