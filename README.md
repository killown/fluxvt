# fluxvt

> A lightweight terminal emulator built with pure wgpu, winit, and Rust.

---

## Features

- **ANSI Compatibility:** Full ANSI escape sequence support.
- **Color Depth:** `xterm-256color` compatibility.
- **Directory Tracking:** OSC 7 working directory tracking.
- **Clipboard Integration:** Selection and copy/paste (`Ctrl`+`Shift`+`C` / `Ctrl`+`Shift`+`V`)[cite: 1].
- **History Retention:** Built-in scrollback buffer.
- **Customizable:** Configurable colors and font options.

---

## Building

Ensure you have the Rust toolchain and the necessary system dependencies for `wgpu` installed, then compile the optimized release build:

```bash
cargo build --release
```
