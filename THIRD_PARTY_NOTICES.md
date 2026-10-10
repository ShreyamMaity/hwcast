# Third-party notices

## PawnIO modules (embedded in hwcast.exe)

`hwcast.exe` embeds two unmodified, signed modules from
[PawnIO.Modules](https://github.com/namazso/PawnIO.Modules) release 0.2.11:
`LpcIO.bin` and `AMDFamily17.bin`. They are licensed under the GNU Lesser
General Public License v2.1; the full text is in
`LICENSES/PawnIO.Modules-LGPL-2.1.txt`. Their source code is available at
https://github.com/namazso/PawnIO.Modules. They are loaded at runtime into the
separately installed [PawnIO](https://pawnio.eu) driver, and you can swap them
for other builds by running `scripts/fetch-modules.ps1` and rebuilding.

## LibreHardwareMonitor (reference)

The Super I/O register layout, ITE chip channel counts, Ryzen temperature
decoding and Gigabyte board label tables were written by following
[LibreHardwareMonitor](https://github.com/LibreHardwareMonitor/LibreHardwareMonitor)
(Mozilla Public License 2.0). No LibreHardwareMonitor code is distributed with
hwcast; it is credited here and in `src/superio.rs`, `src/cpu.rs` and
`src/board.rs`.

## Rust crates

| Crate | License |
|---|---|
| [nvml-wrapper](https://github.com/Cldfire/nvml-wrapper) | MIT OR Apache-2.0 |
| [serde_json](https://github.com/serde-rs/json) | MIT OR Apache-2.0 |
| [windows-sys](https://github.com/microsoft/windows-rs) | MIT OR Apache-2.0 |

Transitive dependencies are listed in `Cargo.lock`.
