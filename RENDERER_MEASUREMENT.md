# Renderer Phase 1 measurement

**Date:** 2026-08-10

**Environment:** macOS 15.3.2, MacBook Pro (Apple M4 Pro, 48 GB), WKWebView 605.1.15

**Command:** `npm run measure:renderer`

The opt-in WebDriverIO benchmark starts a fresh Tauri process for each backend, mounts 16 live
terminals, and reports the active renderer through the same typed diagnostic used by the regression
suite. The fixed throughput workload is 50,000 lines / 2,900,000 bytes.

| Measurement | Canvas | DOM |
| --- | ---: | ---: |
| 16-terminal RSS delta | 5.89 MiB | 6.17 MiB |
| 50k-line parse time | 55 ms | 55 ms |
| 50k-line parse + scheduled paint | 58 ms | 58 ms |
| Five in-place frame samples, mean | 855.4 ms | 900.2 ms |
| In-place frame p95 / max | 3000 / 3000 ms | 3001 / 3001 ms |
| xterm user-input boundary → echoed scheduled paint | 6 ms | 5 ms |

The frame distribution is a comparative smoke metric only. The embedded macOS driver reports the
document as hidden, and its document-start bridge replaces `requestAnimationFrame` with timers so
the application remains testable; hidden-page timer throttling creates the roughly three-second
outliers above. These values must not be read as foreground frame cadence. A foreground Claude Code
performance profile remains the authoritative live-TUI check.

## Decision

Canvas showed no measured user-visible deficit relative to DOM in this gate. It slightly improved the
fixed write completion and the noisy in-place-frame sample, while memory and input latency were within
run-to-run noise. Phase 2 WebGL is therefore **not justified**, and TUI detection remains deferred
because its only committed consumer is the unshipped WebGL corrective sweep.
