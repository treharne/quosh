#!/bin/sh
# Build the Mosh PredictionEngine differential oracle.
#
# Requires the Mosh source checkout (`docs/07-mosh-reference.md`) and g++.
# The reference tree is not vendored, so this is a local development tool, not
# part of the Quosh build or CI.
#
#   MOSH_REF=/path/to/mosh tools/mosh-oracle/build.sh
#
# Produces $MOSH_ORACLE_BUILD/oracle (default /tmp/quosh-mosh-oracle/oracle).
set -e

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
REF=${MOSH_REF:-"$root/reference/mosh"}
BUILD=${MOSH_ORACLE_BUILD:-/tmp/quosh-mosh-oracle}

if [ ! -f "$REF/src/frontend/terminaloverlay.cc" ]; then
    echo "Mosh reference not found at $REF (set MOSH_REF)" >&2
    exit 1
fi

rm -rf "$BUILD/obj" "$BUILD/src"
mkdir -p "$BUILD/src/frontend" "$BUILD/src/include" "$BUILD/obj"

# PredictionEngine's header pulls in the whole network stack only for types it
# does not use. Strip those two includes and supply `timestamp()` ourselves.
python3 - "$REF/src/frontend/terminaloverlay.h" "$BUILD/src/frontend/terminaloverlay.h" <<'PY'
import sys
src, dst = sys.argv[1], sys.argv[2]
s = open(src).read()
s = s.replace('#include "src/network/network.h"\n', '')
s = s.replace('#include "src/network/transportsender.h"\n', '')
s = s.replace(
    '#include "src/terminal/parser.h"',
    '#include <cstdint>\n'
    'uint64_t timestamp( void );\n'
    'namespace Network { enum { ACK_INTERVAL = 3000 }; }\n'
    '#include "src/terminal/parser.h"',
    1,
)
open(dst, 'w').write(s)
PY
cp "$REF/src/frontend/terminaloverlay.cc" "$BUILD/src/frontend/terminaloverlay.cc"

cat > "$BUILD/src/include/config.h" <<'EOF'
/* Minimal stand-in for the autotools-generated config.h, oracle use only. */
#define HAVE_CLOCK_GETTIME 1
#define HAVE_GETTIMEOFDAY 1
EOF

CXX=${CXX:-g++}
CXXFLAGS="-std=c++17 -O2 -I$BUILD -I$REF -I$REF/src"

for pair in terminaloverlay=frontend terminal=terminal parser=terminal parseraction=terminal \
            parserstate=terminal terminalframebuffer=terminal terminaldispatcher=terminal \
            terminaldisplay=terminal terminalfunctions=terminal terminaluserinput=terminal; do
    name=${pair%%=*}
    dir=${pair##*=}
    $CXX $CXXFLAGS -c "$REF/src/$dir/$name.cc" -o "$BUILD/obj/$name.o"
done

$CXX $CXXFLAGS -c "$here/main.cc" -o "$BUILD/obj/main.o"
$CXX "$BUILD/obj"/*.o -o "$BUILD/oracle"
echo "built $BUILD/oracle"
