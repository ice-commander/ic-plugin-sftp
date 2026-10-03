#!/bin/sh
# Builds the plugin in release and collects the libraries into bin/.
set -e
cd "$(dirname "$0")"

case "$(uname -s)" in
    Darwin) EXT=dylib ;;
    MINGW*|MSYS*|CYGWIN*) EXT=dll ;;
    *) EXT=so ;;
esac

# Drop libraries from earlier builds: a renamed crate leaves its old
# artefact behind in target/, and it would be collected as if it were current.
rm -f bin/target/release/*."$EXT"

cargo build --release
mkdir -p bin
rm -f "bin/"*."$EXT"

found=0
for library in bin/target/release/*."$EXT"; do
    [ -f "$library" ] || continue
    cp "$library" bin/
    found=$((found + 1))
    printf '%s\n' "  $(basename "$library")"
done

if [ "$found" -eq 0 ]; then
    echo "no plugin libraries were produced" >&2
    exit 1
fi
echo "$found plugin(s) in bin/"
