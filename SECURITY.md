# Security Policy

## Supported versions

Only the latest release gets security fixes. `ref update` installs it.

## Reporting a vulnerability

Please do not open a public issue for a security problem.

Email **security@mollendorff.ai** with:

- what you found and what it lets someone do,
- steps that reproduce it (the command, MCP call or page involved),
- the Ref version (`ref --version`), your operating system and your Chrome version.

You can also use GitHub's private vulnerability reporting, under the **Security** tab of this repository, if it is enabled.

Ref is a small project kept up on a best-effort basis. I aim to reply within 14 days, and I will tell you what I plan to do about the report. Please give me reasonable time to ship a fix before you share the details publicly. I am glad to credit you in the release notes if you want that.

## What is in scope

- `ref fetch`, `ref pdf` and the other commands: how they handle URLs (only http and https are accepted), redirects and the content they read.
- The MCP server (`ref mcp`), including what its tools can write to disk.
- The saved Chrome profile used by `ref login` (`~/.ref/chrome-profile`), which holds your signed-in sessions: how Ref creates, uses and protects it, and anything that could leak it.
- `ref update`, which downloads a release from GitHub and checks its SHA-256 before installing.

## What is not in scope

- Chrome itself, including the calls it makes to Google: report those to Chromium.
- A web page that tries to give instructions to the AI agent reading Ref's output. That is a risk of letting an agent read the web. Ref cannot judge what a page means, so treat what it returns as untrusted data.
- Wrong or incomplete page content is a bug, not a security problem: please open a normal issue for those.
