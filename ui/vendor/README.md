# Vendored xterm artifacts

These browser bundles are pinned because Buoy loads xterm as globals before the strict-TypeScript
frontend starts. Update the package, version, and SHA-256 together; renderer addons must remain
compatible with the pinned xterm core.

| File | Upstream package | Version | SHA-256 |
| --- | --- | --- | --- |
| `xterm.js` | `@xterm/xterm` | 5.5.0 + upstream #5024 | `c365e94f10448c3b6cedf260b42632162c35b77e84d6ad0eb7054fc34b2a0b78` |
| `xterm.css` | `@xterm/xterm` | 5.5.0 | `ba8e6985669488981ccf40c0cefe3aba80722cb6c92de7ad628b0bd717faf2b6` |
| `addon-fit.js` | `@xterm/addon-fit` | 0.10.0 | `bdaefa370b1bfc42ee88d46fe6072400902a4d4b2d45cd93438dda9b23c97089` |
| `addon-canvas.js` | `@xterm/addon-canvas` | 0.7.0 | `7b3e904d5bec98b54d26674994cf994396c4af0971010ffd0d4983229b65d933` |

All four packages are distributed by the xterm.js project under the MIT license. The CSS and addon
files are unmodified published browser artifacts. `xterm.js` carries the one-line upstream
composition-boundary fix from xtermjs/xterm.js#5024 (commit `887e5a6`), released with xterm 6.0;
keeping that fix on 5.5 preserves Buoy's compatible canvas renderer, which xterm 6 removed.
