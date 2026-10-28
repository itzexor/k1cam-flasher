# ISP diagnostics in Tuning

Metering and Histogram can use the camera's ISP statistics over either a
local USB connection or the remote daemon. The legend shows exposure, gain,
AE target and mean, white balance, effective zoom, requested and accepted
compensation, and the highlight guard's last decision. Older firmware keeps
labelled picture estimates.

## Sampling and overlays

Local diagnostics are polled at 2 Hz while Tuning is visible and Metering,
Clipping or Histogram is enabled. There is one request outstanding at most;
failed reads back off, and unavailable or stale data falls back to estimates.
Automatic exposure and white-balance controls continue refreshing alongside
these requests.

Remote image requests include diagnostics only when an overlay needs them.
The daemon samples before closing the capture stream, so a manual snapshot
keeps the crop and metering state active for that image. Auto snapshot reads
at 2 Hz; a held manual image retains its paired diagnostic record. ISP
statistics and the JPEG are sampled separately, not synchronized to one
sensor frame.

Metering uses accepted ISP weights, including eased Subject weights and the
tuning file's own weights, with the actual crop and linear zone levels.
Histogram shows relative linear counts over the whole sensor, including
pixels outside a zoomed view. Its red marker is the guard threshold at bin
200; green marks the guard's last level in view. Clipping stripes and crushed
shadow masks continue to come from the JPEG pixels.

## Protocol and compatibility

The firmware's [diagnostic-channel specification](https://github.com/itzexor/k1cam-firmware/blob/k1cam/package/k1cam-uvcd/DIAGNOSTICS.md)
is the authoritative wire format. Version 1 is 745 little-endian bytes,
carried in 14 pages through Extension Unit 4 selectors 29 and 30. Snapshot
IDs detect interleaved reads; length and per-group validity distinguish
append-only extensions and missing measurements. Firmware exchange
selectors 26-28 have separate state.

The authenticated, session-scoped `GET /api/v1/tuning/diagnostic` endpoint
returns the same binary record. Missing firmware returns 204; an older
daemon's 404 also means unavailable. Snapshot requests add `diagnostic=1`
and carry the record as hex in `K1cam-Diagnostic`, preserving the JPEG body.
These additions keep remote API version 1 compatible with older clients.

Firmware pins kernel `07f85b04d7c1`, which advertises the new selectors.
Install an image with that kernel and re-enumerate USB before using USB or
remote diagnostics. `uvcdctl diag` on the camera uses the local socket and
can work without the descriptor update; Linux USB hosts can use
`uvcdctl --device /dev/videoN diag`.
