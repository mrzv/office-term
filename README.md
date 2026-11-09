# officeterm (`oterm`)

Minimal terminal Office document viewer built on the ArtCraft collection (`deckcraft`, `wordcraft`, `gridcraft`) and the Kitty graphics protocol:

- **PowerPoint presentations**: `.pptx`, `.ppsx`, `.potx`, `.pptm`, `.ppsm`, `.potm`
- **Word documents**: `.docx`, `.docm`, `.dotx`, `.dotm`, `.doc`, `.dot`, `.rtf`, `.odt`
- **Excel spreadsheets**: `.xlsx`, `.xlsm`, `.xltx`, `.xltm`, `.xls`, `.xlt`, `.xlsb`, `.ods`, `.csv`, `.tsv`

## Requirements

- Rust 1.95+ (Edition 2024; required by the locked font dependencies)
- A terminal that supports the Kitty graphics protocol and reports pixel dimensions cleanly (`kitty`, `Ghostty`, `WezTerm`)

## Build and Run

```sh
cargo run --release -- path/to/document.docx
```

Run the application and vendored-library regression suites:

```sh
cargo test --locked --workspace --all-targets
```

Or launch `oterm` without a path to pick an Office document from the current directory tree:

```sh
cargo run --release
```

Start on a specific 1-based page/slide:

```sh
cargo run --release -- --page 3 path/to/deck.pptx
```

Print the active configuration as JSON (creating the default config file if missing):

```sh
cargo run --release -- --print-config
```

Capture a PNG screenshot from a running named session:

```sh
# In terminal 1:
oterm --session demo path/to/deck.pptx

# In terminal 2:
oterm --session demo --screenshot page.png
```

Relative screenshot paths resolve from the requesting command's working directory,
not the viewer's. Existing destination files are never overwritten.

Stop that session from another terminal:

```sh
oterm --session demo --quit
```

The quit command waits for acknowledgment and exits without opening a document or
entering terminal mode. The viewer removes its socket during normal shutdown.

## Rendering and document fidelity

- Saved spreadsheet formula results are preserved, including results from custom
  functions that the local calculation engine cannot evaluate.
- Sparse worksheets retain populated regions beyond large empty gaps. Blank-cell
  formatting is rendered without expanding entire worksheet ranges into memory.
- Spreadsheet pictures support PNG, JPEG, GIF, WebP, and BMP. Conditional-format
  icons remain visible when the rule hides the cell's numeric value.
- Embedded PowerPoint and Word font subsets are isolated between documents.
  Wrapped hyperlinks are clickable only on their linked text segments.
- PowerPoint EMF+ figures render even when marked dual-format but missing a drawable
  EMF fallback. Existing drawable EMF fallbacks remain preferred.
- Fit-content includes Word freeform drawings and ink, and ignores PowerPoint
  objects parked completely outside the slide.
- An open link browser refreshes after document reload. While editing its filter,
  `q` is ordinary query text; `Esc` still leaves the mode.
- Opening, switching, and closing tabs keep the outgoing frame visible until its
  replacement is ready, then replace it in one synchronized redraw.
- Internal-link navigation has per-tab back/forward history, including scroll
  position. Reloads retain that history and clamp destinations to the new page count.

## Configuration and stored state

Configuration lives at `$XDG_CONFIG_HOME/officeterm/config.toml`, or
`~/.config/officeterm/config.toml` when `XDG_CONFIG_HOME` is unset. IPC socket
length and private-directory requirements apply only when `--session` enables
viewer control; ordinary viewing does not create the socket directory.

Press `R` to reload the configuration and external themes in the running viewer.
Invalid configuration, theme files, or an unavailable replacement socket leave the
current settings and listener intact and show an error in the status bar. Changing
the socket setting moves a named session's listener; viewers started without
`--session` remain uncontrolled. After changing that setting, other commands must
read the same configuration to find the session.

