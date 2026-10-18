#!/bin/zsh
# Build and run the macOS HAL unit tests. Host-arch only and unsigned -- this
# builds a test executable, not the driver bundle; use ../build.sh for that.
#
# Both tests include ../src/AudioHubDriver.c to reach its static functions,
# so the bridge and the frameworks come along for the link.

set -euo pipefail
cd "${0:a:h}"

BUILD_DIR=build
mkdir -p "$BUILD_DIR"

for TEST in taper_test presentation_test; do
    clang -Wall -Wextra -O2 \
        -framework CoreAudio -framework CoreFoundation -framework Security \
        -lbsm \
        -o "$BUILD_DIR/$TEST" \
        "$TEST.c" ../src/AudioHubBridge.c
done

# An optional argument runs only the tests whose name contains it (a filter
# that matches nothing in one binary fails that binary, so pass the binary name
# as AUDIOHUB_HAL_TEST to run just one of them).
for TEST in taper_test presentation_test; do
    if [[ -n "${AUDIOHUB_HAL_TEST:-}" && "$TEST" != "$AUDIOHUB_HAL_TEST" ]]; then
        continue
    fi
    "$BUILD_DIR/$TEST" "$@"
done
