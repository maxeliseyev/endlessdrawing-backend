#!/usr/bin/env bash
# Owner-only: print the private age identity so you can copy it into your
# password manager. Refuses to run when stdout is not an interactive terminal,
# so an agent/CI/log can never capture the secret. Never writes a copy to disk.
set -euo pipefail
identity=${AGE_IDENTITY:-$HOME/.config/age/construct-backup.key}
age=${AGE_BIN:-$(command -v age-keygen || command -v age)}

[[ -n $age ]] || { printf '%s\n' 'age CLI not found on PATH' >&2; exit 1; }
[[ -f $identity ]] || { printf '%s\n' 'age identity file not found' >&2; exit 1; }
[[ ! -L $identity ]] || { printf '%s\n' 'Refusing a symlink identity' >&2; exit 1; }
[[ -r $identity ]] || { printf '%s\n' 'identity is not readable' >&2; exit 1; }
perms=$(stat -c '%a' "$identity" 2>/dev/null || stat -f '%Lp' "$identity")
[[ $perms == 600 ]] || { printf '%s\n' 'identity must be private (0600)' >&2; exit 1; }
[[ -t 0 && -t 1 ]] || { printf '%s\n' 'Refusing: run this in your own interactive terminal, not through an agent/pipe.' >&2; exit 1; }

recipient=$("$age" -y "$identity" 2>/dev/null | head -n 1)
[[ -n $recipient ]] || { printf '%s\n' 'Could not derive the public recipient' >&2; exit 1; }
printf 'Public recipient (for verification, not secret): %s\n' "$recipient"

IFS= read -r -p 'Print the AGE-SECRET-KEY line so you can paste it into your password manager? Type yes: ' confirm
[[ $confirm == yes ]] || { printf '%s\n' 'Cancelled; nothing printed.' >&2; exit 1; }

printf '%s\n' '--- copy exactly the one AGE-SECRET-KEY-... line below into your password manager ---'
cat "$identity"
printf '%s\n' '--- end of secret; clear your terminal scrollback and clipboard after storing ---'
