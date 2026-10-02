# user-service-switcher

Switching between mutually exclusive systemd **user** services: named groups hold a set of services, of which at most one may be active at a time. `uss` is the CLI; `ussd` is the daemon that keeps the rule.

## Language

**uss**:
The CLI control tool; a thin client that sends commands to ussd.
_Avoid_: client, frontend, binary

**ussd**:
The user-level daemon: the control plane (owner of group state, issuer of every systemd operation, single writer) and the watchdog (keeps each group to at most one active member, including against out-of-band starts).
_Avoid_: server, service manager (that is systemd)

**group**:
A named set of mutually exclusive services. Created when the first service is added; deleted when the last is removed.
_Avoid_: pool, cluster, set

**member**:
A service belonging to a group. A service belongs to at most one group.
_Avoid_: entry, item

**service**:
A systemd user `.service` unit. Referred to by bare name, normalized to the full `name.service` form in storage and output.
_Avoid_: unit (in user-facing contexts), program

**active**:
A service is running right now. Distinct from *enabled*, which is out of scope.
_Avoid_: running state, on, enabled

**enabled**:
The systemd notion of "starts at login." Deliberately out of scope: uss/ussd manage the active state only.
_Avoid_: activated, turned on

**out-of-band start**:
A member starting through a path other than `uss` (e.g. plain `systemctl --user start`). The watchdog treats it as the user's intent: it stops the group's other active members and keeps the newcomer.
_Avoid_: external start, stray start, violation

**switch**:
Making one member of a group active: stop the group's other active members, then start the target. Performed by `start`.
_Avoid_: activate, toggle, flip
