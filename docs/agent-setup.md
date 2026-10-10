# hwcast setup guide (for AI coding agents)

This file is written for an AI coding agent (Claude Code, Codex, Antigravity, Grok Build or any other agent that can run commands). A user clicked "Setup with an agent" in the hwcast README and wants you to set hwcast up on their Windows PC. Follow these steps in order.

## What hwcast is

hwcast is a small open-source Windows program (one Rust exe, about 600 KB) that runs as a background Windows service and serves a live hardware dashboard to any browser on the local network: CPU temperature and load, every NVIDIA GPU, RAM and page file, motherboard temperatures and fans, and the apps using the most resources. It reads sensors only while a dashboard is open, so it costs close to nothing otherwise. Source and releases: https://github.com/ShreyamMaity/hwcast

## Ground rules

* Do not change anything on the PC until the user has answered the questions in step 2.
* Before each step that changes the system (installing a driver, installing the service, editing config), say in one line what you are about to do and wait for a yes.
* Installing needs administrator rights. Windows will show a UAC prompt; tell the user to expect it and to approve it. Never try to bypass UAC or change security settings.
* Download only from the official GitHub releases of ShreyamMaity/hwcast and verify the SHA256 checksum before running anything.
* Use PowerShell for all commands. If your tool asks the user to approve each command, that is expected; explain what each one does in a few words.
* If something fails, stop, show the error, and suggest the next step instead of retrying blindly.

## Step 1: check the PC (read only)

Run these and keep the results for the summary:

```powershell
[Environment]::OSVersion.VersionString; $env:PROCESSOR_ARCHITECTURE
Get-Service hwcast -ErrorAction SilentlyContinue | Select-Object Status
Get-Service PawnIO -ErrorAction SilentlyContinue | Select-Object Status
(Get-ItemProperty 'HKLM:\HARDWARE\DESCRIPTION\System\BIOS').BaseBoardManufacturer
(Get-ItemProperty 'HKLM:\HARDWARE\DESCRIPTION\System\BIOS').BaseBoardProduct
(Get-ItemProperty 'HKLM:\HARDWARE\DESCRIPTION\System\CentralProcessor\0').ProcessorNameString
nvidia-smi --query-gpu=index,name --format=csv,noheader 2>$null
```

Then tell the user briefly what will and will not work on their hardware:

* Windows 10 or 11 on x64 is required. Stop and explain if it is anything else.
* CPU temperature and power: AMD Ryzen only. Intel CPUs still get load, clocks are not shown.
* GPUs: NVIDIA only (every card gets its own panel). AMD and Intel GPUs are not shown yet.
* Motherboard temperatures and fans: boards with ITE Super I/O chips, which is most Gigabyte boards. Gigabyte AM5 boards (X870, X870E, B850) get named sensors; other ITE boards get numbered ones. ASUS, MSI and ASRock boards mostly use Nuvoton chips, which are not supported yet, so the board panel will simply be hidden.
* If hwcast is already installed, offer to update it (same steps, the config is kept) or to change settings.

## Step 2: ask the user (one message, then wait)

1. **Port** for the dashboard. Default 8340. Only change it if something else uses that port.
2. **Where will you open the dashboard?**
   a. Only on this PC.
   b. On other devices at home (phone, laptop, tablet, spare screen). This is the default and works out of the box.
   c. Away from home through Tailscale or another VPN. Works out of the box if the VPN is already set up.
   d. Through a reverse proxy on the internet. Possible, but needs a login in front of it because hwcast has none.
3. **PawnIO sensor driver.** CPU temperature and motherboard sensors need the free, signed PawnIO driver (https://pawnio.eu). Install it? Without it, load, memory, GPUs and processes still work.
4. **Install folder.** Default is `C:\Program Files\hwcast`.
5. **Optional:** have they placed any thermistor probes on the motherboard's temperature headers, or want to rename sensors? This can also be done later.

## Step 3: download and verify

```powershell
$dir = Join-Path $env:TEMP 'hwcast-setup'; New-Item -ItemType Directory -Force $dir | Out-Null
$rel = Invoke-RestMethod https://api.github.com/repos/ShreyamMaity/hwcast/releases/latest
foreach ($n in 'hwcast.exe', 'SHA256SUMS.txt') {
  $a = $rel.assets | Where-Object name -eq $n
  Invoke-WebRequest $a.browser_download_url -OutFile (Join-Path $dir $n) -UseBasicParsing
}
$want = (Select-String -Path "$dir\SHA256SUMS.txt" -Pattern ' hwcast\.exe$').Line.Split(' ')[0]
$got = (Get-FileHash "$dir\hwcast.exe" -Algorithm SHA256).Hash.ToLower()
if ($want -ne $got) { throw "Checksum mismatch, stopping." } else { "Checksum OK, version $($rel.tag_name)" }
```

If the checksum does not match, stop and tell the user. Do not run the file.

## Step 4: install (after the user says yes)

If they chose PawnIO in step 2, install it first (one UAC prompt):

```powershell
winget install --id namazso.PawnIO -e --accept-package-agreements --accept-source-agreements
```

If winget is missing, give them the https://pawnio.eu download link and continue once it is installed.

Then install hwcast (one UAC prompt). Add `--port <n>` and `--dir "<path>"` only if they chose something other than the defaults:

```powershell
& "$env:TEMP\hwcast-setup\hwcast.exe" install --no-open --no-pause --no-pawnio
```

## Step 5: verify and hand over

```powershell
hwcast status   # or the full path: & "C:\Program Files\hwcast\hwcast.exe" status
$port = 8340    # or their chosen port
$j = Invoke-RestMethod "http://127.0.0.1:$port/api/now"
"CPU: $($j.cpu.name), temp $($j.cpu.tctl), load $([int]$j.cpu.load)%"
"GPUs: $($j.gpus.Count)"; "Board sensors: $(@($j.sensors.temps | Where-Object v).Count)"
```

Report what is working, then give them the dashboard addresses that `hwcast status` printed, matched to their answer in step 2:

* a: `http://localhost:<port>`
* b: the `192.168.x.x` (or `10.x`) address, to open on any device on the same network
* c: the `100.x.y.z` address from Tailscale or their VPN
* d: explain that a reverse proxy (Caddy, nginx, Traefik) must add HTTPS and a login before the dashboard faces the internet. Offer to help configure it, but only if they ask.

Offer to open the dashboard now with `hwcast open`.

## Step 6 (optional): name sensors and probes

`hwcast --once` (run in an admin terminal) prints every sensor with its ID, for example `it8696e.t5`. Help the user identify which channel is which (for a probe, ask them to hold it between two fingers for 20 seconds and watch which value rises). Then edit `config.json` in the install folder:

```json
{
  "bind": ["0.0.0.0:8340"],
  "labels": { "it8696e.t5": "Between GPUs" },
  "probes": ["it8696e.t5"],
  "hide": []
}
```

Keep the existing `bind` value. Save it as UTF-8 and run `hwcast restart`.

## Useful commands

| Command | Does |
|---|---|
| `hwcast status` | State, sensor driver, dashboard addresses |
| `hwcast open` | Opens the dashboard |
| `hwcast stop` / `hwcast start` | Stops or starts the service (no admin needed) |
| `hwcast uninstall` | Removes everything (admin prompt). Also in Settings > Apps |
