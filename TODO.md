# TODO

## PCB compaction (before sending fecha-gateway-v3 to fab)

Goal: improve Fragua so it can compact a PCB layout as much as possible — shrink the
board outline / pack components tightly while keeping DRC at 0 (courtyard clearances,
routability, edge clearance). The fecha-gateway-v3 board is on hold and will NOT be
sent to JLCPCB until this exists, so it doubles as the real-world test case.

Test board (fecha-gateway-v3):

- Design (Fragua JSON, current state = v3: MOSFET modem power switch + U2 chirality fix,
  silk "v3 fragua", auto-routed, DRC/ERC clean):
  `~/fecha/firmware/sf7/yellow/fecha-gateway-v2/fecha-gateway-v2.json`
- Reference fab export of that state (do not regenerate over it):
  `~/fecha/firmware/sf7/yellow/fecha-gateway-v2/fab/fecha-gateway-v3-*` (+ `preview-v3.png`)
- Work on a copy — that JSON is the source of truth for the v3 order.

Success criteria: measurably smaller board area than the current v3 outline, auto-route
still completes (0 failed nets), DRC 0 / ERC 0, and the JLCPCB export stays valid.
