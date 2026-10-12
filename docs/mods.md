# Writing a hwcast mod

A mod is any program that reads hwcast's live data and does something with it. hwcast does not load plugins: mods run as their own process (or service), connect over HTTP and never need admin rights or access to hardware sensors.

Want an AI agent to read hwcast instead of a program? Use the built-in MCP server described in [mcp.md](mcp.md).

## Endpoints

| Endpoint | Returns |
|---|---|
| `GET /events` | A Server-Sent Events stream. One `data: {json}` line per second |
| `GET /api/now` | One JSON snapshot, the same shape as a stream event |

The default address is `http://127.0.0.1:8340`. Mods running on the same PC should use `127.0.0.1`, which is always allowed. Remote mods need their address inside the `allow` list in hwcast's `config.json`.

Sensors are read only while at least one client is connected, so a mod that keeps `/events` open keeps hwcast sampling. Reconnect on your own if the stream drops (for example when hwcast restarts after an update); a few seconds of delay is enough.

## Snapshot format

Every snapshot carries `"api": 1`. Fields may be added at any time without changing the number. Renaming or removing a field, or changing its unit, bumps it. Check it and ignore keys you do not know.

| Field | Meaning |
|---|---|
| `api` | Snapshot schema version (currently `1`) |
| `t` | Unix time in milliseconds |
| `host` | Computer name |
| `board.name`, `board.profile` | Motherboard and the sensor profile in use |
| `cpu.name` | CPU model |
| `cpu.load` | Total CPU load, percent |
| `cpu.cores[]` | Per logical core load, percent |
| `cpu.tctl` | CPU control temperature, degrees C (AMD; `null` when unavailable) |
| `cpu.ccds[]` | Per CCD temperature, degrees C (AMD) |
| `cpu.power_w` | Package power, watts (AMD; may be `null`) |
| `mem.used_gb`, `mem.total_gb` | Physical RAM |
| `mem.commit_used_gb`, `mem.commit_total_gb` | Commit charge |
| `mem.pagefile_used_gb`, `mem.pagefile_size_gb`, `mem.pagefile_peak_gb` | Page file |
| `gpus[]` | One entry per NVIDIA GPU, empty when there is none |
| `gpus[].index` | NVML index, same as `nvidia-smi` |
| `gpus[].name`, `temp`, `util`, `mem_util` | Model, degrees C, core load percent, memory controller load percent |
| `gpus[].hotspot`, `mem_temp` | Hotspot and memory junction temperature, degrees C (`null` when the card does not report them, for example GDDR7 memory) |
| `gpus[].power_w`, `power_limit_w`, `clock_mhz` | Power, limit, core clock |
| `gpus[].fan_pct`, `fan_rpm` | Fan speed in percent and RPM, the fastest fan channel on the card |
| `gpus[].vram_used_gb`, `vram_total_gb` | Video memory |
| `sensors.temps[]` | Board temperatures: `id`, `label`, `v` (degrees C or `null`), `probe` (thermistor header), `socket` (CPU socket sensor) |
| `sensors.fans[]` | Board fans: `id`, `label`, `rpm` (or `null`) |
| `procs.cpu[]`, `procs.mem[]` | Top apps by CPU percent (`pct`) and memory (`gb`), with process count `n` |
| `self` | hwcast's own CPU, memory and sampling time |
| `notes[]` | Detection notes, for troubleshooting |

Any value can be `null` when the sensor is missing on a given machine. Sensor `id`s are stable across restarts; labels can be renamed by the user, so match on `id` when you need a specific board sensor.

## Minimal example

```powershell
curl.exe -sN http://127.0.0.1:8340/events
```

A mod is that loop plus a parser: read lines, keep the ones starting with `data: `, parse the JSON, act on it. [hwcast-gcb200](https://github.com/ShreyamMaity/hwcast-gcb200) is a complete example in Rust that also installs itself as a low-privilege Windows service.

## Guidelines

- Keep hwcast read only. A mod should not ask users to change hwcast's service account or open it to the internet.
- Run with the least privilege you need. The LocalService account is enough for anything that only talks to the network or a USB serial device.
- Name your project `hwcast-<thing>` and open a pull request to list it in the README's Mods table.
