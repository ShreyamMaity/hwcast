# hwcast for AI agents (MCP)

hwcast speaks the [Model Context Protocol](https://modelcontextprotocol.io), so AI agents (Claude Code, Codex, Claude Desktop, Cursor, VS Code, Gemini CLI, Antigravity and others) can read this PC's hardware live. Ask things like "is a GPU free for a 12 GB model?", "how hot does the CPU get during this build?" or "wait until the render finishes, then start the next one".

Everything is read only. Agents can look, never change fans, clocks, power limits or processes.

## Connect an agent

hwcast 0.2.0 or newer must be installed and running. There are two ways in, and both reach the same tools.

### Agents on this PC: `hwcast mcp`

The agent starts `hwcast mcp` itself and talks to it over stdin and stdout. It forwards each request to the running service.

**Claude Code**

```
claude mcp add --scope user hwcast -- hwcast mcp
```

**Codex CLI**

```
codex mcp add hwcast -- hwcast mcp
```

**Claude Desktop, Cursor, Gemini CLI, Antigravity** and other clients with an `mcpServers` JSON config:

```json
{
  "mcpServers": {
    "hwcast": {
      "command": "C:\\Program Files\\hwcast\\hwcast.exe",
      "args": ["mcp"]
    }
  }
}
```

**VS Code** (`.vscode/mcp.json` or your user `mcp.json`):

```json
{
  "servers": {
    "hwcast": { "type": "stdio", "command": "C:\\Program Files\\hwcast\\hwcast.exe", "args": ["mcp"] }
  }
}
```

If you installed with `--port` or `--dir`, `hwcast mcp` picks up the port from the install folder. Use the full path to `hwcast.exe` if the client cannot find `hwcast` on the PATH (open a new terminal after installing so the PATH is fresh).

### Agents on other machines: `http://<pc>:8340/mcp`

The service answers MCP directly over HTTP at `/mcp`, with the same network rules as the dashboard: this PC, your local network and VPN addresses such as Tailscale. `hwcast status` prints the addresses.

**Claude Code** on a laptop or Mac:

```
claude mcp add --scope user --transport http hwcast http://192.168.1.20:8340/mcp
```

Other clients: use their "HTTP" or "streamable HTTP" server type with the same URL. If a client only supports stdio, another Windows PC can bridge with `hwcast mcp --host 192.168.1.20:8340`, and any machine with Node can use `npx mcp-remote http://192.168.1.20:8340/mcp`.

## Tools

| Tool | Returns |
|---|---|
| `get_summary` | One compact reading of the whole PC: CPU temperature, load and power; every GPU's temperature, load, VRAM, power and fan; RAM; the busiest apps. Agents should start here |
| `get_cpu` | CPU model, Tctl and per CCD temperature, package power, total load and every logical processor's load |
| `get_gpus` | Every NVIDIA GPU, or one with `index`: temperature, hotspot, memory temperature, core and memory controller load, VRAM used, free and total, power and limit, clock, fan % and RPM |
| `get_memory` | RAM, commit charge and page file |
| `get_sensors` | Motherboard temperatures, thermistor probes and fan speeds with their labels and ids |
| `get_processes` | The six apps using the most CPU and the six using the most memory |
| `get_snapshot` | The full raw snapshot, the same JSON mods receive ([docs/mods.md](mods.md)) |
| `list_metrics` | Every metric name this PC offers, with unit and current value |
| `sample` | Watches `metrics` for `seconds` (1 to 60, default 5) and returns min, average, max and last of each |
| `check_gpu_free` | Watches every GPU for a few seconds and says which are free: peak load under `max_load_pct` (default 10) and at least `need_vram_gb` VRAM free. Also names the best one |
| `wait_for` | Blocks until a metric stays `below` or `above` a value for `hold_seconds` (default 5), or `timeout_seconds` passes (default 120, up to 600) |

### Metric names

Used by `sample` and `wait_for`. `list_metrics` shows exactly which ones a PC has.

| Metric | Unit |
|---|---|
| `cpu.temp`, `cpu.ccd0`, `cpu.ccd1` | degrees C |
| `cpu.load`, `cpu.maxcore` (busiest thread) | % |
| `cpu.power_w` | W |
| `gpuN.temp`, `gpuN.hotspot`, `gpuN.memtemp` | degrees C |
| `gpuN.load`, `gpuN.memload`, `gpuN.vram`, `gpuN.power`, `gpuN.fan` | % (VRAM used and power as a share of the limit) |
| `gpuN.vram_used`, `gpuN.vram_free` | GB |
| `gpuN.power_w`, `gpuN.fan_rpm`, `gpuN.clock` | W, RPM, MHz |
| `mem.used`, `mem.commit` | % |
| `mem.used_gb`, `pagefile.used_gb` | GB |
| `board.<id>` | degrees C or RPM, any motherboard sensor by its id from `get_sensors` |

`N` is the GPU number as `nvidia-smi` counts them. The names match the [hwcast-gcb200](https://github.com/ShreyamMaity/hwcast-gcb200) mod where they overlap.

## Cost

hwcast still reads sensors only on demand. A one-off tool call wakes the sampler for about four seconds; `sample`, `check_gpu_free` and `wait_for` keep it running for as long as they watch. After that it goes back to 0% CPU.

## Protocol details

- Endpoint: `POST /mcp`, streamable HTTP transport, stateless (no session ids), every reply is a single JSON object. `GET /mcp` returns 405 since the server never pushes messages.
- Protocol versions 2025-11-25, 2025-06-18, 2025-03-26 and 2024-11-05 are accepted.
- Requests that carry a browser `Origin` header are refused unless the page was served from `localhost`, which blocks DNS rebinding tricks from web pages. Agents do not send one.
- No login, like the dashboard. Anyone who can open the dashboard can call the tools and see the same data.
