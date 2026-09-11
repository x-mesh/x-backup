# x-backup — `status` preflight: can this server be backed up right now?
#
# Record with:  docs/assets/demo-setup.sh && vhs docs/assets/status.tape
# `last backup` / `PITR recoverable` rows read the local store, so record this
# after backup.tape has produced a chain. Run from the repository root.

Output docs/assets/demo-status.gif

Set Shell "bash"
Set FontSize 13
Set Width 1240
Set Height 445
Set Padding 24
Set Theme "Catppuccin Mocha"
Set TypingSpeed 45ms
Set Framerate 24

Hide
Type "export PATH=$PWD/target/release:$PATH" Enter
Type "export XB_CONFIG=/tmp/xb-demo/config.toml" Enter
Type "cd /tmp/xb-demo" Enter
Type "PS1='$ '" Enter
Type "clear" Enter
Show

Sleep 1s
Type "x-backup status" Sleep 500ms Enter
Sleep 7s
