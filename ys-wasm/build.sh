#!/usr/bin/env bash
set -euo pipefail

# Build WASM package
wasm-pack build --target web --no-default-features

# Patch generated package.json with proper metadata
cd "$(dirname "$0")/pkg"

# Add repository, bugs, homepage, keywords
node -e "
const pkg = require('./package.json');
pkg.repository = {
  type: 'git',
  url: 'https://github.com/DevYatsu/ysc.git'
};
pkg.bugs = 'https://github.com/DevYatsu/ysc/issues';
pkg.homepage = 'https://github.com/DevYatsu/ysc#readme';
pkg.keywords = ['yatsuscript', 'scripting', 'language', 'wasm', 'interpreter', 'playground'];
pkg.description = 'YatsuScript language interpreter for the browser — eval, AST, and bytecode via WASM';
pkg.files = [
  'ys_wasm_bg.wasm',
  'ys_wasm.js',
  'ys_wasm.d.ts',
  'ys_wasm_bg.wasm.d.ts'
];
require('fs').writeFileSync('package.json', JSON.stringify(pkg, null, 2) + '\n');
console.log('✓ package.json patched');
"

# Copy README and enhanced types
cp ../README.md . 2>/dev/null || true
cp ../ys_wasm.d.ts . 2>/dev/null || true

echo "✓ Package ready in pkg/"
