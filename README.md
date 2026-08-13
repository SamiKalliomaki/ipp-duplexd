# ipp-duplexd — manual duplex printing via a virtual IPP printer

A small dependency-free Rust daemon that turns any simplex printer into a
manual duplex printer. It is itself an IPP printer, listening on
`127.0.0.1` only. Print to it; it prints the **odd pages** on the real
printer, pauses so you can flip the stack and feed it back in, then prints
the **even pages in reverse order** on the backs.

No PPDs and no CUPS backend involved — it registers as a modern driverless
(IPP Everywhere) queue, the architecture future CUPS 3.x mandates.

```
app --> CUPS queue (driverless) --> ipp-duplexd (127.0.0.1:6632)
                                       |  qpdf split
                                       |--> real printer: odd pages (1,3,5,...)
                                       |    (polls until finished, then stops
                                       |     with reason media-needed)
                                       |<-- you flip the stack, GET /flip
                                       '--> real printer: even pages reversed (...,4,2)
```

## Packages

```sh
./packaging/build-arch.sh   # Arch Linux: packaging/arch/ipp-duplexd-*.pkg.tar.zst  (needs makepkg)
./packaging/build-deb.sh    # Debian/Ubuntu: packaging/ipp-duplexd_*.deb            (needs dpkg-deb)
```

Both install `/usr/bin/ipp-duplexd`, the `ipp-duplexd-setup` helper, a systemd
**user** unit, and docs, and depend on `qpdf`.

## Setup

After installing (or `cargo build --release`), run:

```sh
ipp-duplexd-setup
```

It walks through the whole setup interactively: pick the real printer
from your CUPS queues (or enter an `ipp://` URI, which is probed),
optionally tweak the flip dialog / rotation / blank-page options, write
`~/.config/ipp-duplexd.conf`, enable and start the systemd user service
(writing a user unit if the packaged one is absent), and register the
virtual printer as a driverless CUPS queue. Every answer has a sensible
default; `--yes` runs non-interactively, `--no-service` / `--no-queue`
skip steps, and `--printer URI` / `--queue NAME` / `--listen ADDR`
preseed answers. See `ipp-duplexd-setup --help`.

To do the same by hand instead:

```sh
cp /usr/share/doc/ipp-duplexd/ipp-duplexd.conf.example ~/.config/ipp-duplexd.conf
$EDITOR ~/.config/ipp-duplexd.conf     # set --printer for your real printer
systemctl --user enable --now ipp-duplexd
lpadmin -p laser-duplex -E -v ipp://127.0.0.1:6632/ipp/print -m everywhere
```

The `lpadmin` line registers the virtual printer as a driverless CUPS
queue; pick any queue name, then print with `lp -d laser-duplex file.pdf`.
Best: point `--printer` at a **local CUPS queue** for the real printer
(`ipp://127.0.0.1:631/printers/laser`), so CUPS keeps doing any driver
work and TLS printers work too.

## Printing

Print to the virtual queue like to any other printer (or `lp -d
laser-duplex file.pdf`). The odd pages come out first; then the job
stops with `media-needed` and a desktop dialog opens. Flip the printed
stack, reload it, and confirm the dialog — the backs print. Without a
dialog (or from a script) the same is done with:

```sh
curl http://127.0.0.1:6632/flip
```

`GET http://127.0.0.1:6632/` shows job status.

## Building from source

Any Rust toolchain works and there are no crates to fetch; `qpdf` must
be installed. To try it in the foreground, point it at the real
printer's `ipp://` endpoint:

```sh
cargo build --release
./target/release/ipp-duplexd --printer ipp://127.0.0.1:631/printers/laser
```

