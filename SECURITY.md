# Security policy

HTTPorter holds the keys to people's storage and CDN accounts, so security reports are
welcome and taken seriously.

## Reporting a vulnerability

Please report it **privately**, not in a public issue, pull request or discussion.

- Once the repository is public on GitHub: use **Report a vulnerability** on its **Security**
  tab (GitHub's private security advisories). Only the maintainers see the report, and the
  fix can be prepared in a private fork before it is published.
- Until then, or if that is not possible: contact a maintainer privately and ask for a
  private channel before sending details.

Please include what you found, the HTTPorter version (Settings › About), the steps to
reproduce it, and what an attacker could do with it. Never include real credentials: use a
test zone or bucket, and revoke any key that was exposed.

What to expect: we aim to acknowledge a report within a week, then to send an assessment and
a plan, and to credit you in the release notes if you would like that. Please give us a
reasonable time to release a fix before you disclose the issue publicly.

## Supported versions

Security fixes go into the latest release. HTTPorter checks for updates only if you turn the
update check on (Settings), and never installs one by itself: download the new version from
the project's releases page.

## What counts

For example:

- a secret (storage password, account API key, S3 secret key) reaching anything other than
  Windows Credential Manager and the service it belongs to: the database, a settings export,
  the activity log, an error message, the screen, a crash report, or another host;
- HTTPorter sending data anywhere other than the services and CDNs the user added (and, when
  the update check is on, the release feed);
- a crafted server answer, listing, file name or local file that makes HTTPorter write
  outside the folder the user chose, delete what it should not, or run code;
- Reset HTTPorter or removing a site deleting Credential Manager entries that HTTPorter did
  not write;
- the release files (installer, zip) or the build and release workflows being open to
  tampering.

Out of scope: attacks that need an attacker who already controls the user's Windows account,
and problems in a storage service itself (report those to the service).
