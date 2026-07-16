# halftone

Turn images into ANSI/ASCII art, right in your terminal.

`halftone` converts any image to a colored (or monochrome) character-art
rendering, using histogram equalization and Floyd-Steinberg dithering to
preserve detail that naive brightness-to-glyph mapping throws away. It can
also generate the source image for you from a text prompt via OpenAI's
`gpt-image-1`, so you can go from an idea to terminal art in one command.

## Features

- **Detail-preserving conversion** — histogram equalization spreads out
  clustered midtones instead of collapsing them into one or two glyphs, and
  Floyd-Steinberg dithering fakes far more tonal range than the glyph ramp
  actually has, so fine structure survives instead of blobbing out.
- **Correct terminal proportions** — corrects for terminal cells being
  roughly twice as tall as they are wide, so output isn't vertically
  stretched.
- **True color output** — each glyph is colored with its cell's actual
  sampled RGB, so hue differences that share a luminance (green skin vs. tan
  leather vs. brown wood) don't get flattened away. `--mono` gives you plain
  grayscale ASCII instead.
- **Text-to-art generation** — skip the source image entirely and describe
  what you want; `halftone generate` calls OpenAI's image API and pipes the
  result straight into the same conversion pipeline.

## Install

Requires a [Rust toolchain](https://rustup.rs/).

```sh
git clone <this-repo>
cd halftone
cargo build --release
```

The binary will be at `target/release/halftone`.

## Usage

### Convert an existing image

```sh
halftone convert path/to/image.png
```

### Generate an image from a prompt, then convert it

```sh
export OPENAI_API_KEY=sk-...
halftone generate "a neon-lit alley cat in the rain" --size landscape
```

### Options

Available on both `convert` and `generate`:

| Flag             | Description                                      | Default |
| ---------------- | ------------------------------------------------- | ------- |
| `--width <N>`     | Output width in characters                        | `80`    |
| `--out <path>`    | Write output to a file (also prints to stdout)     | —       |
| `--mono`          | Emit plain grayscale ASCII instead of ANSI color   | off     |

`generate` only:

| Flag                  | Description                                              | Default  |
| --------------------- | ---------------------------------------------------------- | -------- |
| `--size <shape>`       | `square` (1024x1024), `landscape` (1536x1024), or `portrait` (1024x1536) | `square` |
| `--save-image <path>`  | Also save the raw generated image, before conversion       | —        |

## How it works

1. The source image is resized directly to the target character grid
   (`width` × the aspect-corrected `height`), letting the resize filter do
   the per-cell averaging.
2. Each cell's luminance is computed (Rec. 709 weights) and remapped through
   the image's own cumulative luminance distribution — an equalization step
   that spreads out narrow midtone clusters instead of leaving them crushed
   into a couple of glyphs.
3. Equalized values are quantized to the glyph ramp (`` .:-=+*#%@``) with
   Floyd-Steinberg error diffusion, the same trick offset-printing halftones
   use to fake tone levels beyond what the ink (or in this case, the glyph
   ramp) actually offers.
4. Unless `--mono` is set, each glyph is wrapped in a 24-bit ANSI color
   escape using that cell's sampled RGB.

## Requirements

- `OPENAI_API_KEY` environment variable, only for `generate`.
- A true-color-capable terminal to see ANSI output as intended (most modern
  terminal emulators qualify).
