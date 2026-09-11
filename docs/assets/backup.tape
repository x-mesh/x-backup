# x-backup — full backup, incremental backup, and the resulting chain.
#
# Record with:  docs/assets/demo-setup.sh && vhs docs/assets/backup.tape
# Run from the repository root; see docs/assets/README.md.

Output docs/assets/demo-backup.gif

Set Shell "bash"
Set FontSize 13
Set Width 1240
Set Height 580
Set Padding 24
Set Theme "Catppuccin Mocha"
Set TypingSpeed 45ms
Set Framerate 24

# `Hide`는 프레임 캡처만 멈출 뿐 터미널 버퍼는 그대로 둔다. 그래서 숨긴 채로 명령을
# 치면 `Show` 이후 그 줄이 화면에 남는다. 증분이 잡을 변경(demo-churn.sh)은 풀 백업과
# 증분 백업 사이에 일어나야 하므로, 시작할 때 백그라운드로 예약해 두고 `clear`로
# 예약 흔적만 지운다. disown은 완료 시 bash가 찍는 작업 종료 알림을 막는다.
Hide
Type "export PATH=$PWD/target/release:$PATH" Enter
Type "export XB_CONFIG=/tmp/xb-demo/config.toml" Enter
Type "CHURN=$PWD/docs/assets/demo-churn.sh" Enter
Type "cd /tmp/xb-demo && rm -rf store && mkdir -p store" Enter
Type "{ sleep 7; $CHURN; } >/dev/null 2>&1 & disown" Enter
Type "PS1='$ '" Enter
Type "clear" Enter
Show

Sleep 1s
Type "x-backup backup" Sleep 500ms Enter
Sleep 9s

Type "x-backup backup --type incr" Sleep 500ms Enter
Sleep 6s

Type "x-backup list" Sleep 500ms Enter
Sleep 5s
