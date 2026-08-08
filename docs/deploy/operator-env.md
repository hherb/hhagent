# Operator environment settings

`kastellan-cli install` **regenerates** `~/.config/kastellan/kastellan.env` from
its CLI flags every time it runs. Any key you add to that file is dropped on the
next deploy, and any value you tune there reverts to the flag default.

Put operator settings in `~/.config/kastellan/kastellan.env.local` instead.

- The installer **never writes** that file.
- Its values **win**: both backends apply environment files in order, later
  overriding earlier, and the overlay is listed second.
- It is optional — if it does not exist, nothing changes.
- Give it `0600` permissions, like the file it sits beside.

## Example

```sh
cat > ~/.config/kastellan/kastellan.env.local <<'EOF'
KASTELLAN_LLM_LOCAL_MODEL=gemma4:26b-a4b-it-q8_0-ctx64k
KASTELLAN_MAIL_ENDPOINT=https://10.0.0.3:8443
KASTELLAN_MAIL_TOKEN_FILE=/home/you/.config/kastellan/mail-token
KASTELLAN_MATRIX_PEERS=@you:your-homeserver
EOF
chmod 600 ~/.config/kastellan/kastellan.env.local
```

Every key above is one the installer has no flag for, or one it would reset —
which is exactly why they belong here rather than in `kastellan.env`.

## What the file may contain

The grammar is systemd's `EnvironmentFile=`, and the macOS backend implements
the same subset, so one overlay behaves identically on both platforms:

| you write | the daemon gets |
| --- | --- |
| `A=plain` | `plain` |
| `A="a b"` or `A='a b'` | `a b` — surrounding quotes are stripped |
| `A='  x  '` | `  x  ` — whitespace *inside* quotes is kept |
| `A=  c  ` | `c` — whitespace outside them is trimmed |
| `A=f"g` | `f"g` — a quote that is not the first character is literal |
| `A="a` | `a` — a leading quote is stripped even with no closing one |
| `# A=x`, `; A=x` | ignored, both are comments |
| `export A=x` | **ignored** — write `A=x`, not shell syntax |

Backslash line-continuations and C-style escapes inside quotes are *not*
supported; a value needing either is out of contract on both platforms.

## When it takes effect

| platform | mechanism | an edit takes effect |
| --- | --- | --- |
| Linux (systemd) | a second `EnvironmentFile=` directive | next `systemctl --user restart kastellan-core` |
| macOS (launchd) | folded into the plist at install time | next `kastellan-cli install` |

launchd has no `EnvironmentFile=` directive, so the values are baked in when the
plist is written. Surviving a reinstall works identically on both platforms; only
the refresh moment differs.

**On macOS, always re-run `install` with the same flags you passed originally.**
A bare `kastellan-cli install` regenerates `kastellan.env` from flag *defaults*,
so it reverts `--llm-model`, `--llm-url`, `--embedding-model`, `--pg-bin-dir`,
and drops the whole `KASTELLAN_MATRIX_*` block (which is emitted only when both
`--matrix-*` flags are passed). On Linux `scripts/upgrade_from_git.sh` scavenges
the Matrix block back before installing; the macOS path has no such guard.

## If an install reports dropped keys

```
warning: install is regenerating /home/you/.config/kastellan/kastellan.env
  dropped: KASTELLAN_MAIL_ENDPOINT
  changed: KASTELLAN_LLM_LOCAL_MODEL
  previous file saved to /home/you/.config/kastellan/kastellan.env.bak
  to keep these across future installs, move them into /home/you/.config/kastellan/kastellan.env.local —
  the installer never writes that file, and its values override this one;
  then `systemctl --user restart kastellan-core` to pick them up.
```

(The last line differs on macOS, where it tells you to re-run `install` instead.)

Those keys were living in the generated file rather than in the overlay, which is
why the regeneration reached them. Copy them out of the `.bak` into
`kastellan.env.local`, then apply it as the table above describes.

**Move them now, not later.** The backup is written only when something is
actually being lost, so an ordinary clean install will not touch it — but a
*second* install that also drops or changes keys writes its own backup, and by
then the live file is the already-stripped one. To keep the copy that matters,
the installer never overwrites an existing backup: the first one stays at
`kastellan.env.bak` and later ones land beside it as `kastellan.env.bak.1`,
`.bak.2`, and so on. The warning always names the file it actually wrote. Prune
them yourself once the values are safely in the overlay.

If the installer cannot *read* `kastellan.env` at all — a stray non-UTF-8 byte,
wrong permissions — it refuses to continue rather than overwriting a file it
could neither diff nor back up. Move the file aside and re-run.

## Before you `uninstall --purge`

`--purge` deletes `~/.config/kastellan/` outright — that is the directory
`kastellan.env.local` and every `kastellan.env.bak*` live in, so they all go with
it. The installer's own health check points operators at `uninstall --purge`
then reinstall as the fix for a stale-cluster role mismatch; the purge summary
names both files, but by then it is too late. Copy anything you need out of
`kastellan.env.local` (and the backups, if you have not already moved their
contents over) before running `--purge`.
