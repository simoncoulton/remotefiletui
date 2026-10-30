# remotefiletui

A terminal UI for browsing and transferring files over **SFTP**, **FTPS** (explicit TLS), **FTP** and
**Amazon S3 / S3-compatible** object stores (MinIO, Cloudflare R2, Wasabi, Backblaze B2, …).
Two panes (local | remote), keyboard driven (vim-style motions), non-blocking.

The screen is a **header** (what you're connected to, or which saved connection you're editing, with
the endpoint, the current remote path and the config folder), the two panes, and a one-row **footer**
with the most useful keys (a status message replaces it for a few seconds; keys that don't fit are
dropped, never the important ones). `?` lists everything.

**Homebrew**

```
brew tap simoncoulton/remotefiletui https://github.com/simoncoulton/remotefiletui
brew install remotefiletui
```

**From source**

```
cargo run --release        # or: cargo install --path . && remotefiletui
```

## Keys

**Connect screen** has two sections: **[S]aved Connections** and **[C]onnections** (the form).

| Key | Action |
| --- | --- |
| `Ctrl-B` or `S` | Jump to saved connections (`S` and `C` work outside text fields) |
| `Ctrl-N` or `C` | Jump to the connection form |
| `Tab` / `Shift-Tab`, `↑` `↓` | Move between fields |
| `←` `→` (on Protocol) | Change protocol |
| `Esc` | Leave a text field (so `?`, `q`, `S`, `C`, `j`, `k`, `i` work); with no field selected, `Esc` stops editing a saved connection |
| `Enter` | Connect |
| `Ctrl-S` | Save. A connection loaded from the list is **updated in place** (change the Name field to rename it); a new one asks for a name first (suggests the Name field, else the host). Saves host, port, user, key path, default path — **never the password** |
| `Ctrl-P` | Choose a [password manager](#password-managers) (None or 1Password) |
| `?` / `F1` | Help (`?` is typed normally inside text fields; `F1` always works) |
| `Ctrl-T` | Choose a colour theme |
| `q` | Quit, outside text fields (asks first) |
| `Ctrl-C` / `Ctrl-Q` | Quit from anywhere, including text fields (asks first; pressing it again confirms) |

**Saved connections list** (when it has focus)

| Key | Action |
| --- | --- |
| `j` `k` `↑` `↓`, `gg` `G`, `Ctrl-D` `Ctrl-U` (half page), `Ctrl-F` `Ctrl-B` | Move |
| `←` `→` `h` `l`, `Space`, `Enter` on a folder | Collapse / expand a folder (`h` on a closed folder goes to its parent) |
| `Enter` on a connection | Connect (its values are already shown in the form, so you can check them first) |
| `n` | **New** connection: blank form (also `Ctrl-L` from any field); `Ctrl-S` then asks for a name, so the selected entry is never overwritten |
| `e` | **Edit**: load it into the form (header and form title say *editing*, with a `(modified)` marker once you change something); `Ctrl-S` saves your changes |
| `y` | **Duplicate** the connection under a new name (`name copy`), ready to rename |
| `f` | New folder, inside the selected folder (folders nest to any depth) |
| `m` | Move a connection or a whole folder into another folder (or a new one) |
| `r` | Rename a connection or folder |
| `c` | Set the connection's **colour** |
| `t` | Edit the connection's **tags** |
| `/` | **Filter**: a search field (name/host/user/folder/bucket) above a picker of the tags in use |
| `Esc` | Clear the filter |
| `d`, `Del` | Delete a connection or folder (asks first; deleting a folder moves what is inside it up a level) |

**Browser**

| Key | Action |
| --- | --- |
| `Tab` / `Shift-Tab` | Cycle local pane, remote pane, activity log |
| `j` `k` `↑` `↓`, `gg` `G`, `Ctrl-D` `Ctrl-U` (half page), `Ctrl-F` `Ctrl-B` / `PgDn` `PgUp` | Move |
| `Enter` `→` `l` / `Backspace` `←` `h` `-` | Open directory / go up |
| `c` or `F5` | Copy the selected file to the other pane (upload or download) |
| `m` / `r` / `x` (`F7` / `F2` / `F8`) | Make directory / rename / delete |
| `.` | Show or hide dotfiles |
| `R` | Refresh |
| `Esc` | Cancel the running transfer |
| `t` | Choose a colour theme |
| `?` | Help |
| `q` | Disconnect |
| `Ctrl-C` | Quit (asks first) |

## Themes

`t` (or `Ctrl-T` anywhere) opens the theme picker with a live preview; `Enter` saves your choice to
`config.yml`, `Esc` restores the previous theme. Built in: `default` (your terminal's own colours),
`tokyo-night`, `catppuccin-frappe`/`-macchiato`/`-mocha`/`-latte`, `dracula`, `one-dark`,
`gruvbox-dark`, `nord`, `solarized-dark`, `rose-pine`, `monokai`, `github-dark`, `everforest`,
`kanagawa`.

Themes work like torrentui's:

- On start, each built-in without a file is written to `themes/<name>.yml` with every colour spelled
  out (`bg fg muted accent border selection_bg success info warning error special`), so you can edit
  or copy it. Any other `.yml` in `themes/` is a new theme named after the file. Colours are
  `#rrggbb`, an ANSI name (`red`, `darkgray`, `reset`) or `0`–`255`.
- A theme may set `base: <theme>` to inherit; unset colours come from it (or from the built-in of the
  same name, else `default`). The `themes:` map in `config.yml` works the same way and layers on top
  of the files.
- Names ignore case, accents and punctuation (`Catppuccin Frappé` = `catppuccin_frappe`).
- Mistakes (unknown keys, bad colours, unknown base, unknown `theme:`) are reported at start with the
  file name; the built-in themes keep working.
- Connection colours (red … gray) are mapped onto the active theme's palette.

**Transparent background:** `transparent_background: true` (the default) leaves the terminal's own
background, blur or image showing through while keeping the theme's text and accent colours. Press
`b` in the picker to toggle it with a live preview; it is saved with the theme. It has no effect on
`default`, which has no background of its own. 256-colour terminals (no `COLORTERM=truecolor`) get the
nearest palette entries.

## Activity log

A log panel above the footer records what happened, newest at the bottom, with timestamps:
connecting and the host key decision, the connected endpoint and the server's default directory,
the default path being opened (or why it fell back), every directory you open on either side with
its entry count, uploads and downloads (start, result, failures), mkdir/rename/delete, and lost or
closed connections. It is about the connection, not the UI (theme and filter changes aren't logged).
`K` scrolls back and `J` forward (`PageUp`/`PageDown` while typing in a form field). `Tab` also walks into the log (after the last form field, or after the remote pane): the border lights up and `j`/`k`, `gg`/`G`, `Ctrl-D`/`Ctrl-U` move a highlighted line; `Esc` or `Shift-Tab` goes back; the title shows
how far back you are, and new entries don't move a view you have scrolled. The panel appears when the
terminal is tall enough (the connect form needs 20 rows) and holds the last 500 lines.

## Editing saved connections

Moving through the saved list fills the **[C]onnections** form with the highlighted connection's
values, as a preview (the header shows its details too). `Enter` connects with them (no password; for
that, edit first). Press `e`, or `Tab`/`C` to move into the form, to edit: the header and form title
then say `Editing <name>`, with `(modified)` once you change something. `Ctrl-S` updates that entry in
place, keeping its folder, colour and tags; changing the **Name** field renames it (a name used by
another connection is refused). `y` duplicates the highlighted connection to start from, and `n`
(`Ctrl-L` inside a field) starts a blank connection. While you are editing, or have typed an unsaved
new connection, moving through the list leaves the form alone.

With no field selected, `Esc` stops editing and returns to a blank new connection. Anything that
would discard unsaved changes asks first (save / discard / cancel): stopping editing, editing or
copying another connection, starting a new one, connecting to another saved connection, or
connecting while the one you are editing has unsaved changes. `Enter` inside the form connects.

## Folders

Folders nest to any depth. `f` creates one inside the selected folder (or beside the selected
connection), `m` moves a connection or a whole folder, `r` renames, and deleting a folder moves what
is inside it up a level. Connections saved while a folder is selected are filed in it. Folder names
cannot contain `/` (a nested folder is stored as the path `Work/Prod`).

## Default path

**Default path** (last field) is the directory to open after connecting: absolute (`/var/www`),
`~/site` or relative to the server's default directory (`site/logs`). If it can't be opened, the
connection stays up in the default directory and the footer says why. For S3 use `/bucket/prefix`.

## Connection names

The **Name** field (above Protocol) is optional. Leave it empty and the connection is called by its
host (for S3: the bucket, else the endpoint). Saving offers that name and lets you change it; a name
already used by a *different* saved connection is refused. Loading a saved connection fills the Name.

## Password managers

`passwordmanager: onepassword` in `config.yml` (or `Ctrl-P`, which opens a selector listing None and
the managers supported, currently only 1Password) turns lookups on; leaving it unset means none is
used. With a manager selected:

- details you leave empty are filled in when you connect, from an item named after the connection
  (the Name field, else the Host), using the official [`op` CLI](https://developer.1password.com/docs/cli/):
  `op item get <name>`. Restrict the search with `onepassword_vault: Servers`;
- the **Host is optional**, both to connect and to save (it can come from the item), but you need a
  Name or a Host to say which item to look up.

Without one the header shows a warning: passwords are typed each time and are never written to
`profiles.yml`, the config or the log. (The older `onepassword: true` switch is still read as
`passwordmanager: onepassword`.)

| Field | Taken from the item |
| --- | --- |
| Username / Password | the username and password fields |
| Host / Port | fields labelled `host`/`hostname`/`server`/`address`, `port` (host falls back to the item's first URL) |
| Key file | a field labelled `key file`/`key path` (SFTP) |
| S3 | fields labelled `endpoint`, `region`, `bucket`, `access key id`, `secret access key`, `aws profile` |

Rules: **only empty fields are filled** (anything you typed wins), a key id is never paired with a
secret from a different source, and an item that doesn't exist simply leaves the fields empty, as
without 1Password. If `op` is missing, locked or signed out you get a warning and the connection
carries on with what you typed. Fetched values are used for that connection only: they are never put in
the form, the config or logs. `Esc` while connecting stops a lookup that's waiting for you to unlock.

## Colours, tags and filtering

Saved connections can be given a **colour** (red, green, yellow, blue, purple, cyan, gray) and any
number of **tags** (up to 10; letters, digits, `-`, `_`, `.`; stored lower-case).

- The colour shows as a `●` in the list, and marks the remote pane (border and title) while you are
  connected, so a red "production" connection is hard to mistake. Colours are mapped onto the active
  theme's palette, so they suit every theme.
- Filtering: `/` opens a search field with the tags in use listed below it (`Tab` switches between
  them; `Space` ticks a tag, `c` clears, `Enter` applies, `Esc` cancels). Every ticked tag must be on
  the connection, and every word of the search must appear in its name, host, user, folder or bucket.
  Tags are shown and typed without any `#`. While a filter is active, only folders containing matches
  are shown, expanded. The filter is not saved between runs.
- Re-saving a connection (Ctrl-S) keeps its folder, colour and tags.

## Amazon S3 and S3-compatible storage

Pick **Amazon S3 / S3-compatible** as the protocol. Fields:

| Field | Meaning |
| --- | --- |
| Endpoint | Empty for AWS. For MinIO, R2, etc. enter the URL (`https://…`; plain `host:port` is assumed to be https). Path-style addressing is used for custom endpoints. |
| Region | Empty uses your AWS config, falling back to `us-east-1`. A bucket in another region is detected automatically. |
| Bucket | Empty lists all buckets at the top level (needs permission to list them); set it to work inside a single bucket. |
| Access key ID / Secret key | Empty uses the standard AWS credential chain: environment variables, `~/.aws`, SSO, instance roles. The secret key is never saved. |
| AWS profile | Optional named profile from `~/.aws/config`. |

S3 has no real directories. A "folder" is a key prefix; `m` creates the usual empty `name/`
placeholder object. Rename is copy + delete and works on files only. Deleting a folder works only
when it is empty. Uploads over 8 MiB use multipart upload with progress per part; cancelling aborts
the upload on the server so no storage is left behind. At the top level (all-buckets mode), `m`
creates a bucket and `x` deletes an empty one.

## SFTP and `~/.ssh/config`

The host you type is resolved like `ssh <host>` would: `Host` blocks (wildcards, `!negation`),
`HostName`, `User`, `Port`, `IdentityFile`, `IdentitiesOnly`, `IdentityAgent` and `Include` are honoured, so an alias
from your config just works. Values typed in the form win; a port left at 22 defers to the config.
Saved connections keep the alias you typed.

## SFTP authentication

Like OpenSSH: with a key file set in the form, only that key is used (the password field is its
passphrase). Otherwise, in order:

1. a typed password,
2. the config's `IdentityFile`s (before the agent, so a server's attempt limit isn't spent on
   unrelated agent keys),
3. ssh-agent: the socket from `IdentityAgent` if set, else `SSH_AUTH_SOCK` (skipped by
   `IdentitiesOnly yes` or `IdentityAgent none`),
4. `~/.ssh/id_ed25519`, `id_ecdsa`, `id_rsa`, only if no `IdentityFile` is configured.

Passphrase-protected key files are skipped unless the typed passphrase unlocks them; the error
lists everything that was tried.

## Security behaviour

- **SFTP host keys** are checked against `~/.ssh/known_hosts`. An unknown key shows its SHA-256
  fingerprint and is only recorded if you accept. A *changed* key is a hard refusal (no prompt).
- **FTPS** verifies the server certificate against the OS trust store (honours `SSL_CERT_FILE` /
  `SSL_CERT_DIR`, so private CAs work). Untrusted certificates are refused; there is no
  "ignore errors" switch.
- **Plain FTP** is available but marked insecure in the UI.
- Passwords are held in memory only for the connection attempt, cleared from the form after a
  successful connect, and never written to disk or logs.
- Names from servers are sanitised before display (no terminal escape injection), and a name
  containing path separators or control characters is refused as a transfer target.
- Downloads go to `name.part` and are renamed on success; cancelled or failed transfers
  remove the partial file (locally for downloads, on the server for uploads).

## Configuration

Everything lives in `~/.config/remotefiletui/` on every platform (`$XDG_CONFIG_HOME/remotefiletui`
if that is set), as YAML:

| File | Contents |
| --- | --- |
| `config.yml` | Settings: `theme`, `transparent_background`, `passwordmanager`, `onepassword_vault`, and your own `themes:`. Created with every option commented on first run. |
| `profiles.yml` | Saved connections and folders (never passwords). Written by the app when you save. |
| `themes/*.yml` | One editable file per theme (see [Themes](#themes)). |

Changes made in the app (theme, transparency, password manager) edit just that line of `config.yml`, keeping
your comments and everything else. Unknown keys and invalid values are reported with the file name
instead of being ignored, and a file that doesn't parse is left untouched (nothing is saved over it
until you fix it).

**Upgrading:** earlier versions kept one `profiles.toml` (in `~/Library/Application Support/…`,
`~/.config/ftptui`, or here). On first run it is converted: connections and folders to
`profiles.yml`, settings to `config.yml` (unless you already have one), and the original is kept as
`profiles.toml.migrated`. Older copies found elsewhere are kept beside it as `profiles.toml.old-1`.
Nothing is deleted.

## Architecture

```
src/
  main.rs        terminal setup/teardown (panic hook, SIGTERM), event loop
  app.rs         state + input handling (connect form, panes, modals, transfers)
  ui.rs          rendering (pure function of state)
  worker.rs      background task that owns the connection; commands in, events out
  input.rs       single-line text input
  config.rs      config.yml + profiles.yml (in-place edits, TOML migration)
  theme.rs       palettes, theme files, 256-colour fallback
  ssh_config.rs  ~/.ssh/config resolution
  form.rs        the connection form as plain data + validation
  onepassword.rs 1Password lookup through the `op` CLI
  backend/
    mod.rs       `Backend` trait, shared transfer/cancel plumbing
    sftp.rs      russh + russh-sftp, known_hosts verification
    s3.rs        AWS SDK: buckets, prefixes as folders, multipart upload
    ftp.rs       suppaftp (FTP and explicit FTPS via rustls)
    local.rs     local directory listing
```

Adding a backend (S3, WebDAV, …) means implementing `Backend` and adding a `Protocol` variant.

## Current limitations

- Files only: no directory upload/download, and delete works on files and **empty** directories.
- S3: no folder rename, no copy between buckets in the UI, and objects in archive storage classes must be restored first.
- One file at a time; no multi-select, no resume of interrupted transfers.
- Commands are serialised per connection, so navigating while a transfer runs waits for it.
- FTPS is explicit (`AUTH TLS`) only; implicit FTPS (port 990) is not supported.
- `~/.ssh/config`: `ProxyJump`/`ProxyCommand` hosts are refused with an error, and `Match` blocks are ignored.
- No mouse support.

## Development

```
cargo test                 # unit tests
cargo clippy --all-targets
```
