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
KASTELLAN_LLM_TIMEOUT_MS=180000
KASTELLAN_MAIL_ENDPOINT=https://10.0.0.3:8443
KASTELLAN_MAIL_TOKEN_FILE=/home/you/.config/kastellan/mail-token
EOF
chmod 600 ~/.config/kastellan/kastellan.env.local
```

## When it takes effect

| platform | mechanism | an edit takes effect |
| --- | --- | --- |
| Linux (systemd) | a second `EnvironmentFile=` directive | next `systemctl --user restart kastellan-core` |
| macOS (launchd) | folded into the plist at install time | next `kastellan-cli install` |

launchd has no `EnvironmentFile=` directive, so the values are baked in when the
plist is written. Surviving a reinstall works identically on both platforms; only
the refresh moment differs.

## If an install reports dropped keys

```
warning: install is regenerating /home/you/.config/kastellan/kastellan.env
  dropped: KASTELLAN_MAIL_ENDPOINT
  changed: KASTELLAN_LLM_LOCAL_MODEL
  previous file saved to /home/you/.config/kastellan/kastellan.env.bak
```

Those keys were still in the generated file. Copy them out of the `.bak` into
`kastellan.env.local` and restart. The backup is only written when something is
actually being lost, so it is not overwritten by a later clean install.