For permanent use, set it up as described in [Setup](#setup) above
(`ipp-duplexd-setup` is built alongside as `target/release/ipp-duplexd-setup`,
and writes the user unit for you if the packaged one is absent).

## Formatting

Code is formatted with `rustfmt` (`cargo fmt`). If you use
[jj](https://jj-vcs.dev), `.jjconfig.toml` in the repo root has a `jj fix`
tool that formats every mutable commit. jj does not read tracked config
files on its own, so either pass it per invocation:

```sh
jj --config-file .jjconfig.toml fix
```

or append it to your repo config once after cloning, so plain `jj fix`
works:

```sh
JJ_EDITOR='tee -a' jj config edit --repo < .jjconfig.toml
```

(`jj config edit` opens the repo config in `$JJ_EDITOR`; `tee -a` appends
this file to it instead, leaving any repo config you already had intact.)

## Tests

`tests/e2e.rs` is an end-to-end suite (`cargo test`): each test starts
its own ipp-duplexd and a mock IPP printer in place of the real one (on
kernel-assigned ports, so they run in parallel) and plays CUPS as the
IPP client. Covered: attribute proxying and state mirroring (including
the unreachable-printer case), duplex splitting, blank padding,
rotation, copies, page ranges, one-sided pass-through, and the flip
dialog (via a stub zenity). Set `E2E_KEEP=1` to keep the temp dirs and
daemon logs of a failing run. Needs qpdf:

```sh
cargo test
```

## Options

| Flag | Default | Meaning |
|---|---|---|
| `--printer URI` | (required) | Real printer: `ipp://host[:port]/path` |
| `--listen ADDR:PORT` | `127.0.0.1:6632` | Listen address (warns if non-loopback) |
| `--name NAME` | `manual-duplex` | Printer name to advertise |
| `--blank WHERE` | `leading` | Blank-page padding of the even pass for odd page counts: `leading`, `trailing`, `none`. Switch if the last page of odd-count documents lands on the wrong sheet. |
| `--auto-continue N` | off | Print the backs N seconds after the odd pass finishes instead of waiting for `/flip` (takes precedence over `--gui`) |
| `--no-gui` | (gui on) | Disable the flip dialog. By default a desktop dialog (zenity, kdialog, or xmessage — first one found) opens when it is time to flip; clicking OK prints the backs. `/flip` keeps working in parallel; a dismissed or failed dialog never cancels the job, and without any dialog tool ipp-duplexd just waits for `/flip`. |
| `--rotate-even 0\|180` | `180` | Rotation of the even pages; set to `0` if the backs come out upside down for your flip direction |
| `--poll N` | `2` | Seconds between job-state polls of the real printer |

## Behavior details

- **Printer status is mirrored**: `printer-state`, `printer-state-reasons`
  and `printer-is-accepting-jobs` come from the real printer, so the
  virtual queue shows online/idle/processing/stopped (and supply alerts)
  exactly as the hardware reports them. If the real printer is
  unreachable the queue shows stopped with `offline-report`, but keeps
  accepting jobs — they print once it is back. ipp-duplexd's own duplex flow
  overrides this while waiting for a flip (`media-needed`).
- **Printer properties**: `Get-Printer-Attributes` responses are proxied
  from the real printer (cached 30 s), so the virtual printer advertises
  the real hardware's media sizes, resolutions, color modes, and so on.
  Identity, state, document formats, and `sides-supported` are overridden —
  manual duplex is what this printer adds.
- **Sides**: jobs with `sides=two-sided-*` (the advertised default) get the
  manual duplex treatment; `sides=one-sided` jobs pass through unchanged.
- **Options**: job attributes (`media`, `print-quality`,
  `printer-resolution`, `print-color-mode`, ...) are forwarded to the real
  printer for both passes. `copies` (replicated so one flip covers all
  copies, collated) and `page-ranges` are applied locally before the split.
  `number-up` is not forwarded — it would change the pagination underneath
  the split.
- **Odd page counts** get a blank page padded into the even pass so the
  sheet pairing stays aligned (see `--blank`).
- **Cancel** works mid-job: the remote pass is canceled best-effort.
- **Flip direction depends on your printer.** Odd-first + reversed evens is
  the standard scheme for face-up output stacks. Do one two-page test print
  to learn the right flip; use `--rotate-even 0` if backs are upside down.
- **`--gui` under systemd**: if the dialog does not appear, the user
  session's display variables are missing from the service environment;
  run `systemctl --user import-environment DISPLAY WAYLAND_DISPLAY` once
  (or add it to your session startup).

## Limitations

- Talks plain `ipp://` to the real printer — no TLS (`ipps://`). For
  TLS-only printers, forward via a local CUPS queue as shown above.
- PDF jobs only (`application/pdf`), which is what CUPS sends driverless
  printers by default.
- One document per job (no multi-document Send-Document sequences).
