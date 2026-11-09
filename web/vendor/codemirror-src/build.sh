#!/bin/sh
# Bundle CodeMirror 6 into web/vendor/codemirror.js (committed, like three.module.js).
set -e
cd "$(dirname "$0")"
npm install --no-audit --no-fund --silent
npx esbuild entry.js --bundle --format=esm --minify --legal-comments=eof --outfile=../codemirror.js
