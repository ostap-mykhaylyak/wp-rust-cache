# Security policy

## Reporting a vulnerability

Report privately, through [GitHub's private vulnerability
reporting](https://github.com/ostap-mykhaylyak/wp-rust-cache/security/advisories/new)
(the repository's Security tab, "Report a vulnerability"). Please do not
open a public issue for something exploitable.

Include what you did, what happened, and the version
(`wp-rust-cache version`, `php -r 'echo phpversion("wp_rust_cache");'`).
You will get an acknowledgement within a few days, and the fix will be
released with an advisory that credits you, unless you would rather it did
not.

## Supported versions

Fixes go into the latest release only. Before 1.0 there are no maintenance
branches: upgrading to the latest release is the fix.

## What wp-rust-cache assumes

Values stored in the shared-memory segment are passed to PHP's
`unserialize()`. Whoever can write the segment can therefore make every PHP
process that uses it run the code paths of chosen objects. Everything below
exists to keep that write access with the PHP-FPM user alone, and a report
against any of it is a serious one:

- **The segment is only used if its owner is trusted**: the process user,
  root, or the configured `owner` (with the configured `group` for
  deliberate sharing). A file planted in the world-writable `/dev/shm` by
  another local user is refused.
- **No bits for "other"**: a segment whose mode grants anything to other
  users is refused, and the configuration rejects such permissions.
- **No symlinks**: the segment is opened with `O_NOFOLLOW` and created with
  `O_EXCL`.
- **Workers attach after dropping privileges**: nothing is mapped in the
  PHP-FPM master (which runs as root and forks every pool).
- **Corrupted structures cannot escape the segment**: every offset read from
  shared memory is bounds-checked against limits kept in process memory, and
  every list walk is bounded. Corruption resets a shard; it must never lead
  to a read or write outside the mapping, a crash or a hang.
- **Local mode has no network surface**: no socket is opened.

Out of scope: an attacker who already runs code as the PHP-FPM user or root
(they can write the site's PHP files anyway), and denial of service by a
local user who creates the segment path first (the cache is refused, the
site keeps working; see "Several PHP users" in docs/OPERATIONS.md for
per-pool paths).
