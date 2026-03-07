# RustifyFlow

## Acknowledgements

This code was *lovingly* crafted with the help of **ChatGPT**. Yes, an AI helped make this. So if something doesn't work, feel free to blame the robots. But if it *does* work? Yeah, that was totally on purpose.

**RustifyFlow** is the *ultimate* PDF viewer you never knew you needed—because who doesn't want to *destroy* Adobe Acrobat Pro with a Rust program? Built with **pdfium-render** for high-fidelity PDF rendering and **Skia** for blazing-fast GPU-accelerated drawing, this tool is designed to be a *sleek* (or at least *functional*) alternative to the bloated giants of the PDF world. The old Python prototype days are over—welcome to the Rust era.

> **Note:** The previous Python versions (`acrobatprokiller.py`, `pdf31.py`, and all earlier iterations) have been archived under the [`Archive/`](./Archive/) folder for historical reference.

---

## Features

- **Fast PDF Rendering:** Hardware-accelerated rendering via OpenGL + Skia. It actually opens PDFs quickly.
- **Smooth Zoom:** Mouse-wheel zoom and `+`/`-` keyboard shortcuts. Reset to 100% with `0`.
- **Pan Support:** Click and drag to pan around large pages.
- **Page Navigation:** Use `←` / `→` arrow keys to flip through pages.
- **Zoom HUD:** A real-time zoom percentage is displayed in the bottom-right corner.
- **Lightweight & Efficient:** Written in Rust—memory-safe, fast, and no garbage collector in sight.

---

## Requirements

- [Rust toolchain](https://rustup.rs/) (stable, 1.70+)
- A copy of the **PDFium** shared library (`pdfium.dll` on Windows, `libpdfium.so` on Linux, `libpdfium.dylib` on macOS) placed next to the compiled binary or in a `libs/` subfolder.  
  Pre-built binaries are available from the [pdfium-binaries](https://github.com/bblanchon/pdfium-binaries) project.
- A GPU driver that supports **OpenGL**.

---

## Installation

### 1. Clone the Repository

```bash
git clone https://github.com/SQA2009/RustifyFlow.git
cd RustifyFlow
```

### 2. Place the PDFium Library

Copy the appropriate PDFium shared library into the project root (next to `Cargo.toml`), or into a `libs/` subfolder:

```
RustifyFlow/
├── pdfium.dll      <- Windows example
├── Cargo.toml
└── src/
    └── main.rs
```

### 3. Build the Project

```bash
cargo build --release
```

The compiled binary will be located at `target/release/rust-pdf-skia` (or `rust-pdf-skia.exe` on Windows).

---

## Usage

### 1. Place a PDF File

Put a PDF named `test.pdf` in the same directory as the compiled binary (or the project root when using `cargo run`).

### 2. Run the Application

```bash
cargo run --release
```

Or run the compiled binary directly:

```bash
./target/release/rust-pdf-skia
```

### 3. Navigation & Controls

| Action | Control |
|--------|---------|
| Next page | `→` (Right Arrow) |
| Previous page | `←` (Left Arrow) |
| Zoom in | `+` or `=` or scroll up |
| Zoom out | `-` or scroll down |
| Reset zoom & pan | `0` |
| Pan | Click and drag |

---

## Tutorial

### Step-by-Step Guide:

1. **Install the Rust toolchain** (if you haven't already):
   ```bash
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   ```

2. **Clone and enter the project:**
   ```bash
   git clone https://github.com/SQA2009/RustifyFlow.git
   cd RustifyFlow
   ```

3. **Add the PDFium library** to the project root (see Requirements above).

4. **Drop in a test PDF** named `test.pdf`.

5. **Build and run:**
   ```bash
   cargo run --release
   ```

6. **Navigate your PDF** using the keyboard and mouse controls listed above.

---

## Screenshots

No screenshots. This is an *adventure*, not a slideshow.

---

## Contributing

Feel like making this mess better? Go ahead, fork it. Submit a pull request. Or don't. I'm not your boss. But if you do, I might even look at it... eventually. Maybe.

---

## Credits

**RustifyFlow** is built on the shoulders of giants. The following open-source libraries and projects make it possible:

| Crate / Library | Description | Version |
|-----------------|-------------|---------|
| [**winit**](https://github.com/rust-windowing/winit) | Cross-platform window creation and event handling | 0.29 |
| [**glutin**](https://github.com/rust-windowing/glutin) | OpenGL context creation | 0.31 |
| [**glutin-winit**](https://github.com/rust-windowing/glutin) | Glutin + Winit integration helpers | 0.4 |
| [**raw-window-handle**](https://github.com/rust-windowing/raw-window-handle) | Cross-platform raw window handle abstraction | 0.5 |
| [**skia-safe**](https://github.com/rust-skia/rust-skia) | Rust bindings for the Skia 2D graphics library | 0.75 |
| [**gl**](https://github.com/brendanzab/gl-rs) | OpenGL function pointer loader | 0.14 |
| [**pdfium-render**](https://github.com/ajrcarey/pdfium-render) | Rust bindings for Google's PDFium library | 0.8 |
| [**PDFium**](https://pdfium.googlesource.com/pdfium/) | Google's open-source PDF rendering engine (bundled as `pdfium.dll`) | — |

---

## License

No license file yet. Use it, don't use it—I'm not here to police your life choices.
