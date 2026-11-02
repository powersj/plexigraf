# Security policy

Tern runs third-party plugins in a WebAssembly sandbox, so we treat a way out
of that sandbox as the most serious kind of bug.

## Supported versions

Tern is pre-1.0. Only the latest release gets security fixes.

## Reporting a vulnerability

Report vulnerabilities privately through GitHub's
[private vulnerability reporting](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing-information-about-vulnerabilities/privately-reporting-a-security-vulnerability)
on this repository (**Security** tab, then **Report a vulnerability**). Do not
open a public issue.

Include the Tern version, the config and plugin needed to reproduce the
problem, and what an attacker gains.

We aim to acknowledge a report within 3 working days and to agree on a
disclosure date with you once we understand the impact. We credit reporters
in the advisory unless you ask us not to.

## In scope

- A plugin reading or writing files, environment variables, the network, or
  host memory that its manifest and grants do not allow.
- A plugin exceeding its memory, time, or batch limits, or taking down the
  agent or other plugins.
- Secrets reaching plugins that were not approved for them, or appearing in
  logs.
- Malformed input (config, line protocol, plugin output) that crashes the
  agent or corrupts other pipelines' data.

## Out of scope

- Anything that needs control of the agent's config file or the host account
  that runs it.
- Bugs in plugins themselves, unless they let the plugin escape the sandbox.
