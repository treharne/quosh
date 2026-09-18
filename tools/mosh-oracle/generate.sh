#!/bin/sh
# Regenerate the golden traces under crates/quosh-predict/tests/fixtures/.
# Requires the oracle from build.sh (run it first) and the Mosh reference.
set -e

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
BUILD=${MOSH_ORACLE_BUILD:-/tmp/quosh-mosh-oracle}
FIX="$root/crates/quosh-predict/tests/fixtures"

if [ ! -x "$BUILD/oracle" ]; then
    echo "oracle not built; run tools/mosh-oracle/build.sh" >&2
    exit 1
fi

mkdir -p "$FIX"
for scenario in "$here"/scenarios/*.txt; do
    name=$(basename "$scenario" .txt)
    "$BUILD/oracle" 80 24 < "$scenario" > "$FIX/$name.golden"
    echo "wrote $FIX/$name.golden"
done