Reloading preserves manually selected fit and dark modes unless their configured
defaults changed. A CLI file-size override remains authoritative after reload;
without one, the new configured limit applies to subsequent opens and reloads.
Setting `theme = ""` restores the built-in palette even when an external theme
catalog is configured.

`max_file_size_mib = 256` limits document size on disk before reading the file.
Set a positive integer MiB value in the configuration, or override it for one
viewer process:

```sh
oterm --max-file-size-mib 1024 path/to/large-document.xlsx
```

The CLI value takes precedence without modifying the configuration file. The
same limit applies to startup, file-picker opens, links, and reloads. It is not
a memory budget: parsing and decompression can use substantially more memory
than the file size. Importer and image-decoding safeguards remain unchanged.

Recent-file history stores Unix path bytes in JSON, preserving newlines and
non-UTF8 filenames. The previous newline-delimited history is ignored and replaced
when a document is next recorded.

The vendored `wordcraft-doc` crate is pinned to upstream revision
`ff3c7d8d25bd8271326f372cb90f2c8106ec4d92`, with local fixes for embedded symbol
fonts, numbering restarts, legacy-form XML text, equation breaks, locale-aware
collation, and rotated connector rerouting. The vendored `deckcraft-fonts` and
`deckcraft-metafile` crates are pinned to `354087ecd56305842582e9e410cf24d595849789`.
Local patches add document-local font-family registration without changing embedded
font bytes, and replay EMF+ when a dual-format metafile has no drawable EMF fallback.
The `gridcraft-xlsx` importer and `gridcraft-calc` engine are pinned to
`8ae67c5534f3d7f145423a57206cf5fd08fe9221`. The importer preserves saved array
and spill results, releasing their cached followers only when an anchor needs
calculation. The viewer calculates only missing results without hiding formula
metadata or recalculating saved results.

## Controls

| Key | Action |
| --- | --- |
| `j` / `k`, `Down` / `Up`, `Mouse wheel` | Scroll vertically |
| `h` / `l`, `Left` / `Right` | Pan horizontally (or switch focus between slide and split pane) |
| `Space`, `PageDown` | Page viewport forward (or next slide) |
| `Backspace`, `PageUp` | Page viewport backward (or previous slide) |
| `g` / `G`, `Home` / `End` | First / last slide (or first / last item in split pane) |
| `:` | Open go-to-slide prompt |
| `/` | Search presentation text (or filter/start a new search in split pane) |
| `n` / `N` | Next / previous search match |
| `Enter` | Browse slide links on the current slide (or open selected link/match) |
| `L` | Toggle link mode and split link browser |
| `s` / `a` | Cycle split layout / restore automatic split layout |
| `b` / `B` | Jump backward / forward through internal-link history |
| `Click` | Follow slide link under the pointer |
| `m` | Cycle fit mode (`fit-page`, `fit-width`, `fit-height`, `fit-content`) |
| `+`, `=` / `-` | Zoom in / out |
| `0` | Reset zoom to 100% |
| `i` | Toggle dark mode for slide rendering |
| `S` | Toggle smooth scrolling |
| `p` | Toggle performance timings in the status bar |
| `R` | Reload configuration and external themes |
| `t` | Open table of contents / slide outline picker |
| `T` | Open color theme picker with live preview |
| `y` | Open slide text viewer to select and copy text |
| `f` | Open presentation picker in a new tab |
| `D` | Duplicate active tab at the current view |
| `Tab` / `Shift-Tab`, `Alt-1` … `Alt-9` | Switch tabs |
| `?` | Open keyboard and mouse shortcuts help menu |
| `q` | Leave current mode or close active tab |
| `Esc`, `Ctrl-c` | Leave current mode, clear search, or exit |

Unexpected Ctrl/Alt/Super-modified character shortcuts are ignored rather than
being treated as ordinary letters. `Ctrl-o` also navigates backward; `Ctrl-i`
navigates forward when the terminal reports it distinctly from `Tab`. Use `B`
for forward navigation in terminals where `Ctrl-i` is indistinguishable from `Tab`.
