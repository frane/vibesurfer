# Security Policy

## Supported versions

vibesurfer is pre-1.0; only the latest released version receives security
fixes. Update to the newest release before reporting.

| Version  | Supported |
| -------- | --------- |
| latest   | yes       |
| older    | no        |

## Reporting a vulnerability

Please **do not open a public issue** for security findings.

Report privately via one of:

- GitHub private vulnerability reporting:
  <https://github.com/frane/vibesurfer/security/advisories/new>
- Email: <frane.bandov@gmail.com> (subject line starting with
  `[vibesurfer security]`)

Include a reproduction if you can. You should hear back within a week;
fixes for confirmed issues are released as soon as practical and credited
in the CHANGELOG unless you prefer otherwise.

## Scope notes

vibesurfer is a *local* daemon: the threat model assumes the attacker is
either a web page the agent visits or another local user — not a remote
network peer (the daemon only listens on a local socket / named pipe).
Reports about pages escaping the engine sandbox, other local users
reaching the daemon socket or the on-disk store (`~/.vibesurfer`), or
secrets leaking into logs/audit rows are all in scope.
