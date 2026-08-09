#!/bin/zsh
# Build and run the macOS HAL unit tests. Host-arch only and unsigned -- this
# builds a test executable, not the driver bundle; use ../build.sh for that.
#
# taper_test.c includes ../src/AudioHubDriver.c to reach its static functions,
# so the bridge and the frameworks come along for the link.

set -euo pipefail
cd "${0:a:h}"

BUILD_DIR=build
mkdir -p "$BUILD_DIR"

clang -Wall -Wextra -O2 \
    -framework CoreAudio -framework CoreFoundation \
    -lbsm \
    -o "$BUILD_DIR/taper_test" \
    taper_test.c ../src/AudioHubBridge.c

# An optional argument runs only the tests whose name contains it.
"$BUILD_DIR/taper_test" "$@"
