#!/bin/sh
# What a pull request to main needs: every line, statement, function, branch outcome and MC/DC condition of every
# source file reached by the tests (the latest measured run's, merged from its shards). Each file short of that is
# said, with what it lacks; `.github/coverage-gaps.sh` has written each one's missing conditions in gaps/.
# (Measurement limitations are not gaps: code the run could not attribute, said by supercov beside each file.)
set -u
npx -y supercov@4.29.18 runs latest files --limit 0 > gate-files.txt || { echo "supercov did not list the run's files" >&2; exit 1; }
short=$(awk '
  /^[^ ]/ { file = $0 }
  /^  uncovered:/ {
    line = $0; sub(/measurement limitations.*/, "", line)
    n = split(line, w, " ")
    for (i = 1; i <= n; i++) if (w[i] ~ /^[0-9]+$/ && w[i] + 0 > 0) { print file ": " line; break }
  }' gate-files.txt)
if [ -n "$short" ]; then
  echo "Not every line, branch and MC/DC condition is covered:" >&2
  echo "$short" >&2
  echo "Each file's missing conditions: the gaps artifact (gaps/<crate>-<file>.txt)." >&2
  exit 1
fi
grep -q 'uncovered:\|covered in this projection' gate-files.txt || { echo "The run lists no files: nothing was measured" >&2; exit 1; }
echo "Every line, statement, function, branch outcome and MC/DC condition is covered."
