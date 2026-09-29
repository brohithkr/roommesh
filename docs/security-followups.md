# Security follow-ups

Open items from the security reviews of the `feat/roommesh` branch, kept here to fix later.
`docs/security.md` describes the model as implemented; this file lists what is known to be weak,
unverified or deliberately deferred. Items are roughly in priority order.

## Needs review

1. **Responder rate limiter (a873a5d) has not been re-reviewed.** The fix for online SAS
   grinding (one responder attempt per connection; one new handshake per claimed peer id per
   2 s, doubling to 60 s after abandoned attempts; 20 new handshakes a minute globally; a
   "possible interference" warning) passed its own tests, including the reviewer's attack as a
   regression test. The adversarial re-review was stopped before it finished. It should check:
   - rotating peer ids, reconnecting per attempt, and completing wrong-code handshakes then
     redialling;
   - whether an attacker can grind from the **initiator** side (attacker takes the higher id
     against a lower-id victim who initiates), which the limiter does not cover;
   - that the 2 s per-id limit doesn't slow honest reconnects (peer restart, simultaneous dial,
     rekey) in combination with the watchdog and redial cadence;
   - the stated bound (~1,200 attempts/hour, ~35 days to an even chance of a match).

## Known limitations

2. **Key continuity (M8).** Every reconnect is a fresh, unauthenticated X25519 exchange; the
   6-digit code is only compared at invite time. An attacker who can force a reconnect between
   two existing members gets a man-in-the-middle position without anyone comparing a code.
   Fix: mix a secret exported from the verified session into later handshakes (key continuity),
   or show the code again when an existing member reconnects with a new key.
3. **Shared memory is world-accessible.** The driver creates `/roommesh.v1` with mode 0666, so
   any local process of any user can read the room mic and far-end audio or inject audio into
   RoomMesh Microphone. The app only trusts a region owned by `_coreaudiod`, which stops
   squatting but not reading or injection. Fix: hand the region to the app over XPC with 0600
   (the plan's fallback (b)), or restrict it to a group.
4. **The transport preamble is unauthenticated.** `RMHELLO1:<peer id>` is plaintext, so any LAN
   host can claim any peer id, displace a link (rate-limited to one replacement per second per
   peer), or pose as a peer that is backed off for an incompatible version. The handshake
   contains the damage (no session without keys), but it is a denial-of-service lever.
5. **Members are fully trusted.** Any member may invite, remove members, pick the coordinator or
   speaker, switch any member's mic and rename the room. A malicious member can take the
   speaker role and hear the meeting. Only name and capabilities are self-only.
6. **The removal denylist is partition-local.** A peer removed on one side of a split brain can
   come back when a higher-epoch manifest from the other side wins the heal.
7. **Pending invites can be lost.** An invite issued by a coordinator that dies before the
   invitee's join request arrives is lost (10-minute expiry anyway); the user re-invites.
8. **Mixed protocol versions (v2 vs v3).** When a v3 Mac initiates to a v2 Mac with the higher
   id, the v2 side logs a malformed frame instead of reporting a version mismatch, and the v3
   side keeps reconnecting through the watchdog. v3 → v3 and v2 → v3 are reported cleanly.

## Out of scope for now (denial of service)

9. No caps on links, sessions or nearby entries; fake peers stay in "Nearby" until they vanish
   from discovery.
10. The 64 inbound UDP slots (30 s idle timeout) can be held by spoofed source ports.
11. The handshake rate limits can be exhausted on purpose, delaying honest connections.
12. Traffic analysis: peer ids, display names and packet timing are visible on the LAN.

## Installer

13. The package is unsigned unless `DEVELOPER_ID_APP` / `DEVELOPER_ID_INSTALLER` are set, and
    not notarized unless `NOTARY_PROFILE` is set. Gatekeeper will warn on other Macs until both
    are done.
14. The app postinstall opens RoomMesh as the console user with `launchctl asuser … sudo -u`;
    the uninstaller removes files with `sudo rm -rf` on fixed paths only.
