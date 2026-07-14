# docx-sanitizer

A structure-preserving sanitization tool for `.docx` files. It strips sensitive
payload — visible text, tracked-change content, comments, revision authors,
document properties, and external hyperlink targets — while keeping the
original OOXML package structure, section layout, styles, and formatting
intact.

Unlike a "convert to plain text and back" approach, `docx-sanitizer` edits the
underlying XML at the byte-span level: only the payload ranges it recognizes
are replaced, everything else (attribute order, whitespace, entity encoding,
self-closing tag style) is copied through unchanged.

## Why

Problematic Word documents often need to leave the organization that owns
them — for customer support investigations, parser/rendering bug reports, or
regression fixtures — but can't be shared as-is because they contain
confidential text, metadata, comments, revision history, or embedded
business data.

The usual workarounds are slow and lossy: manually deleting content,
rebuilding a minimal example by hand, or copying visible text into a new
file. Each of these tends to destroy the very structure needed to reproduce
the original issue. `docx-sanitizer` automates producing a structurally
faithful, confidentiality-safe artifact instead — safe to attach to a ticket
or send to a vendor, while still exercising the same parser/renderer code
paths as the original.

## Installation

Install from crates.io:

```sh
cargo install docx-sanitizer
```

This installs the `docx-sanitizer` command.

## Usage

### `sanitize` — produce a sanitized `.docx`

```sh
docx-sanitizer sanitize input.docx --output output.docx
```

By default this runs in **strict mode**: if the document contains any content
the tool cannot confidently classify or sanitize (e.g. `word/customXml/`,
`word/media/`, `word/embeddings/`, or an unrecognized field instruction), it
refuses to write output and exits with an error instead of silently producing
a document that looks sanitized but isn't.

### `inspect` — see what's inside a package

```sh
docx-sanitizer inspect input.docx
```

Lists every part in the package with its classification (kind) and support
tier (`guaranteed`, `best-effort`, or `unsupported`).

### `report` — see what a sanitize run would do, without writing output

```sh
docx-sanitizer report input.docx
docx-sanitizer report input.docx --report-json report.json
```

Prints (or writes) a JSON report describing the sanitize outcome, per-part
status, and any unsupported-content concerns, without touching the input
file.

## Policy flags

These flags are shared by `sanitize` and `report`:

| Flag | Description |
|---|---|
| `--best-effort` | Produce output even if unsupported content is present, instead of refusing (the default, strict behavior). |
| `--include <categories>` | Only sanitize these comma-separated categories: `headers`, `footers`, `comments`, `footnotes`, `endnotes`, `docprops`, `revisions`. `word/document.xml` is always sanitized regardless. Conflicts with `--exclude`. |
| `--exclude <categories>` | Sanitize every category except these. Conflicts with `--include`. |
| `--mode <mode>` | Replacement strategy for visible/revision text (`w:t`/`w:delText`): `preserve-length` (default), `constant`, or `clear`. Does not affect author/initials/date or document properties, which have their own fixed canonical replacements. |
| `--remove-track-changes` | Collapse tracked changes to their accepted state before sanitizing (deleted text removed, inserted text kept and unwrapped). Off by default — track-changes structure is preserved. |
| `--strip-media` | Replace `word/media/*` images with a fixed placeholder instead of leaving them as unsupported content. Only `png`/`jpg`/`jpeg`/`gif`/`bmp` are covered; other formats (e.g. `emf`/`wmf`) remain unsupported regardless. Works independently of `--best-effort` — with this flag, strict mode no longer blocks on media with a supported extension. |
| `--sanitize-customxml` | Replace text-node payload in `word/customXml/*` parts in place, regardless of schema, instead of leaving them as unsupported content. Text alongside a child element (mixed content) is left untouched and still blocks strict mode if found. Works independently of `--best-effort`. |

Note: unsupported *part classes* (`word/customXml/`, `word/media/`,
`word/embeddings/`) are not part of the `--include`/`--exclude` vocabulary —
their presence always blocks strict mode regardless of scope. `--best-effort`
allows producing output despite them (leaving that content untouched);
`--strip-media` and `--sanitize-customxml` each resolve their respective
part class by rewriting it in place rather than leaving it untouched or
requiring `--best-effort`.

### Examples

```sh
# Strict sanitize, default preserve-length replacement
docx-sanitizer sanitize input.docx --output output.docx

# Allow output even if unsupported content is present
docx-sanitizer sanitize input.docx --output output.docx --best-effort

# Only sanitize comments and revision metadata; leave headers/footers/etc. untouched
docx-sanitizer sanitize input.docx --output output.docx --include comments,revisions

# Replace visible text with a constant placeholder instead of preserving length
docx-sanitizer sanitize input.docx --output output.docx --mode constant

# Collapse tracked changes to their accepted state first
docx-sanitizer sanitize input.docx --output output.docx --remove-track-changes

# Replace images with a placeholder instead of blocking strict mode on them
docx-sanitizer sanitize input.docx --output output.docx --strip-media

# Sanitize customXml text payload instead of blocking strict mode on it
docx-sanitizer sanitize input.docx --output output.docx --sanitize-customxml

# Check what would happen without writing a file
docx-sanitizer report input.docx --report-json report.json
```

## License

MIT — see [`LICENSE`](LICENSE).
