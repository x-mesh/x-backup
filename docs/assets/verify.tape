# x-backup — prove a backup is restorable without restoring it.
#
# `verify --deep` decrypts and decodes the archive; `--chain` walks the
# incremental chain back to its base. Both are read-only: nothing is written to
# the database, and nothing in the store is modified.
#
# Record with:  docs/assets/demo-setup.sh && vhs docs/assets/verify.tape
# Record after backup.tape, which produces the chain this reads. Needs `jq`.
# Run from the repository root.

Output docs/assets/demo-verify.gif

Set Shell "bash"
Set FontSize 13
Set Width 1240
Set Height 235
Set Padding 24
Set Theme "Catppuccin Mocha"
Set TypingSpeed 45ms
Set Framerate 24

Hide
Type "export PATH=$PWD/target/release:$PATH" Enter
Type "export XB_CONFIG=/tmp/xb-demo/config.toml" Enter
Type "export XB_AGE_IDENTITY_FILE=/tmp/xb-demo/age.key" Enter
Type "cd /tmp/xb-demo" Enter
Type "PS1='$ '" Enter
Type "clear" Enter
Show

Sleep 1s
Type "ID=$(x-backup list --json | jq -r '.backups[0].id')" Sleep 500ms Enter
Sleep 1500ms

Type "x-backup verify --id $ID --deep --chain" Sleep 500ms Enter
Sleep 6s
