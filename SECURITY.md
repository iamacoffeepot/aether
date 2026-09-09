# Security policy

## Reporting a vulnerability

Report privately through GitHub's private vulnerability reporting for this
repository: open the **Security** tab and choose **Report a vulnerability**.
That opens a private advisory visible only to the maintainers.

Do not open a public issue, pull request, or discussion for a security problem.

Include what you have: affected crate or binary, the commit you reproduced on,
the steps, and the impact you believe it has. A proof of concept helps but is
not required to file.

Expect a first response within a week. There is no bug bounty.

## Supported versions

aether is pre-1.0 and unreleased. Only the latest tagged pre-release on `main`
is supported; older tags and unmerged branches receive no fixes. No crate is
published to crates.io yet, so there is no released artifact to patch: fixes
land on `main` and appear in the next tag.
