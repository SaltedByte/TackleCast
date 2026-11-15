# Draft retest request — do not post yet

Could you try one run of this diagnostic Linux build? Please keep the original
generic **pipewire** input/output, 1080p60, and all other settings unchanged.
It adds diagnostic logging without adjusting audio buffering or playback.

Download the attached `TackleCast-Linux-x86_64-issue25.tar.gz` and its
`.sha256` file to Downloads, close TackleCast, then paste this into a terminal.
It copies your existing settings into a separate test folder and leaves the
original installation intact. It looks in the AppImages and Downloads folders
shown in your earlier logs, using the most recently saved settings file.

```sh
(
set -eu
cd "$HOME/Downloads"
sha256sum -c TackleCast-Linux-x86_64-issue25.tar.gz.sha256
ORIGINAL_SETTINGS="$(ls -t -- \
  "$HOME/AppImages/TackleCast-Linux-x86_64/tacklecast_settings.json" \
  "$HOME/Downloads/TackleCast-Linux-x86_64/tacklecast_settings.json" \
  2>/dev/null | head -n 1)"
if [ -z "$ORIGINAL_SETTINGS" ]; then
  echo 'Original settings not found; please tell us where your current TackleCast folder is.'
  exit 1
fi
TEST_DIR="$(mktemp -d "$HOME/Downloads/TackleCast-issue25.XXXXXX")"
tar -xzf TackleCast-Linux-x86_64-issue25.tar.gz -C "$TEST_DIR"
cd "$TEST_DIR/TackleCast-Linux-x86_64"
cp -- "$ORIGINAL_SETTINGS" ./tacklecast_settings.json
TACKLECAST_DIAGNOSTICS=1 ./run.sh
LOG="$(ls -t logs/tacklecast_*.log | head -n 1)"
cp -- "$LOG" "$HOME/Downloads/tacklecast-issue25.log"
echo 'Please attach Downloads/tacklecast-issue25.log to your reply.'
)
```

Play normally until the drift appears, or for roughly as long as it usually
takes to appear (30+ minutes if that is typical). Please keep this to a single
run without switching devices or changing settings. After noticing the drift,
close TackleCast normally so the block above can collect the log.

Please send `tacklecast-issue25.log` plus the approximate elapsed time after
launch when you first noticed drift and whether audio was **ahead of** or
**behind** video. If it does not recur, please report how long you played.
