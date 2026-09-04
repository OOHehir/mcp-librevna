# mcp-librevna

An [MCP](https://modelcontextprotocol.io) server for the
[LibreVNA](https://github.com/jankae/LibreVNA) — a 100 kHz–6 GHz, 2-port USB vector network
analyser. It lets an AI agent configure sweeps, calibrate, measure S-parameters, run
spectrum analysis and drive the signal generator, with guardrails that make it hard to
damage the instrument, the device under test, or the credibility of the numbers.

Built in Rust on top of [`rmcp`](https://crates.io/crates/rmcp) (the official MCP SDK),
talking SCPI to the LibreVNA-GUI application.

## How it works

```
MCP client (Claude)
    │ stdio, JSON-RPC
mcp-librevna
    │ TCP :19542, newline-framed SCPI
LibreVNA-GUI --no-gui
    │ USB
LibreVNA hardware
```

The server does not talk to the hardware directly. It drives LibreVNA-GUI, which already
implements calibration, de-embedding and sweep sequencing — the parts that are hard to get
right and easy to get subtly wrong. The GUI serves SCPI on port 19542 by default and runs
headless with `--no-gui`.

`mcp-librevna` will attach to a GUI you already have open, or start a headless one itself
with `--spawn` and `--gui-path`. Attaching is always tried first: the SCPI server accepts a
single client, so a second GUI would fight the first for the device.

## Tools

| Tool | Tier | What it does |
|---|---|---|
| `librevna_connect` | — | Connect, and report the device's own limits and the enabled tiers |
| `librevna_disconnect` | — | Release the device, leaving the GUI running |
| `librevna_status` | — | Mode, sweep, calibration validity, error flags, temperatures |
| `librevna_list_devices` | — | Serial numbers of every attached unit |
| `librevna_rf_off` | — | Stop all RF output. Never gated |
| `vna_configure_sweep` | measure | Span, points, IF bandwidth, power, averaging, log/linear |
| `vna_sweep` | measure | Run one sweep, waiting for averaging to complete |
| `vna_read` | measure | Trace summary: min, max, and a min/max envelope |
| `vna_marker` | measure | One interpolated value at a given frequency |
| `vna_analyze` | measure | Resonance, return loss, VSWR, bandwidth, Q |
| `vna_trace_list` | measure | List defined traces |
| `vna_trace_manage` | measure¹ | Create, rename, pause, resume, set S-parameter |
| `vna_cal_status` | measure | Active calibration and whether it still applies |
| `vna_cal_measure` | measure | Measure one calibration standard |
| `vna_cal_activate` | measure | Activate a calibration type, e.g. SOLT |
| `vna_cal_load` | measure | Load a calibration from the working directory |
| `sa_configure` | measure | Span, RBW, window, detector, averaging |
| `sa_sweep` | measure | Run one spectrum sweep |
| `sa_read` | measure | Peak table plus an estimated noise floor |
| `vna_export_touchstone` | destructive | Write full-resolution data as Touchstone |
| `vna_cal_save` | destructive | Save the active calibration |
| `vna_cal_reset` | destructive | Discard the calibration and all standards |
| `gen_configure` | emission | Drive a continuous carrier out of a port |
| `gen_off` | — | Stop the generator. Never gated |
| `scpi_raw` | manual-hardware | Send arbitrary SCPI, including the `MANUAL:` subsystem |

¹ `vna_trace_manage` needs the destructive tier only for its `delete` action.

## Safety

The RF ports have **no input protection** and are damaged above **+10 dBm**. Beyond that,
the risks an agent runs into are less obvious than they look, so the design targets four
of them specifically.

**Capability tiers.** Read and measure work out of the box. Everything with a consequence
is gated behind a startup flag, and a refusal names the flag that would allow it.

| Tier | Enabled by | Covers |
|---|---|---|
| read-only | always | Queries, status, device info |
| measure | default | Sweep configuration, acquisition, trace and calibration work |
| emission | `--allow-emission` | Generator, tracking generator, power above the ceiling |
| destructive | `--allow-destructive` | `*RST`, calibration reset, writing files |
| manual-hardware | `--allow-manual-hardware` | `scpi_raw` and the `MANUAL:` subsystem |

Firmware update (`DEV:UPDATE`) is **not exposed at any tier**, including through
`scpi_raw`. It can brick the unit and an agent has no reason to reach for it.

**Limits come from the device.** On connect the server reads the unit's own
`DEV:INF:LIM:*` values and validates every parameter against them before transmitting
anything, so a rejected request never leaves the instrument half-reconfigured. Output
power is additionally capped by `--max-stimulus-dbm`, which defaults to a conservative
−10 dBm regardless of what the hardware would permit.

**Bad data is labelled as bad.** Every measurement carries the instrument's live error
flags (ADC overload, PLL unlock, source unlevelled) and a calibration verdict:

- `valid` — the sweep matches the calibration
- `interpolated` — inside the calibrated range but at unmeasured points
- `invalid` — the sweep has moved outside the calibrated range
- `none` — no calibration is active

This matters more than it sounds. Changing the span after calibrating does not fail and
does not look wrong; the traces stay smooth and plausible. They are just no longer
accurate. `vna_analyze` also flags a feature too narrow for the current point spacing to
characterise, so an agent cannot report a confident Q derived from two samples.

**Results are summaries.** A 4501-point 2-port sweep is hundreds of kilobytes. The read
tools return derived values and a min/max **envelope**, deliberately not a stride sample —
subsampling would step straight over a narrow notch, which is usually the whole point of
the measurement. Full-resolution data goes to a Touchstone file via
`vna_export_touchstone`.

**Files are confined.** Calibration and Touchstone paths resolve inside a working
directory (`--workdir`), rejecting traversal, absolute paths and symlinks that escape.

## Install

```bash
cargo install mcp-librevna
```

You also need [LibreVNA-GUI](https://github.com/jankae/LibreVNA/releases) on the machine
with the hardware attached.

## Use with Claude Code

```bash
claude mcp add librevna -- mcp-librevna --workdir ~/vna-work
```

Or in `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "librevna": {
      "command": "mcp-librevna",
      "args": ["--workdir", "/home/you/vna-work", "--allow-destructive"]
    }
  }
}
```

Add `--allow-emission` only when something is actually connected to the ports that you
intend to drive.

## Options

| Flag | Env | Default | Meaning |
|---|---|---|---|
| `--host` | `LIBREVNA_HOST` | `127.0.0.1` | Machine running LibreVNA-GUI |
| `--port` | `LIBREVNA_PORT` | `19542` | SCPI port |
| `--gui-path` | `LIBREVNA_GUI_PATH` | — | LibreVNA-GUI binary, for `--spawn` |
| `--spawn` | `LIBREVNA_SPAWN` | off | Start a headless GUI if none is listening |
| `--mock` | — | off | Serve a simulated instrument; no hardware needed |
| `--workdir` | `LIBREVNA_WORKDIR` | cwd | Directory files are confined to |
| `--max-stimulus-dbm` | `LIBREVNA_MAX_STIMULUS_DBM` | `-10` | Output power ceiling |
| `--allow-emission` | `LIBREVNA_ALLOW_EMISSION` | off | Enable the emission tier |
| `--allow-destructive` | `LIBREVNA_ALLOW_DESTRUCTIVE` | off | Enable the destructive tier |
| `--allow-manual-hardware` | `LIBREVNA_ALLOW_MANUAL_HARDWARE` | off | Enable raw SCPI |
| `--timeout-s` | — | `10` | Ordinary SCPI exchange budget |
| `--sweep-timeout-s` | — | `120` | Sweep completion budget |

## Without hardware

`--mock` serves a simulated LibreVNA: a resonator notch at 2.44 GHz for the VNA, a carrier
above a noise floor for the spectrum analyser, and the limits of a real v1 unit. Readings
are synthetic and the server says so loudly on stderr, but the whole tool surface,
including every safety gate, behaves exactly as it does against hardware.

```bash
mcp-librevna --mock --workdir /tmp/vna
```

The same mock backs the integration tests, so `cargo test` needs no instrument.

## Device notes

- **Frequency**: 100 kHz – 6 GHz, 2 ports. Note the unit reports its own lower limit as
  0 Hz via `DEV:INF:LIM:MINF?`, below the specified range; the server trusts the device
- **Damage threshold**: +10 dBm, no input protection
- **Headless Qt**: the server sets `QT_QPA_PLATFORM=offscreen` when it spawns the GUI, so
  `--no-gui` works without a display
- **USB access**: the unit enumerates as `1209:4121`. Without a udev rule granting your
  user write access to it, LibreVNA-GUI starts but finds no device

## Protocol notes

Verified against LibreVNA-GUI v1.6.5. Two behaviours of the SCPI server shape the
transport, and neither is obvious from the programming guide:

**An unrecognised command is answered with silence**, not an error. A misspelt command
name is indistinguishable from a slow one, so it costs a full timeout and misaligns every
later exchange. Command names in this crate are ones observed to answer on hardware, and
the mock answers anything it does not model with silence too — so a wrong name fails in
CI rather than only against the instrument.

**`VNA:TRAC:TOUCHSTONE?` returns many lines**, terminated by a blank one, unlike every
other query. A 4501-point 2-port export is ~610 kB across a dozen TCP segments, so the
blank line is the only reliable end marker. It is also answered only for a complete
N-port matrix of traces: one trace or four, never two — an incomplete set gets silence,
so `vna_export_touchstone` rejects one before transmitting.

## Direct smoke test

`examples/smoke.rs` talks SCPI to the hardware without any MCP involved, and reports
whether this crate's response parsers agree with what the instrument actually sends:

```bash
cargo run --example smoke -- 127.0.0.1:19542
```

## Layout

| Path | Contents |
|---|---|
| `src/scpi/` | Transport and response parsing |
| `src/instrument.rs` | Connection state, sweep control, trace reads |
| `src/safety.rs` | Capability tiers, power ceiling, path sandbox |
| `src/device.rs` | Device-reported limits and error flags |
| `src/calibration.rs` | Calibration validity state machine |
| `src/analysis.rs` | Resonance, VSWR, bandwidth, envelope decimation |
| `src/server.rs` | The MCP tool surface |
| `src/mock.rs` | Simulated instrument, used by `--mock` and the tests |
| `examples/smoke.rs` | Direct hardware check, no MCP |

## Status

The SCPI command set is taken from the upstream programming guide. The response *formats*
for `VNA:TRAC:DATA?`, `VNA:TRAC:AT?` and `VNA:TRAC:TOUCHSTONE?` are documented only
loosely, so the parsers are deliberately tolerant and isolated in `src/scpi/parse.rs`.
Run `examples/smoke.rs` against a real unit to confirm them.

## License

MIT
