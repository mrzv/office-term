# officeterm (`oterm`)

Minimal terminal Office document viewer built on the ArtCraft collection (`deckcraft`, `wordcraft`, `gridcraft`) and the Kitty graphics protocol:

- **PowerPoint presentations**: `.pptx`, `.ppsx`, `.potx`, `.pptm`, `.ppsm`, `.potm`
- **Word documents**: `.docx`, `.docm`, `.dotx`, `.dotm`, `.doc`, `.dot`, `.rtf`, `.odt`
- **Excel spreadsheets**: `.xlsx`, `.xlsm`, `.xltx`, `.xltm`, `.xls`, `.xlt`, `.xlsb`, `.ods`, `.csv`, `.tsv`

## Requirements

- Rust 1.88+ (Edition 2024)
- A terminal that supports the Kitty graphics protocol and reports pixel dimensions cleanly (`kitty`, `Ghostty`, `WezTerm`)

## Build and Run

```sh
cargo run --release -- path/to/document.docx
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
| `b` | Jump back from a followed internal link |
| `Click` | Follow slide link under the pointer |
| `m` | Cycle fit mode (`fit-page`, `fit-width`, `fit-height`, `fit-content`) |
| `+`, `=` / `-` | Zoom in / out |
| `0` | Reset zoom to 100% |
| `i` | Toggle dark mode for slide rendering |
| `S` | Toggle smooth scrolling |
| `p` | Toggle performance timings in the status bar |
| `t` | Open table of contents / slide outline picker |
| `T` | Open color theme picker with live preview |
| `y` | Open slide text viewer to select and copy text |
| `f` | Open presentation picker in a new tab |
| `D` | Duplicate active tab at the current view |
| `Tab` / `Shift-Tab`, `Alt-1` … `Alt-9` | Switch tabs |
| `?` | Open keyboard and mouse shortcuts help menu |
| `q` | Leave current mode or close active tab |
| `Esc`, `Ctrl-c` | Leave current mode, clear search, or exit |
