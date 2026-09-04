# Changelog

## 0.1.0

- Added `librevna_connect`, `librevna_status` and `librevna_list_devices` to attach to a LibreVNA through LibreVNA-GUI and report the unit's own limits
- Added `vna_configure_sweep`, `vna_sweep`, `vna_read`, `vna_marker` and `vna_analyze` for S-parameter measurement, returning resonance, return loss, VSWR, bandwidth and Q
- Added `vna_trace_list` and `vna_trace_manage` to create, rename, pause, resume and re-parameterise traces
- Added `vna_cal_status`, `vna_cal_measure`, `vna_cal_activate`, `vna_cal_load`, `vna_cal_save` and `vna_cal_reset` for SOLT and related calibrations
- Added `sa_configure`, `sa_sweep` and `sa_read` for spectrum analysis, reporting a peak table and an estimated noise floor
- Added `gen_configure` and `gen_off` to drive a continuous carrier from either port
- Added `vna_export_touchstone` to write full-resolution data as Touchstone, since the read tools return summaries
- Added `scpi_raw` for arbitrary SCPI including the `MANUAL:` subsystem. **Note:** firmware update (`DEV:UPDATE`) is refused at every tier, `scpi_raw` included
- Added capability tiers gating anything with a consequence behind `--allow-emission`, `--allow-destructive` and `--allow-manual-hardware`, each refusal naming the flag that would permit it
- Added `--max-stimulus-dbm` to cap output power independently of what the hardware permits, defaulting to -10 dBm against a +10 dBm damage threshold
- Added a calibration verdict (`valid`, `interpolated`, `invalid`, `none`) to every measurement, so a sweep moved outside its calibrated range is not reported as accurate
- Added live device error flags (ADC overload, PLL unlock, source unlevelled) to every measurement result
- Added `--workdir` confining calibration and Touchstone paths, rejecting traversal, absolute paths and escaping symlinks
- Added `--mock`, serving a simulated instrument so the whole tool surface runs without hardware
- Added `--spawn` with `--gui-path` to start a headless LibreVNA-GUI, attaching to a running one first
