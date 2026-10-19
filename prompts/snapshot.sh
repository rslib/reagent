#!/bin/sh
# Environment snapshot: what the run can see, cheaply and safely.
set -u
echo "=== environment snapshot ==="
echo "pwd: $PWD"
echo "--- top-level files and folders ---"
ls -la 2>/dev/null | head -21
echo "--- OS ---"
uname -a 2>/dev/null
echo "--- languages and tools on PATH ---"
for tool in python3 pip pip3 uv node npm cargo rustc go java gcc g++ make apt-get; do
  if command -v "$tool" >/dev/null 2>&1; then
    case "$tool" in
      go) version=$(go version 2>&1 | head -1) ;;
      java) version=$(java -version 2>&1 | head -1) ;;
      *) version=$("$tool" --version 2>&1 | head -1) ;;
    esac
    echo "$tool: $version"
  else
    echo "$tool: (not found)"
  fi
done
echo "--- free memory ---"
if command -v free >/dev/null 2>&1; then
  free -h 2>/dev/null | grep -i mem
else
  echo "free: (not present)"
fi
echo "--- free disk ---"
if command -v df >/dev/null 2>&1; then
  df -h . 2>/dev/null | tail -1
else
  echo "df: (not present)"
fi