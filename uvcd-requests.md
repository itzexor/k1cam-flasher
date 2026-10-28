# uvcd diagnostic requirements

The diagnostic channel requested here is implemented. See
[ISP diagnostics in Tuning](docs/diagnostics.md) for client behavior and the
[firmware specification](https://github.com/itzexor/k1cam-firmware/blob/k1cam/package/k1cam-uvcd/DIAGNOSTICS.md)
for the version 1 wire format.

Live white-balance temperature under auto was already supported by uvcd's
GET_CUR and AUTOUPDATE responses when the channel was added.
