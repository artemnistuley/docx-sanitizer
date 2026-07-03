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

Note: unsupported *part classes* (`word/customXml/`, `word/media/`,
`word/embeddings/`) are not part of the `--include`/`--exclude` vocabulary —
their presence always blocks strict mode regardless of scope. Only
`--best-effort` allows producing output despite them (leaving that content
untouched in the output).

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

# Check what would happen without writing a file
docx-sanitizer report input.docx --report-json report.json
```

## License

MIT — see [`LICENSE`](LICENSE).
