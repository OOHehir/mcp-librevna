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
| `librecal_status` | — | Report an attached LibreCAL: firmware, oven temperature, port states |
| `librecal_verify` | measure | Prove the LibreCAL is cabled to the VNA and switching, and find the wiring |
| `vna_cal_auto` | destructive | Full SOLT calibration from a LibreCAL, using its own factory coefficients |
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

### The LibreCAL module

The optional [LibreCAL](https://github.com/jankae/LibreCAL) eCal module is driven
directly over its USB CDC-ACM interface rather than through LibreVNA-GUI. That is forced
rather than chosen: the GUI's LibreCAL support lives entirely in a Qt dialog, and its
`CALibration` SCPI node exposes nothing for it, so under `--no-gui` there is no way to
ask the GUI to drive the module.

Its framing is the **inverse** of the GUI's, which is worth holding in mind when working
across both. Every LibreCAL exchange answers with exactly one line: an empty line for
success, `ERROR` for a refusal, and `ERROR` for an unrecognised command too. A misspelt
name therefore fails at once instead of costing a timeout — and silence, which on the
GUI's SCPI port means "refused", here means the module has stopped answering. Bulk
responses are framed by literal `START` and `END` lines rather than a blank one.

Verified against firmware v0.3.0. The command set is discoverable from the module itself
with `*LST?`, which is more trustworthy than the published PDF.

### Proving the module is really connected

`librecal_verify` measures each module port terminated into LOAD, then OPEN, then SHORT,
and reports the mean reflection of each. The **change between LOAD and OPEN** is what
carries the proof, not either level alone:

| What is attached | LOAD | OPEN | Change | Verdict |
|---|---|---|---|---|
| The module, cabled and switching | −23.4 dB | −7.3 dB | 16.2 dB | controllable |
| Nothing — a bare port | −6.4 dB | −6.4 dB | ~0 dB | not connected |
| A fixed 50 Ω termination | −23.4 dB | −23.4 dB | ~0 dB | static termination |

Measured uncalibrated over 100 MHz – 6 GHz. A LOAD-only check would wave the fixed
termination through, and an OPEN-only check would pass a port with no cable at all, since
a disconnected port *is* an open. Only the pair separates them.

The same LOAD sweep reads both reflection parameters, so the wiring between module ports
and VNA ports is discovered rather than declared — a swapped pair would otherwise produce
a calibration that is wrong with no visible symptom.

### Calibrating from the module

`vna_cal_auto` runs the whole sequence: verify the connection, install the module's factory
coefficients as the GUI's calibration kit, measure open/short/load at each port and a
through across the pair, activate, then check its own work.

Installing the kit is the part that matters. Driving the module and calling
`VNA:CAL:MEASURE` produces a calibration either way, but with the GUI's default kit the
standards are treated as *ideal* — a perfect open, a perfect 50 Ω load — when the real
ones are nothing of the sort. That calibration completes cleanly, reports itself valid,
and is quietly inaccurate. So the coefficients are fetched over serial, written as
Touchstone into `librecal-standards/<serial>/`, loaded with `VNA:CAL:KIT:STAndard:<n>:FILE`,
and each measurement is bound to its standard by name with `VNA:CAL:ADD <type> <name>`.

Installing that kit **replaces whatever kit was loaded**, since a kit is one shared
namespace and leaving strangers in it invites a later measurement binding to the wrong
standard. The GUI keeps no copy, so the displaced kit is written to
`librecal-standards/<serial>/replaced-kit-<timestamp>.calkit` first and the path is
reported back.

Three behaviours of the GUI shape this, and each fails silently:

- **`VNA:CAL:MEASURE` does not measure.** It marks the entry pending; the *next* sweep
  fills it. Disconnect the standard before that sweep and the entry records whatever
  replaced it.
- **`VNA:CAL:BUSy?` is not a usable handshake.** The command exists, but on v1.6.5 it
  never reads true — sampled every 40 ms across a measurement it stays `FALSE` throughout,
  running or stopped. Polling it returns instantly, which reads as "finished". Both
  `vna_cal_auto` and `vna_cal_measure` drive a sweep and wait for that instead.
- **`VNA:CAL:ACTivate` takes port-qualified type names.** `VNA:CAL:ACTivate?` reports
  `OSL_1, OSL_2, OSL_12, SOLT_1, SOLT_2, SOLT_12, ThroughNormalization_12, TRL_12`. A bare
  `SOLT` is not among them and is answered with silence, leaving the calibration inactive
  while every command appears to have succeeded.

### Checking the calibration is real

A calibration built on ideal definitions activates and reads as valid exactly like one
built on the module's own data, so `vna_cal_auto` finishes by re-measuring each standard
through the finished calibration and comparing it against its own coefficients:

```
standard                             param   expected   measured  deviation
LibreCAL_5mV8QRceRywA_P1_LOAD          S11    -19.66dB   -19.62dB     0.04dB
LibreCAL_5mV8QRceRywA_P2_LOAD          S22    -20.11dB   -20.03dB     0.09dB
LibreCAL_5mV8QRceRywA_P12_THROUGH      S21     -3.11dB    -3.11dB    -0.00dB
```

A corrected standard reads back as its definition, **not** as zero — the load tracking its
real −19.66 dB reflection is the evidence the module's data is in use. Had the kit failed
to load, the GUI would have corrected that load towards a perfect match instead, and the
comparison would have caught it.

If a standard cannot be checked — its coefficient file has gone, or the sweep has moved
outside the range the coefficients cover — the tool fails rather than skipping it. A
skipped check would leave nothing to disagree with, and "everything passed" and "nothing
was checked" must not look the same.

Two limits worth stating. This is self-consistency, not independent verification: these
are the same standards the calibration was solved from, so it proves the calibration uses
the module's data, not that the module's factory data is accurate. And a through is
reciprocal, so S21 and S12 agree — the comparison cannot detect a transposed port order,
which is enforced by construction instead.

## Direct smoke test

`examples/smoke.rs` talks SCPI to the hardware without any MCP involved, and reports
whether this crate's response parsers agree with what the instrument actually sends:

```bash
cargo run --example smoke -- 127.0.0.1:19542
```

`examples/librecal_smoke.rs` does the same for the LibreCAL, exercising discovery,
status and the connection check against real hardware. It leaves every module port
released:

```bash
cargo run --example librecal_smoke
```

`examples/autocal_smoke.rs` runs a full automatic calibration and prints the residual
comparison above. It needs the destructive tier:

```bash
cargo run --example autocal_smoke -- <workdir>
```

## Layout

| Path | Contents |
|---|---|
| `src/scpi/` | Transport and response parsing |
| `src/instrument.rs` | Connection state, sweep control, trace reads |
| `src/librecal.rs` | LibreCAL discovery, transport and the connection check |
| `src/autocal.rs` | Calibration kit installation, the SOLT sequence and its residual check |
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
