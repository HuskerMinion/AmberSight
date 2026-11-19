#!/bin/sh
# Syntax-check every web-app script the way browsers load it: files that use
# import/export are ES modules and are checked as such (node --check on a
# plain .js file does not catch every module syntax error). Needs node.
set -u
dir=$(mktemp -d); trap 'rm -rf "$dir"' EXIT
bad=0
for f in static/js/*.js static/sw.js; do
  if grep -q -E '^(import|export) ' "$f"; then
    cp "$f" "$dir/$(basename "$f" .js).mjs"
    node --check "$dir/$(basename "$f" .js).mjs" || { echo "FAIL $f"; bad=1; }
  else
    node --check "$f" || { echo "FAIL $f"; bad=1; }
  fi
done
[ $bad = 0 ] && echo "all scripts parse"
# Translated text in an attribute must be quoted (an unquoted attribute lets a translation add attributes).
if grep -nE "[a-zA-Z-]+=' *\+ *(esc\()?t\(" static/js/*.js; then echo "FAIL: unquoted attribute with translated text (above)"; bad=1; fi
# Unit tests of the pure modules.
node scripts/test-camaddr.mjs || bad=1
node scripts/test-display.mjs || bad=1
node scripts/test-fit.mjs || bad=1
exit $bad
